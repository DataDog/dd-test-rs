// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use crate::config::Config;
use crate::read_cache::ReadCache;
use dd_test_core::debug;
use dd_test_core::warn;
use flate2::write::GzEncoder;
use flate2::Compression;
use hyper_util::client::proxy::matcher::Matcher;
use libdd_http_client::{HttpClient, HttpClientError, HttpMethod, HttpRequest};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::HashSet;
use std::io::Write;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;

const TIMEOUT: Duration = Duration::from_secs(15);
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const ATTEMPTS: u32 = 3;
const MAX_ERROR_BODY_CHARS: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intake {
    Api,
    TestCycle,
}

impl Intake {
    fn subdomain(self) -> &'static str {
        match self {
            Intake::Api => "api",
            Intake::TestCycle => "citestcycle-intake",
        }
    }
}

/// A failed request. `status` is set when the server answered with an error.
#[derive(Debug)]
pub struct RequestError {
    pub status: Option<u16>,
    pub message: String,
}

impl RequestError {
    fn other(message: String) -> RequestError {
        RequestError { status: None, message }
    }
}

/// Truncates an error response body and escapes control characters, so a
/// response cannot add lines or terminal sequences to the log.
fn printable_body(body: &str) -> String {
    let mut printable: String = body
        .chars()
        .take(MAX_ERROR_BODY_CHARS)
        .flat_map(char::escape_debug)
        .collect();
    if body.chars().nth(MAX_ERROR_BODY_CHARS).is_some() {
        printable.push_str("...");
    }
    printable
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub struct Transport {
    runtime: Runtime,
    client: Arc<HttpClient>,
    api_key: String,
    site: String,
    intake_url: Option<String>,
    timeout: Duration,
    unreachable: Arc<Unreachable>,
}

/// The hosts that did not answer. A timeout is shared through the read cache
/// with the other agents of the run when a TCP connection to the host cannot be
/// opened either, so they do not wait for the same timeout. A slow host is only
/// skipped by the agent that waited for it. Shared markers describe the direct
/// route, so an agent that reaches the host through a proxy ignores them.
/// `hosts` is always locked before `answered`.
struct Unreachable {
    hosts: Mutex<HashSet<String>>,
    answered: Mutex<HashSet<String>>,
    warned: Mutex<HashSet<String>>,
    cache: Option<ReadCache>,
    proxied: fn(&str) -> bool,
    connects: fn(&str) -> bool,
}

impl Unreachable {
    fn contains(&self, host: &str) -> bool {
        if lock(&self.hosts).contains(host) {
            return true;
        }
        let shared = self
            .cache
            .as_ref()
            .is_some_and(|cache| !(self.proxied)(host) && cache.is_unreachable(host));
        if shared && lock(&self.warned).insert(host.to_string()) {
            warn!("{host} did not answer another test binary of this run, no requests are sent to it");
        }
        shared
    }

    fn mark(&self, host: &str, error: &HttpClientError, url: &str) {
        self.mark_host(host, error, url, false);
    }

    /// A warm-up request that times out after the host answered a real
    /// request does not mark it.
    fn warm_up_timed_out(&self, host: &str, url: &str) {
        self.mark_host(host, &HttpClientError::TimedOut, url, true);
    }

    /// Holds the lock until the marker is shared, so concurrent requests to
    /// the host wait for the probe instead of releasing cache waiters early.
    fn mark_host(&self, host: &str, error: &HttpClientError, url: &str, warm_up: bool) {
        let mut hosts = lock(&self.hosts);
        if warm_up && lock(&self.answered).contains(host) {
            return;
        }
        if !hosts.insert(host.to_string()) {
            return;
        }
        warn!("{url}: {error}. {host} is unreachable, no further requests are sent to it");
        if let (HttpClientError::TimedOut, Some(cache)) = (error, &self.cache) {
            if !(self.proxied)(host) && !(self.connects)(host) {
                cache.mark_unreachable(host);
            }
        }
    }

    /// An answer shows the host is reachable, so a timeout of a request that
    /// was in flight meanwhile no longer stops requests to it, in this agent
    /// or in the others.
    fn answered(&self, host: &str) {
        let mut hosts = lock(&self.hosts);
        lock(&self.answered).insert(host.to_string());
        if hosts.remove(host) {
            if let Some(cache) = &self.cache {
                cache.mark_reachable(host);
            }
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether the HTTP client sends requests to `host`, a base URL, through a
/// proxy. The proxy variables are matched as the client matches them.
fn proxied(host: &str) -> bool {
    static MATCHER: OnceLock<Matcher> = OnceLock::new();
    uses_proxy(MATCHER.get_or_init(Matcher::from_env), host)
}

fn uses_proxy(matcher: &Matcher, host: &str) -> bool {
    host.parse().is_ok_and(|uri| matcher.intercept(&uri).is_some())
}

/// The scheme and the `host[:port]` authority of a URL.
fn split_url(url: &str) -> (&str, &str) {
    let (scheme, rest) = url.split_once("://").unwrap_or(("https", url));
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    (scheme, authority.rsplit_once('@').map_or(authority, |(_, host)| host))
}

/// Whether a direct TCP connection to `host`, a base URL, opens within
/// `PROBE_TIMEOUT`, name resolution included. Every address is tried at once.
fn connects_directly(host: &str) -> bool {
    let (scheme, authority) = split_url(host);
    let has_port = !authority.ends_with(']')
        && authority
            .rsplit_once(':')
            .is_some_and(|(_, port)| port.parse::<u16>().is_ok());
    let address = if has_port {
        authority.to_string()
    } else {
        format!("{authority}:{}", if scheme == "http" { 80 } else { 443 })
    };
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let (sender, results) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for address in address.to_socket_addrs().into_iter().flatten() {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return;
            };
            let sender = sender.clone();
            std::thread::spawn(move || {
                let _ = sender.send(TcpStream::connect_timeout(&address, remaining).is_ok());
            });
        }
    });
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match results.recv_timeout(remaining) {
            Ok(true) => return true,
            Ok(false) => {}
            Err(_) => return false,
        }
    }
    false
}

