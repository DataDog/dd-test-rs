// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Shares backend responses and local git tags between the agents of one
//! invocation, such as the test binaries of one `cargo test`, through files in
//! a cache directory.
//!
//! It follows dd-trace-go's read cache. The first agent that needs a response
//! takes a lock file, makes the request and stores the response. Agents that
//! find the lock wait for the response. Any I/O error, or a wait that runs out,
//! makes the agent request for itself, so the cache never fails a run.

use crate::ci::Tags;
use crate::config::Config;
use crate::ids::{new_id, now_nanos};
use dd_test_core::debug;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const SCHEMA: &str = "dd-test-rs-read-cache-v1";
const INITIAL_BACKOFF: Duration = Duration::from_millis(5);
const MAX_BACKOFF: Duration = Duration::from_millis(20);
/// Longer than a paginated request takes, so a slow owner still serves its waiters.
const WAIT_TIMEOUT: Duration = Duration::from_secs(120);
/// Also the longest TTL, so older files are never fresh.
const STALE_AFTER: Duration = Duration::from_secs(3600);
/// Short, so one timeout does not stop reporting for long.
const UNREACHABLE_TTL: Duration = Duration::from_secs(300);
const CLEANUP_MAX_ENTRIES: usize = 32;
const CLEANUP_MAX_DURATION: Duration = Duration::from_millis(5);

/// Who shares cached responses, and for how long.
#[derive(Clone, Debug, PartialEq)]
pub struct Scope {
    pub kind: &'static str,
    pub ttl: Duration,
    pub identity: Vec<(&'static str, String)>,
}

impl Scope {
    /// Selects the scope as dd-trace-go does, with nextest runs added. Without
    /// a CI job, a nextest run or a parent process, there is nothing to share.
    pub fn select(tags: &Tags, nextest_run_id: Option<&str>, parent_pid: Option<u32>) -> Option<Scope> {
        let tag = |key: &str| tags.get(key).filter(|v| !v.is_empty()).cloned();
        let scope = |kind, minutes: u64, identity| Scope {
            kind,
            ttl: Duration::from_secs(minutes * 60),
            identity,
        };
        let pipeline = tag("ci.pipeline.id")
            .map(|v| ("ci.pipeline.id", v))
            .or_else(|| tag("ci.pipeline.number").map(|v| ("ci.pipeline.number", v)));
        if let Some(provider) = tag("ci.provider.name") {
            let mut identity = vec![("ci.provider.name", provider)];
            identity.extend(pipeline.clone());
            if let Some(job) = tag("ci.job.id") {
                identity.push(("ci.job.id", job));
                return Some(scope("ci_strong", 60, identity));
            }
            if pipeline.is_some() {
                let names: Vec<_> = ["ci.job.name", "ci.stage.name", "ci.node.name"]
                    .into_iter()
                    .filter_map(|key| tag(key).map(|v| (key, v)))
                    .collect();
                if names.is_empty() {
                    return Some(scope("ci_weak", 5, identity));
                }
                identity.extend(names);
                return Some(scope("ci_medium", 15, identity));
            }
        }
        if let Some(run) = nextest_run_id.filter(|v| !v.is_empty()) {
            return Some(scope("nextest", 60, vec![("nextest.run_id", run.to_string())]));
        }
        parent_pid.map(|pid| scope("local", 1, vec![("parent_pid", pid.to_string())]))
    }
}

#[derive(Clone)]
pub struct ReadCache {
    dir: PathBuf,
    ttl: Duration,
    base: Value,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    key: String,
    created_at: i64,
    ttl_seconds: u64,
    bytes: usize,
    response: Value,
}

#[derive(Serialize, Deserialize, PartialEq)]
struct Lock {
    pid: u32,
    created_at: i64,
    nonce: u64,
}

enum Acquire {
    Owner(Lock),
    Held,
    Bypass,
}

impl ReadCache {
    pub fn new(dir: PathBuf, scope: &Scope, config: &Config, library_version: &str) -> ReadCache {
        let intake = config.intake_url.clone().unwrap_or_else(|| config.site.clone());
        ReadCache {
            dir,
            ttl: scope.ttl,
            base: json!({
                "schema": SCHEMA,
                "library_version": library_version,
                "intake": intake,
                "api_key": sha256(config.api_key.as_bytes()),
                "scope": scope.kind,
                "identity": scope.identity,
            }),
        }
    }

