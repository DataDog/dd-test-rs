// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use crate::api::{self, RequestContext, Settings};
use crate::ci::{self, ProcessEnv, Tags};
use crate::config::Config;
use crate::event::{encode_payload, Kind, Metadata, Span};
use crate::ids::{new_id, now_nanos};
use crate::read_cache::{self, ReadCache, Scope};
use crate::telemetry::{self, Telemetry};
use crate::transport::{Intake, Transport};
use crate::uploader::Uploader;
use dd_test_core::protocol::{
    AutoTestRetries, Bootstrap, EarlyFlakeDetection, Finish, Hello, Status, TestManagement, TestRecord,
};
use dd_test_core::{debug, warn};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

pub const FRAMEWORK: &str = "libtest";
const DEFAULT_ATTEMPT_TO_FIX_RETRIES: u32 = 20;
const MAX_EVENTS_PER_PAYLOAD: usize = 1000;
const INCOMPLETE_MESSAGE: &str = "The test binary exited before reporting all results";

#[derive(Default)]
struct Counts {
    passed: usize,
    failed: usize,
    skipped: usize,
}

impl Counts {
    fn add(&mut self, status: Status) {
        match status {
            Status::Pass => self.passed += 1,
            Status::Fail => self.failed += 1,
            Status::Skip => self.skipped += 1,
        }
    }

    fn status(&self) -> Status {
        if self.failed > 0 {
            Status::Fail
        } else if self.passed > 0 {
            Status::Pass
        } else {
            Status::Skip
        }
    }
}

struct SuiteState {
    id: u64,
    start: i64,
    end: i64,
    source_file: Option<String>,
    counts: Counts,
}

/// One test session: one test binary.
pub struct AgentSession {
    uploader: Uploader,
    telemetry: Option<Arc<Telemetry>>,
    provider: String,
    service: String,
    module: String,
    command: String,
    working_directory: String,
    common: Tags,
    session_id: u64,
    module_id: u64,
    start: i64,
    early_flake_detection: bool,
    test_management: bool,
    suites: BTreeMap<String, SuiteState>,
    counts: Counts,
}