impl Transport {
    pub fn new(config: &Config, cache: Option<ReadCache>) -> Result<Transport, String> {
        static CRYPTO: Once = Once::new();
        CRYPTO.call_once(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
        });
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| format!("starting the agent runtime: {e}"))?;
        let base_url = config
            .intake_url
            .clone()
            .unwrap_or_else(|| format!("https://api.{}", config.site));
        let client = HttpClient::builder()
            .base_url(base_url)
            .timeout(TIMEOUT)
            .build()
            .map_err(|e| format!("creating the HTTP client: {e}"))?;
        Ok(Transport {
            runtime,
            client: Arc::new(client),
            api_key: config.api_key.clone(),
            site: config.site.clone(),
            intake_url: config.intake_url.clone(),
            timeout: TIMEOUT,
            unreachable: Arc::new(Unreachable {
                hosts: Mutex::new(HashSet::new()),
                answered: Mutex::new(HashSet::new()),
                warned: Mutex::new(HashSet::new()),
                cache,
                proxied,
                connects: connects_directly,
            }),
        })
    }

    /// True once a request to the host serving `intake` failed without an
    /// answer. Later requests would most likely wait for the same timeout.
    pub fn is_unreachable(&self, intake: Intake) -> bool {
        self.is_host_unreachable(&self.host(intake))
    }

    fn is_host_unreachable(&self, host: &str) -> bool {
        self.unreachable.contains(host)
    }

    fn mark_unreachable(&self, host: &str, error: &HttpClientError, url: &str) {
        self.unreachable.mark(host, error, url);
    }

    /// The scheme and host requests for `intake` go to.
    fn host(&self, intake: Intake) -> String {
        self.intake_url
            .clone()
            .unwrap_or_else(|| format!("https://{}.{}", intake.subdomain(), self.site))
    }

    /// Opens the connection to the host serving `intake` in the background, so
    /// the first request to it does not wait for TCP and TLS setup. A timeout
    /// marks the host unreachable unless it answered a request meanwhile.
    pub fn warm_up(&self, intake: Intake) {
        let host = self.host(intake);
        if self.is_host_unreachable(&host) {
            return;
        }
        let client = Arc::clone(&self.client);
        let unreachable = Arc::clone(&self.unreachable);
        let url = format!("{host}/");
        let request = HttpRequest::new(HttpMethod::Head, url.clone())
            .with_headers([("dd-api-key".to_string(), self.api_key.clone())])
            .with_timeout(self.timeout);
        self.runtime.spawn(async move {
            let started = Instant::now();
            let result = client.send(request).await;
            debug!("opened the connection to {url} in {:?}", started.elapsed());
            if let Err(HttpClientError::TimedOut) = result {
                let _ = tokio::task::spawn_blocking(move || unreachable.warm_up_timed_out(&host, &url)).await;
            }
        });
    }

    /// POSTs `body`, gzip-compressed when `gzip` is set, retrying connection
    /// errors, 429 and 5xx responses. Timeouts are not retried.
    pub fn post(
        &self,
        intake: Intake,
        path: &str,
        content_type: &str,
        body: &[u8],
        gzip: bool,
    ) -> Result<Vec<u8>, RequestError> {
        let host = self.host(intake);
        let url = format!("{host}{path}");
        let mut headers = vec![
            ("dd-api-key".to_string(), self.api_key.clone()),
            ("Content-Type".to_string(), content_type.to_string()),
        ];
        let body = if gzip {
            headers.push(("Content-Encoding".to_string(), "gzip".to_string()));
            let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
            encoder
                .write_all(body)
                .and_then(|_| encoder.finish())
                .map_err(|e| RequestError::other(e.to_string()))?
        } else {
            body.to_vec()
        };
        let mut last_error = RequestError::other(String::new());
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(100 << attempt));
            }
            if self.is_host_unreachable(&host) {
                return Err(RequestError::other(format!("{url}: skipped, {host} is unreachable")));
            }
            let request = HttpRequest::new(HttpMethod::Post, url.clone())
                .with_headers(headers.iter().cloned())
                .with_body(body.clone())
                .with_timeout(self.timeout);
            let started = Instant::now();
            let result = self.runtime.block_on(self.client.send(request));
            let elapsed = started.elapsed();
            let retry = match result {
                Ok(response) => {
                    self.unreachable.answered(&host);
                    debug!("POST {url}: {} bytes in {elapsed:?}", response.body().len());
                    return Ok(response.body().to_vec());
                }
                Err(HttpClientError::RequestFailed { status, body }) => {
                    self.unreachable.answered(&host);
                    last_error = RequestError {
                        status: Some(status),
                        message: format!("{url} returned {status}: {}", printable_body(&body)),
                    };
                    status >= 500 || status == 429
                }
                Err(error) => {
                    last_error = RequestError::other(format!("{url}: {error}"));
                    let unreachable = is_network_error(&error)
                        && (attempt + 1 == ATTEMPTS || matches!(error, HttpClientError::TimedOut));
                    if unreachable {
                        self.mark_unreachable(&host, &error, &url);
                    }
                    !unreachable
                }
            };
            debug!("request failed after {elapsed:?}: {last_error}");
            if !retry {
                break;
            }
        }
        Err(last_error)
    }

    /// POSTs JSON and returns the decoded response with its size in bytes.
    pub fn post_json<B: Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<(R, usize), RequestError> {
        let body = serde_json::to_vec(body).map_err(|e| RequestError::other(e.to_string()))?;
        let response = self.post(Intake::Api, path, "application/json", &body, false)?;
        let decoded = serde_json::from_slice(&response)
            .map_err(|e| RequestError::other(format!("invalid response from {path}: {e}")))?;
        Ok((decoded, response.len()))
    }
}