    /// Returns the response to `request` on `endpoint`, with its size in bytes,
    /// from the cache or from `live`. Only successful live responses are stored.
    pub fn get_or_fetch<E>(
        &self,
        endpoint: &str,
        request: &Value,
        live: impl FnOnce() -> Result<(Value, usize), E>,
    ) -> Result<(Value, usize), E> {
        let key = self.key(endpoint, request);
        if let Err(error) = create_dir(&self.dir) {
            debug!("read_cache: bypass [endpoint:{endpoint} error:{error}]");
            return live();
        }
        if let Some(hit) = self.read(&key, self.ttl) {
            debug!("read_cache: hit [endpoint:{endpoint} key:{key}]");
            return Ok(hit);
        }
        let lock_path = self.dir.join(format!("{key}.lock"));
        let started = Instant::now();
        let mut backoff = INITIAL_BACKOFF;
        let mut waiting = false;
        loop {
            match acquire(&lock_path, waiting) {
                Acquire::Owner(lock) => {
                    let result = match self.read(&key, self.ttl) {
                        Some(hit) => {
                            debug!("read_cache: hit [endpoint:{endpoint} key:{key}]");
                            Ok(hit)
                        }
                        None => {
                            let result = live();
                            if let Ok((response, bytes)) = &result {
                                self.write(&key, self.ttl, response, *bytes);
                            }
                            debug!(
                                "read_cache: miss [endpoint:{endpoint} key:{key} stored:{}]",
                                result.is_ok()
                            );
                            result
                        }
                    };
                    release(&lock_path, &lock);
                    self.cleanup();
                    return result;
                }
                Acquire::Bypass => {
                    debug!("read_cache: lock bypass [endpoint:{endpoint} key:{key}]");
                    return live();
                }
                Acquire::Held => waiting = true,
            }
            if started.elapsed() >= WAIT_TIMEOUT {
                debug!(
                    "read_cache: waiter fallback [endpoint:{endpoint} key:{key} waited:{:?}]",
                    started.elapsed()
                );
                return live();
            }
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(MAX_BACKOFF);
            if let Some(hit) = self.read(&key, self.ttl) {
                debug!(
                    "read_cache: hit after wait [endpoint:{endpoint} key:{key} waited:{:?}]",
                    started.elapsed()
                );
                return Ok(hit);
            }
            if !lock_path.exists() {
                if let Some(hit) = self.read(&key, self.ttl) {
                    return Ok(hit);
                }
                debug!(
                    "read_cache: waiter fallback [endpoint:{endpoint} key:{key} waited:{:?}]",
                    started.elapsed()
                );
                return live();
            }
        }
    }

    /// Hashes the scope, the endpoint and the request without its random `data.id`.
    fn key(&self, endpoint: &str, request: &Value) -> String {
        let mut request = request.clone();
        if let Some(data) = request.get_mut("data").and_then(Value::as_object_mut) {
            data.remove("id");
        }
        let text = json!({ "base": self.base, "endpoint": endpoint, "request": request }).to_string();
        sha256(text.as_bytes())
    }

    /// Tells the other agents of the scope that `host` did not answer.
    pub fn mark_unreachable(&self, host: &str) {
        if create_dir(&self.dir).is_ok() {
            self.write(
                &self.key("unreachable", &json!(host)),
                self.unreachable_ttl(),
                &Value::Null,
                0,
            );
        }
    }

    /// Withdraws the marker of `host` once it answered.
    pub fn mark_reachable(&self, host: &str) {
        let key = self.key("unreachable", &json!(host));
        let _ = fs::remove_file(self.dir.join(format!("{key}.json")));
    }

    /// True when an agent of the scope recently found `host` unreachable.
    pub fn is_unreachable(&self, host: &str) -> bool {
        self.read(&self.key("unreachable", &json!(host)), self.unreachable_ttl())
            .is_some()
    }

    fn unreachable_ttl(&self) -> Duration {
        self.ttl.min(UNREACHABLE_TTL)
    }

    fn read(&self, key: &str, ttl: Duration) -> Option<(Value, usize)> {
        let text = fs::read(self.dir.join(format!("{key}.json"))).ok()?;
        let entry: Entry = serde_json::from_slice(&text).ok()?;
        let age = now_nanos() - entry.created_at;
        let fresh =
            entry.key == key && entry.ttl_seconds == ttl.as_secs() && age >= 0 && (age as u128) <= ttl.as_nanos();
        fresh.then_some((entry.response, entry.bytes))
    }