impl AgentSession {
    pub fn start(hello: &Hello, config: Config) -> (AgentSession, Bootstrap) {
        let working_dir = canonical(Path::new(&hello.working_dir));
        let provider_tags = ci::provider_tags(&ProcessEnv);
        let nextest_run_id = std::env::var("NEXTEST_RUN_ID").ok();
        let cache = Scope::select(&provider_tags, nextest_run_id.as_deref(), hello.parent_pid)
            .map(|scope| ReadCache::new(read_cache::default_dir(), &scope, &config, &hello.library_version));
        let tags = ci::collect(&ProcessEnv, provider_tags, &working_dir, cache.as_ref());
        let repo_root = repo_root(tags.get("ci.workspace_path").map(String::as_str), &working_dir);
        let user_service = config.service.clone();
        let service = user_service
            .clone()
            .or_else(|| tags.get("git.repository_url").and_then(|url| repository_name(url)))
            .unwrap_or_else(|| hello.module.clone());
        let env = config.env.clone().unwrap_or_else(|| "none".to_string());

        let mut configurations = BTreeMap::new();
        configurations.insert("os.platform".to_string(), os_platform().to_string());
        configurations.insert("os.architecture".to_string(), std::env::consts::ARCH.to_string());
        if let Some(version) = os_version() {
            configurations.insert("os.version".to_string(), version);
        }
        configurations.insert("runtime.name".to_string(), "rustc".to_string());
        configurations.insert("runtime.version".to_string(), hello.rustc_version.clone());

        let mut common = config.tags.clone();
        common.extend(tags.clone());
        common.extend(configurations.clone());
        for (key, value) in [
            ("_dd.origin", "ciapp-test"),
            ("language", "rust"),
            ("span.kind", "test"),
            ("component", FRAMEWORK),
            ("test.type", "test"),
            ("test.framework", FRAMEWORK),
            ("_dd.library_capabilities.auto_test_retries", "1"),
            ("_dd.library_capabilities.early_flake_detection", "1"),
            ("_dd.library_capabilities.test_management.quarantine", "1"),
            ("_dd.library_capabilities.test_management.disable", "1"),
            ("_dd.library_capabilities.test_management.attempt_to_fix", "5"),
        ] {
            common.insert(key.to_string(), value.to_string());
        }
        common.insert("test.framework_version".into(), hello.rustc_version.clone());
        common.insert("library_version".into(), hello.library_version.clone());
        common.insert("env".into(), env.clone());
        common.insert(
            "_dd.test.is_user_provided_service".into(),
            user_service.is_some().to_string(),
        );

        let context = RequestContext {
            service: service.clone(),
            env: env.clone(),
            repository_url: tags.get("git.repository_url").cloned().unwrap_or_default(),
            branch: tags
                .get("git.branch")
                .or_else(|| tags.get("git.tag"))
                .cloned()
                .unwrap_or_default(),
            sha: tags.get("git.commit.sha").cloned().unwrap_or_default(),
            commit_message: tags.get("git.commit.message").cloned().unwrap_or_default(),
            configurations,
            custom_configurations: config
                .tags
                .iter()
                .filter_map(|(key, value)| Some((key.strip_prefix("test.configuration.")?.to_string(), value.clone())))
                .collect(),
        };
        let transport = match Transport::new(&config) {
            Ok(transport) => {
                transport.warm_up(Intake::TestCycle);
                Some(transport)
            }
            Err(error) => {
                warn!("test results will not be sent to Datadog: {error}");
                None
            }
        };
        let telemetry =
            Telemetry::start(&config, &service, &env, &hello.rustc_version, &hello.library_version).map(Arc::new);
        let mut bootstrap = match &transport {
            Some(transport) => load_features(
                transport,
                cache.as_ref(),
                telemetry.as_deref(),
                &context,
                &config,
                &hello.module,
            ),
            None => Bootstrap::default(),
        };
        bootstrap.repo_root = Some(repo_root.to_string_lossy().to_string());

        let working_directory = working_dir
            .strip_prefix(&repo_root)
            .map(|p| p.to_string_lossy().to_string())
            .ok()
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| ".".to_string());
        let metadata = Metadata {
            runtime_id: format!("{:016x}{:016x}", new_id(), new_id()),
            library_version: hello.library_version.clone(),
            env,
            session_name: session_name(config.session_name.as_deref(), &tags, &hello.command),
        };
        let uploader = Uploader::start({
            let telemetry = telemetry.clone();
            move |spans| upload(transport.as_ref(), telemetry.as_deref(), &metadata, &spans)
        });
        let session = AgentSession {
            uploader,
            telemetry,
            provider: tags
                .get("ci.provider.name")
                .cloned()
                .unwrap_or_else(|| "unsupported".to_string()),
            service,
            module: hello.module.clone(),
            command: hello.command.clone(),
            working_directory,
            common,
            session_id: new_id(),
            module_id: new_id(),
            start: now_nanos(),
            early_flake_detection: bootstrap.early_flake_detection.is_some(),
            test_management: bootstrap.test_management.is_some(),
            suites: BTreeMap::new(),
            counts: Counts::default(),
        };
        (session, bootstrap)
    }

    pub fn record(&mut self, record: TestRecord) {
        let resource = format!("{}.{}", record.suite, record.name);
        let mut span = Span::new(Kind::Test, "libtest.test", &resource, &self.service, record.start);
        span.trace_id = record.trace_id;
        span.span_id = record.span_id;
        span.session_id = self.session_id;
        span.module_id = self.module_id;
        span.duration = record.duration;
        span.meta = self.common.clone();
        span.set("test.module", self.module.clone());
        span.set("test.command", self.command.clone());
        span.set("test.working_directory", self.working_directory.clone());
        span.set("test.suite", record.suite.clone());
        span.set("test.name", record.name.clone());
        if let Some(file) = &record.source_file {
            span.set("test.source.file", file.clone());
            span.set_metric("test.source.start", record.start_line as f64);
            span.set_metric("test.source.end", record.end_line as f64);
        }
        span.set_metric("_dd.top_level", 1.0);
        if let Some(t) = &self.telemetry {
            let mut tags = vec!["test_framework:libtest", "event_type:test"];
            t.count("event_created", &tags);
            for (tag, metric) in [
                ("test.is_new", "is_new:true"),
                ("test.is_retry", "is_retry:true"),
                ("test.test_management.is_attempt_to_fix", "is_attempt_to_fix:true"),
                ("test.test_management.is_quarantined", "is_quarantined:true"),
                ("test.test_management.is_test_disabled", "is_disabled:true"),
                ("test.has_failed_all_retries", "has_failed_all_retries:true"),
            ] {
                if record.tags.get(tag).is_some_and(|v| v == "true") {
                    tags.push(metric);
                }
            }
            if record
                .tags
                .get("test.early_flake.abort_reason")
                .is_some_and(|v| v == "slow")
            {
                tags.push("early_flake_detection_abort_reason:slow");
            }
            t.count("event_finished", &tags);
        }
        span.meta.extend(record.tags);
        if let Some(error) = record.error {
            span.error = true;
            span.set("error.type", error.kind);
            span.set("error.message", error.message);
            if let Some(stack) = error.stack {
                span.set("error.stack", stack);
            }
        }

        let end = record.start + record.duration;
        let suite = self.suites.entry(record.suite).or_insert_with(|| SuiteState {
            id: new_id(),
            start: span.start,
            end,
            source_file: record.source_file,
            counts: Counts::default(),
        });
        suite.start = suite.start.min(span.start);
        suite.end = suite.end.max(end);
        span.suite_id = suite.id;
        if let Some(outcome) = record.outcome {
            suite.counts.add(outcome);
            self.counts.add(outcome);
        }
        self.uploader.send(span);
    }

    /// Reports suites, module and session, then sends everything. `None` means
    /// the test binary exited without finishing, so the session fails.
    pub fn finish(&mut self, finish: Option<Finish>) {
        let end = now_nanos();
        let complete = finish.is_some();
        let status = if complete { self.counts.status() } else { Status::Fail };
        let suites = std::mem::take(&mut self.suites);
        for (name, suite) in suites {
            let mut span = self.container(Kind::Suite, "libtest.test_suite", &format!("{}.{name}", self.module));
            span.start = suite.start;
            span.duration = suite.end - suite.start;
            span.suite_id = suite.id;
            span.set("test.suite", name);
            if let Some(file) = suite.source_file {
                span.set("test.source.file", file);
            }
            set_status(&mut span, suite.counts.status());
            self.uploader.send(span);
            self.container_events("event_type:suite", false);
        }
        for (kind, name, resource) in [
            (Kind::Module, "libtest.test_module", self.module.clone()),
            (
                Kind::Session,
                "libtest.test_session",
                format!("libtest.test_session.{}", self.command),
            ),
        ] {
            let mut span = self.container(kind, name, &resource);
            span.duration = end - self.start;
            set_status(&mut span, status);
            if !complete {
                span.set("error.message", INCOMPLETE_MESSAGE);
            }
            span.set("test.itr.tests_skipping.enabled", "false");
            span.set("test.code_coverage.enabled", "false");
            if self.early_flake_detection {
                span.set("test.early_flake.enabled", "true");
                if finish.is_some_and(|f| f.efd_faulty) {
                    span.set("test.early_flake.abort_reason", "faulty");
                }
            }
            if self.test_management {
                span.set("test.test_management.enabled", "true");
            }
            self.uploader.send(span);
            let event_type = if kind == Kind::Module {
                "event_type:module"
            } else {
                "event_type:session"
            };
            self.container_events(event_type, finish.is_some_and(|f| f.efd_faulty));
        }
        if let Some(t) = &self.telemetry {
            let provider = format!("provider:{}", self.provider);
            t.count("test_session", &[provider.as_str()]);
        }
        self.uploader.close();
    }

    /// Flushes telemetry. Runs after the test binary was told its results are
    /// delivered, so a slow telemetry endpoint does not hold it up.
    pub fn close(&self) {
        if let Some(t) = &self.telemetry {
            t.stop();
        }
    }

    fn container_events(&self, event_type: &str, efd_faulty: bool) {
        if let Some(t) = &self.telemetry {
            t.count("event_created", &["test_framework:libtest", event_type]);
            let mut tags = vec!["test_framework:libtest", event_type];
            if efd_faulty && event_type == "event_type:session" {
                tags.push("early_flake_detection_abort_reason:faulty");
            }
            t.count("event_finished", &tags);
        }
    }

    fn container(&self, kind: Kind, name: &str, resource: &str) -> Span {
        let mut span = Span::new(kind, name, resource, &self.service, self.start);
        span.session_id = self.session_id;
        span.module_id = self.module_id;
        span.meta = self.common.clone();
        span.meta.remove("span.kind");
        span.set("test.module", self.module.clone());
        span.set("test.command", self.command.clone());
        span.set("test.working_directory", self.working_directory.clone());
        span.set_metric("_dd.top_level", 1.0);
        span
    }
}