fn is_network_error(error: &HttpClientError) -> bool {
    matches!(
        error,
        HttpClientError::ConnectionFailed(_) | HttpClientError::TimedOut | HttpClientError::IoError(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Instant;

    fn transport(intake_url: Option<String>) -> Transport {
        let config = Config::from_lookup(|key| match key {
            "DD_API_KEY" => Some("k".into()),
            "DD_SITE" => Some("datadoghq.eu".into()),
            "DD_CIVISIBILITY_AGENTLESS_URL" => intake_url.clone(),
            _ => None,
        });
        let mut transport = Transport::new(&config, None).unwrap();
        transport.timeout = Duration::from_millis(200);
        transport
    }

    fn agentless(url: String) -> Transport {
        transport(Some(url))
    }

    /// Answers every connection with `response`, or never answers when it is `None`.
    fn server(response: Option<&'static str>) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let connections = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&connections);
        std::thread::spawn(move || {
            let mut open = Vec::new();
            for mut stream in listener.incoming().flatten() {
                counter.fetch_add(1, Ordering::SeqCst);
                match response {
                    Some(response) => {
                        let _ = stream.read(&mut [0; 4096]);
                        let _ = stream.write_all(response.as_bytes());
                    }
                    None => open.push(stream),
                }
            }
        });
        (url, connections)
    }

    fn post(transport: &Transport) -> Result<Vec<u8>, RequestError> {
        transport.post(
            Intake::TestCycle,
            "/api/v2/citestcycle",
            "application/msgpack",
            b"x",
            true,
        )
    }

    #[test]
    fn a_timeout_is_not_retried_and_marks_the_endpoint_unreachable() {
        let (url, connections) = server(None);
        let transport = agentless(url);
        let started = Instant::now();
        assert!(post(&transport).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(connections.load(Ordering::SeqCst), 1);
        assert!(transport.is_unreachable(Intake::TestCycle));
    }

    #[test]
    fn refused_connections_mark_the_endpoint_unreachable() {
        let url = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", listener.local_addr().unwrap())
        };
        let transport = agentless(url);
        assert!(post(&transport).is_err());
        assert!(transport.is_unreachable(Intake::TestCycle));
    }

    #[test]
    fn http_errors_keep_the_endpoint_reachable() {
        let (url, connections) = server(Some(
            "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        ));
        let transport = agentless(url);
        assert_eq!(post(&transport).unwrap_err().status, Some(400));
        assert_eq!(connections.load(Ordering::SeqCst), 1);
        assert!(!transport.is_unreachable(Intake::TestCycle));

        let (url, connections) = server(Some(
            "HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        ));
        let transport = agentless(url);
        assert_eq!(post(&transport).unwrap_err().status, Some(503));
        assert_eq!(connections.load(Ordering::SeqCst), ATTEMPTS as usize);
        assert!(!transport.is_unreachable(Intake::TestCycle));
    }

    #[test]
    fn error_bodies_are_escaped_and_truncated() {
        assert_eq!(printable_body("bad\nrequest\u{1b}[2J"), "bad\\nrequest\\u{1b}[2J");
        let long = "x".repeat(MAX_ERROR_BODY_CHARS + 1);
        assert_eq!(
            printable_body(&long),
            format!("{}...", "x".repeat(MAX_ERROR_BODY_CHARS))
        );
        assert_eq!(printable_body("not found"), "not found");
    }

    /// Answers every request with 200, keeping connections open. Counts connections and requests.
    fn keep_alive_server() -> (String, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (connections, requests) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (connection_count, request_count) = (Arc::clone(&connections), Arc::clone(&requests));
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                connection_count.fetch_add(1, Ordering::SeqCst);
                let requests = Arc::clone(&request_count);
                std::thread::spawn(move || {
                    let mut reader = std::io::BufReader::new(stream);
                    loop {
                        let mut length = 0;
                        loop {
                            let mut line = String::new();
                            if std::io::BufRead::read_line(&mut reader, &mut line).unwrap_or(0) == 0 {
                                return;
                            }
                            if line.trim().is_empty() {
                                break;
                            }
                            if let Some((name, value)) = line.split_once(':') {
                                if name.eq_ignore_ascii_case("content-length") {
                                    length = value.trim().parse().unwrap();
                                }
                            }
                        }
                        let _ = reader.read_exact(&mut vec![0; length]);
                        requests.fetch_add(1, Ordering::SeqCst);
                        let _ = reader
                            .get_mut()
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
                    }
                });
            }
        });
        (url, connections, requests)
    }

    #[test]
    fn requests_reuse_the_warmed_up_connection() {
        let (url, connections, requests) = keep_alive_server();
        let transport = agentless(url);
        transport.warm_up(Intake::TestCycle);
        let deadline = Instant::now() + Duration::from_secs(5);
        while requests.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline, "the warm-up request arrives");
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(100));
        post(&transport).unwrap();
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        assert_eq!(connections.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn sends_each_intake_to_its_host() {
        let transport = transport(None);
        assert_eq!(transport.host(Intake::Api), "https://api.datadoghq.eu");
        assert_eq!(
            transport.host(Intake::TestCycle),
            "https://citestcycle-intake.datadoghq.eu"
        );
    }

    #[test]
    fn an_unreachable_api_host_does_not_stop_results() {
        let transport = transport(None);
        transport.mark_unreachable(&transport.host(Intake::Api), &HttpClientError::TimedOut, "u");
        assert!(transport.is_unreachable(Intake::Api));
        assert!(!transport.is_unreachable(Intake::TestCycle));
    }

    fn shared(url: &str, dir: &std::path::Path) -> Transport {
        shared_with_probe(url, dir, |_| false)
    }

    fn shared_with_probe(url: &str, dir: &std::path::Path, connects: fn(&str) -> bool) -> Transport {
        let config = Config::from_lookup(|key| match key {
            "DD_API_KEY" => Some("k".into()),
            "DD_CIVISIBILITY_AGENTLESS_URL" => Some(url.to_string()),
            _ => None,
        });
        let scope = crate::read_cache::Scope::select(&crate::ci::Tags::new(), None, Some(1)).unwrap();
        let cache = ReadCache::new(dir.to_path_buf(), &scope, &config, "0");
        let mut transport = Transport::new(&config, Some(cache)).unwrap();
        transport.timeout = Duration::from_millis(200);
        let unreachable = Arc::get_mut(&mut transport.unreachable).unwrap();
        unreachable.proxied = |_| false;
        unreachable.connects = connects;
        transport
    }

    fn cache_dir(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("dd-test-transport-{name}-{:x}", crate::ids::new_id()))
    }

    #[test]
    fn timeouts_are_shared_with_the_other_agents() {
        let (url, connections) = server(None);
        let dir = cache_dir("timeout");
        assert!(post(&shared(&url, &dir)).is_err());
        let other = shared(&url, &dir);
        assert!(other.is_unreachable(Intake::TestCycle));
        assert!(post(&other).unwrap_err().message.contains("skipped"));
        assert_eq!(connections.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refused_connections_are_not_shared() {
        let url = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", listener.local_addr().unwrap())
        };
        let dir = cache_dir("refused");
        let first = shared(&url, &dir);
        assert!(post(&first).is_err());
        assert!(first.is_unreachable(Intake::TestCycle));
        assert!(!shared(&url, &dir).is_unreachable(Intake::TestCycle));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_warm_up_timeout_marks_the_host_unreachable() {
        let (url, _) = server(None);
        let dir = cache_dir("warm-up");
        let transport = shared(&url, &dir);
        transport.warm_up(Intake::TestCycle);
        let other = shared(&url, &dir);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !other.is_unreachable(Intake::TestCycle) {
            assert!(Instant::now() < deadline, "the warm-up times out");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(transport.is_unreachable(Intake::TestCycle));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn slow_responses_are_not_shared() {
        let (url, _) = server(None);
        let dir = cache_dir("slow");
        let first = shared_with_probe(&url, &dir, connects_directly);
        assert!(post(&first).is_err());
        assert!(first.is_unreachable(Intake::TestCycle));
        assert!(!shared(&url, &dir).is_unreachable(Intake::TestCycle));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn markers_from_other_agents_expire() {
        let (url, _) = server(None);
        let dir = cache_dir("expire");
        assert!(post(&shared(&url, &dir)).is_err());
        let other = shared(&url, &dir);
        assert!(other.is_unreachable(Intake::TestCycle));
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(!other.is_unreachable(Intake::TestCycle));
    }

    #[test]
    fn a_warm_up_timeout_after_an_answer_is_ignored() {
        let (url, _) = server(None);
        let dir = cache_dir("answered");
        let transport = shared(&url, &dir);
        transport.unreachable.answered(&url);
        transport.unreachable.warm_up_timed_out(&url, &url);
        assert!(!transport.is_unreachable(Intake::TestCycle));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detects_proxies_for_the_scheme() {
        let https = |no: &str| Matcher::builder().https("http://proxy").no(no).build();
        assert!(uses_proxy(&https(""), "https://api.datadoghq.com"));
        assert!(!uses_proxy(&https(""), "http://api.datadoghq.com"));
        let all = Matcher::builder().all("http://proxy").build();
        assert!(uses_proxy(&all, "http://user@127.0.0.1:8126/proxy?x=1"));
        assert!(!uses_proxy(&Matcher::builder().build(), "https://api.datadoghq.com"));
        for no in ["*", "api.datadoghq.com", ".datadoghq.com", "localhost, DATADOGHQ.com"] {
            assert!(!uses_proxy(&https(no), "https://api.datadoghq.com"), "{no}");
        }
        assert!(uses_proxy(&https("example.com,hq.com"), "https://api.datadoghq.com"));
        assert!(!uses_proxy(&https("10.0.0.0/8"), "https://10.1.2.3:8126"));
        assert!(uses_proxy(&https("10.0.0.0/8"), "https://11.1.2.3:8126"));
    }

    #[test]
    fn agents_using_a_proxy_ignore_markers_of_the_direct_route() {
        let (url, _) = server(None);
        let dir = cache_dir("proxied");
        assert!(post(&shared(&url, &dir)).is_err());
        let mut proxied = shared(&url, &dir);
        Arc::get_mut(&mut proxied.unreachable).unwrap().proxied = |_| true;
        assert!(!proxied.is_unreachable(Intake::TestCycle));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_answer_after_a_warm_up_timeout_keeps_the_host_reachable() {
        let (url, _) = server(None);
        let dir = cache_dir("reconcile");
        let transport = shared(&url, &dir);
        transport.unreachable.warm_up_timed_out(&url, &url);
        let other = shared(&url, &dir);
        assert!(transport.is_unreachable(Intake::TestCycle));
        assert!(other.is_unreachable(Intake::TestCycle));
        transport.unreachable.answered(&url);
        assert!(!transport.is_unreachable(Intake::TestCycle));
        assert!(!other.is_unreachable(Intake::TestCycle));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn splits_intake_urls() {
        assert_eq!(
            split_url("http://user@127.0.0.1:8126/proxy?x=1"),
            ("http", "127.0.0.1:8126")
        );
        assert_eq!(split_url("https://api.datadoghq.com"), ("https", "api.datadoghq.com"));
    }

    #[test]
    fn probes_tcp_connections() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        assert!(connects_directly(&format!("http://{address}")));
        assert!(connects_directly(&format!("http://user@{address}/proxy?x=1")));
        assert!(!connects_directly("http://unresolvable.invalid"));
    }

    #[test]
    fn requests_to_an_unreachable_host_are_not_sent() {
        let (url, connections) = server(None);
        let transport = agentless(url.clone());
        transport.mark_unreachable(&url, &HttpClientError::TimedOut, "u");
        assert!(post(&transport).unwrap_err().message.contains("skipped"));
        assert_eq!(connections.load(Ordering::SeqCst), 0);
    }
}