    fn write(&self, key: &str, ttl: Duration, response: &Value, bytes: usize) {
        let entry = Entry {
            key: key.to_string(),
            created_at: now_nanos(),
            ttl_seconds: ttl.as_secs(),
            bytes,
            response: response.clone(),
        };
        let path = self.dir.join(format!("{key}.json"));
        let temp = self
            .dir
            .join(format!("{key}.tmp.{}.{:x}", std::process::id(), new_id()));
        let result = serde_json::to_vec(&entry)
            .map_err(|e| e.to_string())
            .and_then(|bytes| write_new(&temp, &bytes).map_err(|e| e.to_string()))
            .and_then(|_| fs::rename(&temp, &path).map_err(|e| e.to_string()));
        if let Err(error) = result {
            let _ = fs::remove_file(&temp);
            debug!("read_cache: write failed [key:{key} error:{error}]");
        }
    }

    /// Removes cache files older than any TTL, within a small budget.
    fn cleanup(&self) {
        self.cleanup_within(CLEANUP_MAX_DURATION);
    }

    fn cleanup_within(&self, max_duration: Duration) {
        let started = Instant::now();
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        let mut checked = 0;
        for entry in entries.flatten() {
            if checked == CLEANUP_MAX_ENTRIES || started.elapsed() > max_duration {
                break;
            }
            if !is_cache_file(&entry.file_name().to_string_lossy()) {
                continue;
            }
            checked += 1;
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age > STALE_AFTER);
            if old {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

/// The directory for cached responses: `DD_TEST_READ_CACHE_DIR`, or one under
/// the user's cache directory, or the temporary directory.
pub fn default_dir() -> PathBuf {
    let var = |key| std::env::var_os(key).map(PathBuf::from).filter(|p| p.is_absolute());
    if let Some(dir) = var("DD_TEST_READ_CACHE_DIR") {
        return dir;
    }
    let base = if cfg!(target_os = "macos") {
        var("HOME").map(|home| home.join("Library/Caches"))
    } else if cfg!(windows) {
        var("LOCALAPPDATA")
    } else {
        var("XDG_CACHE_HOME").or_else(|| var("HOME").map(|home| home.join(".cache")))
    };
    base.unwrap_or_else(std::env::temp_dir)
        .join("dd-test-rs")
        .join("read-cache")
}

/// Matches the names the cache cleans up: `<key>.json`, `<key>.lock` and
/// `<key>.tmp.<pid>.<nonce>`. Takeover guards are never removed, since an agent
/// may hold one.
fn is_cache_file(name: &str) -> bool {
    let Some((key, suffix)) = name.split_once('.') else {
        return false;
    };
    let hex = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit());
    let temp = |s: &str| {
        s.strip_prefix("tmp.")
            .and_then(|rest| rest.split_once('.'))
            .is_some_and(|(pid, nonce)| !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()) && hex(nonce))
    };
    key.len() == 64 && hex(key) && (suffix == "json" || suffix == "lock" || temp(suffix))
}

/// Takes the lock, or takes over a stale one. A waiter only takes over stale
/// locks: when the owner releases the lock, waiters request for themselves.
fn acquire(path: &Path, waiting: bool) -> Acquire {
    let lock = Lock {
        pid: std::process::id(),
        created_at: now_nanos(),
        nonce: new_id(),
    };
    if !waiting {
        match try_lock(path, &lock) {
            Ok(true) => return Acquire::Owner(lock),
            Ok(false) => {}
            Err(_) => return Acquire::Bypass,
        }
    }
    match fs::read(path) {
        Ok(current) if is_stale(&current) => take_over(path, &current, lock),
        // A lock this agent cannot read would otherwise hold it back for the
        // full timeout on every miss.
        Err(error) if error.kind() != ErrorKind::NotFound => Acquire::Bypass,
        _ => Acquire::Held,
    }
}

/// Replaces a stale lock in place while holding the takeover guard, an OS
/// lock on `<key>.takeover`, so only one agent replaces it and the lock never
/// disappears for the waiters. The OS releases the guard when its holder dies.
fn take_over(path: &Path, stale: &[u8], lock: Lock) -> Acquire {
    let _guard = match takeover_guard(&path.with_extension("takeover")) {
        Ok(Some(guard)) => guard,
        Ok(None) => return Acquire::Held,
        Err(_) => return Acquire::Bypass,
    };
    if fs::read(path).ok().as_deref() != Some(stale) {
        return Acquire::Held;
    }
    debug!("read_cache: owner gone, taking over [lock:{}]", path.display());
    replace(path, &lock).map_or(Acquire::Bypass, |()| Acquire::Owner(lock))
}

/// The open guard file, locked until it is dropped, or `None` when another
/// agent holds it.
#[cfg(unix)]
fn takeover_guard(path: &Path) -> std::io::Result<Option<File>> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(file));
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == ErrorKind::WouldBlock {
        Ok(None)
    } else {
        Err(error)
    }
}