fn upload(transport: Option<&Transport>, telemetry: Option<&Telemetry>, metadata: &Metadata, spans: &[Span]) {
    let Some(transport) = transport else { return };
    for chunk in spans.chunks(MAX_EVENTS_PER_PAYLOAD) {
        if transport.is_unreachable(Intake::TestCycle) {
            debug!("dropped {} events, Datadog is unreachable", chunk.len());
            continue;
        }
        let payload = encode_payload(metadata, chunk);
        let started = Instant::now();
        let result = transport.post(
            Intake::TestCycle,
            "/api/v2/citestcycle",
            "application/msgpack",
            &payload,
            true,
        );
        if let Some(t) = telemetry {
            let endpoint = "endpoint:test_cycle";
            t.count("endpoint_payload.requests", &[endpoint, "rq_compressed:true"]);
            t.distribution(
                "endpoint_payload.requests_ms",
                started.elapsed().as_secs_f64() * 1000.0,
                &[endpoint],
            );
            t.distribution("endpoint_payload.bytes", payload.len() as f64, &[endpoint]);
            t.distribution("endpoint_payload.events_count", chunk.len() as f64, &[endpoint]);
            if let Err(error) = &result {
                let mut tags = telemetry::error_tags(error.status);
                tags.insert(0, endpoint.to_string());
                t.count(
                    "endpoint_payload.requests_errors",
                    &tags.iter().map(String::as_str).collect::<Vec<_>>(),
                );
            }
        }
        match result {
            Ok(_) => debug!("sent {} events ({} bytes)", chunk.len(), payload.len()),
            Err(error) => warn!("could not send test results to Datadog: {error}"),
        }
    }
}

/// `DD_TEST_SESSION_NAME`, else the test command, prefixed with the CI job name
/// when there is one, like the other Test Optimization libraries.
fn session_name(configured: Option<&str>, tags: &Tags, command: &str) -> String {
    match (configured, tags.get("ci.job.name")) {
        (Some(name), _) => name.to_string(),
        (None, Some(job)) => format!("{job}-{command}"),
        (None, None) => command.to_string(),
    }
}

fn set_status(span: &mut Span, status: Status) {
    span.set("test.status", status.as_str());
    span.error = status == Status::Fail;
}

fn load_features(
    transport: &Transport,
    cache: Option<&ReadCache>,
    telemetry: Option<&Telemetry>,
    context: &RequestContext,
    config: &Config,
    module: &str,
) -> Bootstrap {
    if transport.is_unreachable(Intake::Api) {
        return Bootstrap::default();
    }
    let settings = match api::fetch_settings(transport, cache, context, telemetry) {
        Ok(settings) => settings,
        Err(error) => {
            warn!("could not fetch Test Optimization settings, features are disabled: {error}");
            Settings::default()
        }
    };
    debug!("settings: {settings:?}");
    let auto_test_retries =
        (settings.flaky_test_retries_enabled && config.flaky_retry_enabled).then_some(AutoTestRetries {
            retries: config.flaky_retry_count,
            budget: config.total_flaky_retry_count,
        });
    let want_efd =
        settings.early_flake_detection.enabled && settings.known_tests_enabled && config.early_flake_detection_enabled;
    let want_tm =
        settings.test_management.enabled && config.test_management_enabled && !context.repository_url.is_empty();

    let (known, properties) = std::thread::scope(|scope| {
        let known = want_efd.then(|| scope.spawn(|| api::fetch_known_tests(transport, cache, context, telemetry)));
        let properties =
            want_tm.then(|| scope.spawn(|| api::fetch_test_management(transport, cache, context, telemetry)));
        (known.map(join), properties.map(join))
    });

    let early_flake_detection = match known {
        Some(Ok(known)) if !known.is_empty() => Some(EarlyFlakeDetection {
            buckets: settings.early_flake_detection.slow_test_retries.clone(),
            faulty_session_threshold: settings.early_flake_detection.faulty_session_threshold,
            known_tests: known
                .into_iter()
                .filter(|k| k.module == module)
                .map(|k| (k.suite, k.name))
                .collect(),
        }),
        Some(Ok(_)) => {
            debug!("known tests response is empty, early flake detection is disabled");
            None
        }
        Some(Err(error)) => {
            warn!("could not fetch known tests, early flake detection is disabled: {error}");
            None
        }
        None => None,
    };
    let test_management = match properties {
        Some(Ok(properties)) => Some(TestManagement {
            attempt_to_fix_retries: config
                .attempt_to_fix_retries
                .or(settings.test_management.attempt_to_fix_retries)
                .unwrap_or(DEFAULT_ATTEMPT_TO_FIX_RETRIES),
            tests: properties
                .into_iter()
                .filter(|(k, _)| k.module == module)
                .map(|(k, p)| ((k.suite, k.name), p))
                .collect(),
        }),
        Some(Err(error)) => {
            warn!("could not fetch Test Management properties, Test Management is disabled: {error}");
            None
        }
        None => None,
    };
    Bootstrap {
        repo_root: None,
        auto_test_retries,
        early_flake_detection,
        test_management,
    }
}