#[cfg(not(unix))]
fn takeover_guard(_path: &Path) -> std::io::Result<Option<File>> {
    Err(ErrorKind::Unsupported.into())
}

fn replace(path: &Path, lock: &Lock) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(lock).map_err(std::io::Error::other)?;
    let temp = temp_path(path);
    write_new(&temp, &bytes)?;
    fs::rename(&temp, path).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })
}

/// Publishes a complete lock file with a link that fails when the lock exists.
fn try_lock(path: &Path, lock: &Lock) -> std::io::Result<bool> {
    let bytes = serde_json::to_vec(lock).map_err(std::io::Error::other)?;
    let temp = temp_path(path);
    write_new(&temp, &bytes)?;
    let linked = fs::hard_link(&temp, path);
    let _ = fs::remove_file(&temp);
    match linked {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error),
    }
}

/// A unique `<key>.tmp.<pid>.<nonce>` path next to a `<key>.lock` path.
fn temp_path(lock_path: &Path) -> PathBuf {
    lock_path.with_extension(format!("tmp.{}.{:x}", std::process::id(), new_id()))
}

/// A lock is stale when it is unreadable, its owner no longer runs, or it is
/// older than any request. Locks are published complete, so an unreadable lock
/// has no owner.
fn is_stale(content: &[u8]) -> bool {
    match serde_json::from_slice::<Lock>(content) {
        Ok(lock) => !process_alive(lock.pid) || now_nanos() - lock.created_at > STALE_AFTER.as_nanos() as i64,
        Err(_) => true,
    }
}

fn release(path: &Path, lock: &Lock) {
    let current = fs::read(path)
        .ok()
        .and_then(|text| serde_json::from_slice::<Lock>(&text).ok());
    if current.as_ref() == Some(lock) {
        let _ = fs::remove_file(path);
    }
}

fn write_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file: File = options.open(path)?;
    file.write_all(bytes).inspect_err(|_| {
        let _ = fs::remove_file(path);
    })
}

fn create_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    unsafe { libc::kill(pid, 0) == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) }
}

#[cfg(not(unix))]
fn process_alive(_pid: u32) -> bool {
    true
}