fn join<T>(handle: std::thread::ScopedJoinHandle<'_, Result<T, String>>) -> Result<T, String> {
    handle
        .join()
        .unwrap_or_else(|_| Err("request thread panicked".to_string()))
}

/// The CI workspace when the tests run inside it, else the working directory.
fn repo_root(workspace: Option<&str>, working_dir: &Path) -> PathBuf {
    workspace
        .map(|root| canonical(Path::new(root)))
        .filter(|root| working_dir.starts_with(root))
        .unwrap_or_else(|| working_dir.to_path_buf())
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn repository_name(url: &str) -> Option<String> {
    let trimmed = url.trim_end_matches('/').trim_end_matches(".git");
    let name = trimmed.rsplit(['/', ':']).next()?;
    (!name.is_empty()).then(|| name.to_string())
}

fn os_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    }
}

fn os_version() -> Option<String> {
    let output = std::process::Command::new("uname").arg("-r").output().ok()?;
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && !version.is_empty()).then_some(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_sessions_like_other_libraries() {
        let mut tags = Tags::new();
        assert_eq!(session_name(None, &tags, "cargo test"), "cargo test");
        tags.insert("ci.job.name".into(), "unit".into());
        assert_eq!(session_name(None, &tags, "cargo test"), "unit-cargo test");
        assert_eq!(session_name(Some("custom"), &tags, "cargo test"), "custom");
    }

    #[cfg(unix)]
    #[test]
    fn resolves_symlinks_in_the_ci_workspace_path() {
        let dir = std::env::temp_dir().join(format!("dd-test-root-{:x}", new_id()));
        let workspace = dir.join("workspace");
        let member = workspace.join("member");
        std::fs::create_dir_all(&member).unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&workspace, &link).unwrap();
        let member = member.canonicalize().unwrap();
        let workspace = workspace.canonicalize().unwrap();

        assert_eq!(repo_root(link.to_str(), &member), workspace);
        assert_eq!(repo_root(dir.join("other").to_str(), &member), member);
        assert_eq!(repo_root(None, &member), member);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn derives_repository_names() {
        assert_eq!(
            repository_name("https://github.com/tokio-rs/bytes.git").as_deref(),
            Some("bytes")
        );
        assert_eq!(
            repository_name("git@github.com:DataDog/dd-test-rs").as_deref(),
            Some("dd-test-rs")
        );
    }
}