fn sha256(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dd-test-read-cache-{name}-{:x}", new_id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn cache(dir: &Path) -> ReadCache {
        let config = Config::from_lookup(|key| (key == "DD_API_KEY").then(|| "key".to_string()));
        let scope = Scope::select(&Tags::new(), None, Some(1)).unwrap();
        ReadCache::new(dir.to_path_buf(), &scope, &config, "0.1.0")
    }

    fn request(id: &str) -> Value {
        json!({"data": {"id": id, "type": "t", "attributes": {"sha": "abc"}}})
    }

    fn fetch(cache: &ReadCache, request: &Value, calls: &AtomicUsize) -> Result<(Value, usize), String> {
        cache.get_or_fetch("/p", request, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok((json!({"n": 1}), 7))
        })
    }

    fn tags(pairs: &[(&str, &str)]) -> Tags {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn selects_the_scope_like_dd_trace_go() {
        let kind = |tags: Tags, nextest, pid| Scope::select(&tags, nextest, pid).map(|s| (s.kind, s.ttl.as_secs()));
        let ci = [("ci.provider.name", "github"), ("ci.pipeline.id", "1")];
        assert_eq!(
            kind(tags(&[ci[0], ci[1], ("ci.job.id", "2")]), Some("run"), Some(1)),
            Some(("ci_strong", 3600))
        );
        assert_eq!(
            kind(tags(&[ci[0], ("ci.job.id", "2")]), None, None),
            Some(("ci_strong", 3600))
        );
        assert_eq!(
            kind(tags(&[ci[0], ci[1], ("ci.job.name", "test")]), None, None),
            Some(("ci_medium", 900))
        );
        assert_eq!(kind(tags(&ci), None, None), Some(("ci_weak", 300)));
        assert_eq!(kind(tags(&[ci[0]]), Some("run"), Some(1)), Some(("nextest", 3600)));
        assert_eq!(kind(Tags::new(), None, Some(1)), Some(("local", 60)));
        assert_eq!(kind(Tags::new(), None, None), None);
    }

    #[test]
    fn requests_once_and_ignores_the_request_id() {
        let dir = temp_dir("once");
        let calls = AtomicUsize::new(0);
        assert_eq!(fetch(&cache(&dir), &request("a"), &calls), Ok((json!({"n": 1}), 7)));
        assert_eq!(fetch(&cache(&dir), &request("b"), &calls), Ok((json!({"n": 1}), 7)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let other = json!({"data": {"id": "c", "attributes": {"sha": "def"}}});
        fetch(&cache(&dir), &other, &calls).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2, "another request has its own entry");
        assert!(!dir.join(format!("{}.lock", cache(&dir).key("/p", &other))).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn does_not_share_between_scopes() {
        let dir = temp_dir("scopes");
        let calls = AtomicUsize::new(0);
        let config = Config::from_lookup(|key| (key == "DD_API_KEY").then(|| "key".to_string()));
        for pid in [1, 2] {
            let scope = Scope::select(&Tags::new(), None, Some(pid)).unwrap();
            let cache = ReadCache::new(dir.clone(), &scope, &config, "0.1.0");
            fetch(&cache, &request("a"), &calls).unwrap();
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn does_not_store_failures() {
        let dir = temp_dir("failures");
        let cache = cache(&dir);
        let failed: Result<(Value, usize), String> = cache.get_or_fetch("/p", &request("a"), || Err("500".into()));
        assert!(failed.is_err());
        let calls = AtomicUsize::new(0);
        fetch(&cache, &request("a"), &calls).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn expired_entries_are_requested_again() {
        let dir = temp_dir("expired");
        let cache = cache(&dir);
        let calls = AtomicUsize::new(0);
        fetch(&cache, &request("a"), &calls).unwrap();
        let path = dir.join(format!("{}.json", cache.key("/p", &request("a"))));
        let mut entry: Entry = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        entry.created_at -= 61 * 1_000_000_000;
        fs::write(&path, serde_json::to_vec(&entry).unwrap()).unwrap();
        fetch(&cache, &request("a"), &calls).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn waiters_share_the_owners_response() {
        let dir = temp_dir("waiters");
        let calls = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let (dir, calls, barrier) = (dir.clone(), Arc::clone(&calls), Arc::clone(&barrier));
                std::thread::spawn(move || {
                    barrier.wait();
                    cache(&dir).get_or_fetch("/p", &request(&i.to_string()), || {
                        calls.fetch_add(1, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(100));
                        Ok::<_, String>((json!({"n": 1}), 7))
                    })
                })
            })
            .collect();
        for thread in threads {
            assert_eq!(thread.join().unwrap(), Ok((json!({"n": 1}), 7)));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn waiters_request_when_the_owner_stores_nothing() {
        let dir = temp_dir("nothing");
        let cache = cache(&dir);
        create_dir(&dir).unwrap();
        let lock_path = dir.join(format!("{}.lock", cache.key("/p", &request("a"))));
        let owner = Lock {
            pid: std::process::id(),
            created_at: now_nanos(),
            nonce: 1,
        };
        assert!(try_lock(&lock_path, &owner).unwrap());
        let releaser = {
            let lock_path = lock_path.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                release(&lock_path, &owner);
            })
        };
        let calls = AtomicUsize::new(0);
        fetch(&cache, &request("a"), &calls).unwrap();
        releaser.join().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            cache.read(&cache.key("/p", &request("a")), cache.ttl).is_none(),
            "waiters do not store"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn takes_over_the_lock_of_a_dead_owner() {
        let dir = temp_dir("dead");
        let cache = cache(&dir);
        create_dir(&dir).unwrap();
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        let lock_path = dir.join(format!("{}.lock", cache.key("/p", &request("a"))));
        let lock = Lock {
            pid: dead,
            created_at: now_nanos(),
            nonce: 1,
        };
        assert!(try_lock(&lock_path, &lock).unwrap());
        let calls = AtomicUsize::new(0);
        let started = Instant::now();
        fetch(&cache, &request("a"), &calls).unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(cache.read(&cache.key("/p", &request("a")), cache.ttl).is_some());
        assert!(!lock_path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn recognizes_its_own_files() {
        let key = "a".repeat(64);
        for name in [format!("{key}.json"), format!("{key}.lock"), format!("{key}.tmp.42.1f")] {
            assert!(is_cache_file(&name), "{name}");
        }
        for name in [
            "keep-me.txt".to_string(),
            "notes.json".to_string(),
            format!("{key}.takeover"),
            format!("{key}.txt"),
            format!("{key}.tmp.42"),
            format!("{key}.tmp.x.1f"),
            format!("{}.json", "g".repeat(64)),
        ] {
            assert!(!is_cache_file(&name), "{name}");
        }
    }

    #[test]
    fn cleanup_keeps_old_files_it_did_not_create() {
        let dir = temp_dir("cleanup");
        create_dir(&dir).unwrap();
        let old = std::time::SystemTime::now() - STALE_AFTER * 2;
        for name in ["keep-me.txt", "notes.json"] {
            File::create(dir.join(name)).unwrap().set_modified(old).unwrap();
        }
        let expired = dir.join(format!("{}.json", "a".repeat(64)));
        File::create(&expired).unwrap().set_modified(old).unwrap();
        for i in 0..=CLEANUP_MAX_ENTRIES {
            File::create(dir.join(format!("other-{i}.txt"))).unwrap();
        }
        cache(&dir).cleanup_within(Duration::MAX);
        assert!(dir.join("keep-me.txt").exists() && dir.join("notes.json").exists());
        assert!(!expired.exists(), "other files do not use up the cleanup budget");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn takes_over_an_incomplete_lock() {
        let dir = temp_dir("incomplete");
        let cache = cache(&dir);
        create_dir(&dir).unwrap();
        let lock_path = dir.join(format!("{}.lock", cache.key("/p", &request("a"))));
        File::create(&lock_path).unwrap();
        let calls = AtomicUsize::new(0);
        let started = Instant::now();
        fetch(&cache, &request("a"), &calls).unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(cache.read(&cache.key("/p", &request("a")), cache.ttl).is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Retries `f` for a while. Child processes that other tests start share
    /// open files until they exec, so they can briefly hold a released guard.
    fn eventually<T>(mut f: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(value) = f() {
                return value;
            }
            assert!(Instant::now() < deadline, "timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn only_one_agent_takes_over_a_stale_lock() {
        let dir = temp_dir("takeover");
        create_dir(&dir).unwrap();
        let path = dir.join("k.lock");
        let stale = b"stale".to_vec();
        fs::write(&path, &stale).unwrap();
        let Acquire::Owner(first) = acquire(&path, false) else {
            panic!("the first agent takes over");
        };
        let second = Lock {
            pid: std::process::id(),
            created_at: now_nanos(),
            nonce: new_id(),
        };
        assert!(matches!(take_over(&path, &stale, second), Acquire::Held));
        let current: Lock = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(current == first);
        let mut files: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        files.sort();
        assert_eq!(files, ["k.lock", "k.takeover"]);

        fs::write(&path, &stale).unwrap();
        let held = eventually(|| takeover_guard(&path.with_extension("takeover")).unwrap());
        assert!(matches!(acquire(&path, false), Acquire::Held));
        assert_eq!(fs::read(&path).unwrap(), stale);
        drop(held);
        eventually(|| matches!(acquire(&path, false), Acquire::Owner(_)).then_some(()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn bypasses_a_stale_lock_whose_guard_cannot_be_opened() {
        let dir = temp_dir("guard-error");
        create_dir(&dir).unwrap();
        let path = dir.join("k.lock");
        fs::write(&path, b"stale").unwrap();
        fs::create_dir(path.with_extension("takeover")).unwrap();
        assert!(matches!(acquire(&path, false), Acquire::Bypass));
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn bypasses_a_lock_it_cannot_read() {
        let dir = temp_dir("unreadable");
        create_dir(&dir).unwrap();
        // A directory in place of the lock fails to read with an error other
        // than NotFound, even when running as root.
        let path = dir.join("k.lock");
        fs::create_dir(&path).unwrap();
        assert!(matches!(acquire(&path, true), Acquire::Bypass));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn bypasses_an_unusable_directory() {
        let file = temp_dir("file");
        fs::write(&file, b"").unwrap();
        let calls = AtomicUsize::new(0);
        fetch(&cache(&file.join("cache")), &request("a"), &calls).unwrap();
        fetch(&cache(&file.join("cache")), &request("a"), &calls).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let _ = fs::remove_file(&file);
    }
}
