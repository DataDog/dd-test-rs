// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Builds and runs `fixtures/basic` through the wrapper against a fake intake.
//! `DD_TEST_E2E_TOOLCHAIN` selects the toolchain that builds the fixture, and so the harness.

use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SETTINGS: &str = r#"{"data":{"attributes":{"flaky_test_retries_enabled":true,"known_tests_enabled":true,
"early_flake_detection":{"enabled":true,"slow_test_retries":{"5s":3},"faulty_session_threshold":30},
"test_management":{"enabled":true,"attempt_to_fix_retries":2}}}}"#;

const KNOWN_TESTS: &str = r#"{"data":{"attributes":{"tests":{"basic-fixture":{
"basic::tests":["ignored","passes","returns_ok","panics","panics_with_message","flaky_once"],
"integration":["adds","fails"]}}}}}"#;

const TEST_MANAGEMENT: &str = r#"{"data":{"attributes":{"modules":{"basic-fixture":{"suites":{
"integration":{"tests":{"fails":{"properties":{"quarantined":true}}}},
"basic::tests":{"tests":{"passes":{"properties":{"disabled":true}}}}}}}}}}"#;

/// The variables that identify a CI provider, removed so that runs in CI report like local runs.
const CI_PROVIDER_VARS: &[&str] = &[
    "APPVEYOR",
    "TF_BUILD",
    "BITBUCKET_COMMIT",
    "BUDDY",
    "BUILDKITE",
    "CIRCLECI",
    "GITHUB_SHA",
    "GITLAB_CI",
    "JENKINS_URL",
    "TEAMCITY_VERSION",
    "TRAVIS",
    "BITRISE_BUILD_SLUG",
    "CF_BUILD_ID",
    "CODEBUILD_INITIATOR",
    "DRONE",
];

#[derive(Default)]
struct Recorded {
    upload_delay: Duration,
    paths: Vec<String>,
    settings_requests: Vec<Value>,
    events: Vec<Value>,
    session_names: Vec<String>,
}

fn serve(listener: TcpListener, recorded: Arc<Mutex<Recorded>>) {
    for stream in listener.incoming().flatten() {
        let recorded = Arc::clone(&recorded);
        std::thread::spawn(move || handle(stream, &recorded));
    }
}

fn handle(stream: TcpStream, recorded: &Mutex<Recorded>) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
    let mut length = 0;
    let mut gzip = false;
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap();
            } else if name.eq_ignore_ascii_case("content-encoding") && value.trim() == "gzip" {
                gzip = true;
            }
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    if path == "/api/v2/citestcycle" {
        let delay = recorded.lock().unwrap().upload_delay;
        std::thread::sleep(delay);
    }

    let response = match path.as_str() {
        "/api/v2/libraries/tests/services/setting" => SETTINGS,
        "/api/v2/ci/libraries/tests" => KNOWN_TESTS,
        "/api/v2/test/libraries/test-management/tests" => TEST_MANAGEMENT,
        _ => "{}",
    };
    {
        let mut recorded = recorded.lock().unwrap();
        recorded.paths.push(path.clone());
        if path == "/api/v2/libraries/tests/services/setting" {
            recorded.settings_requests.push(serde_json::from_slice(&body).unwrap());
        }
        if path == "/api/v2/citestcycle" {
            if gzip {
                let mut decoded = Vec::new();
                flate2::read::GzDecoder::new(body.as_slice())
                    .read_to_end(&mut decoded)
                    .unwrap();
                body = decoded;
            }
            let payload: Value = rmp_serde::from_slice(&body).unwrap();
            if let Some(name) = payload
                .pointer("/metadata/test/test_session.name")
                .and_then(Value::as_str)
            {
                recorded.session_names.push(name.to_string());
            }
            if let Some(Value::Array(events)) = payload.get("events") {
                recorded.events.extend(events.iter().cloned());
            }
        }
    }
    let mut stream = reader.into_inner();
    let reply = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
        response.len()
    );
    stream.write_all(reply.as_bytes()).unwrap();
}

fn meta<'a>(event: &'a Value, key: &str) -> &'a str {
    event
        .pointer(&format!("/content/meta/{key}"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn tests_named<'a>(events: &'a [Value], suite: &str, name: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) == Some("test"))
        .filter(|e| meta(e, "test.suite") == suite && meta(e, "test.name") == name)
        .collect()
}

struct Run {
    output: std::process::Output,
    recorded: Arc<Mutex<Recorded>>,
    read_cache: PathBuf,
}

fn run_fixture(args: &[&str], env: &[(&str, &str)]) -> Run {
    run_fixture_with_upload_delay(args, env, Duration::ZERO)
}

fn run_fixture_with_upload_delay(args: &[&str], env: &[(&str, &str)], upload_delay: Duration) -> Run {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let recorded = Arc::new(Mutex::new(Recorded {
        upload_delay,
        ..Recorded::default()
    }));
    let server_state = Arc::clone(&recorded);
    std::thread::spawn(move || serve(listener, server_state));

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let read_cache = root.join(format!("target/e2e/read-cache/{}", url.rsplit(':').next().unwrap()));
    let _ = std::fs::remove_dir_all(&read_cache);
    let mut command = match std::env::var("DD_TEST_E2E_TOOLCHAIN") {
        Ok(toolchain) => {
            let mut command = Command::new("cargo");
            command.arg(format!("+{toolchain}"));
            command
        }
        Err(_) => Command::new(env!("CARGO")),
    };
    for var in CI_PROVIDER_VARS {
        command.env_remove(var);
    }
    let output = command
        .args(args)
        .current_dir(root.join("fixtures/basic"))
        .env("CARGO_TARGET_DIR", root.join("target/e2e"))
        .env("RUSTC_WORKSPACE_WRAPPER", env!("CARGO_BIN_EXE_dd-test-wrapper"))
        .env("DD_CIVISIBILITY_AGENTLESS_URL", &url)
        .env("DD_API_KEY", "test")
        .env("DD_TEST_READ_CACHE_DIR", &read_cache)
        .env("DD_GIT_REPOSITORY_URL", "https://github.com/DataDog/dd-test-rs.git")
        .env("DD_GIT_COMMIT_SHA", "0123456789abcdef0123456789abcdef01234567")
        .env_remove("RUSTC_WRAPPER")
        .envs(env.iter().copied())
        .output()
        .unwrap();
    Run {
        output,
        recorded,
        read_cache,
    }
}

fn events_of<'a>(events: &'a [Value], kind: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) == Some(kind))
        .collect()
}

#[test]
fn binaries_built_with_reporting_off_report_when_it_is_on() {
    let target_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/e2e-reporting-off");
    let _ = std::fs::remove_dir_all(&target_dir);
    let target_dir = target_dir.to_str().unwrap();
    let args = ["test", "--quiet", "--test", "integration"];
    let off = run_fixture(
        &args,
        &[("CARGO_TARGET_DIR", target_dir), ("DD_CIVISIBILITY_ENABLED", "false")],
    );
    assert!(
        !off.output.status.success(),
        "the quarantined failure fails without Datadog"
    );
    assert!(off.recorded.lock().unwrap().paths.is_empty());

    let on = run_fixture(&args, &[("CARGO_TARGET_DIR", target_dir)]);
    assert!(
        on.output.status.success(),
        "{}",
        String::from_utf8_lossy(&on.output.stderr)
    );
    let recorded = on.recorded.lock().unwrap();
    assert_eq!(events_of(&recorded.events, "test_session_end").len(), 1);
}

#[test]
fn binaries_without_matching_tests_are_not_reported() {
    let run = run_fixture(&["test", "--quiet", "--", "--exact", "adds"], &[]);
    assert!(run.output.status.success());
    let recorded = run.recorded.lock().unwrap();
    let events = &recorded.events;
    let tests: Vec<_> = events_of(events, "test").iter().map(|t| meta(t, "test.name")).collect();
    assert_eq!(tests, ["adds"]);
    assert_eq!(events_of(events, "test_session_end").len(), 1);
    assert_eq!(events_of(events, "test_module_end").len(), 1);
}

#[test]
fn names_resources_like_other_libraries() {
    let run = run_fixture(
        &["test", "--quiet", "--test", "integration", "--", "--exact", "adds"],
        &[],
    );
    assert!(run.output.status.success());
    let recorded = run.recorded.lock().unwrap();
    let resources = |kind| -> Vec<&str> {
        events_of(&recorded.events, kind)
            .iter()
            .map(|e| e.pointer("/content/resource").and_then(Value::as_str).unwrap_or(""))
            .collect()
    };
    assert_eq!(resources("test"), ["integration.adds"]);
    assert_eq!(resources("test_suite_end"), ["libtest.test_suite.integration"]);
    assert_eq!(resources("test_module_end"), ["libtest.test_module.basic-fixture"]);
    assert!(resources("test_session_end")[0].starts_with("libtest.test_session.cargo test"));
}

#[test]
fn adds_dd_tags_to_events_and_custom_configurations_to_requests() {
    let run = run_fixture(
        &["test", "--quiet", "--test", "integration"],
        &[("DD_TAGS", "team:ci,test.configuration.gpu:a100")],
    );
    assert!(run.output.status.success());
    let recorded = run.recorded.lock().unwrap();
    let configurations = &recorded.settings_requests[0]["data"]["attributes"]["configurations"];
    assert_eq!(configurations["custom"], serde_json::json!({ "gpu": "a100" }));
    assert_eq!(configurations["runtime.name"], "rustc");
    for kind in ["test", "test_suite_end", "test_module_end", "test_session_end"] {
        let events = events_of(&recorded.events, kind);
        assert!(!events.is_empty(), "{kind}");
        for event in events {
            assert_eq!(meta(event, "team"), "ci", "{kind}");
            assert_eq!(meta(event, "test.configuration.gpu"), "a100", "{kind}");
        }
    }
}

#[test]
fn results_survive_the_test_binary_exiting() {
    let run = run_fixture_with_upload_delay(
        &["test", "--quiet", "--test", "exits", "--", "--test-threads=1"],
        &[
            ("BASIC_EXIT", "1"),
            ("DD_CIVISIBILITY_EARLY_FLAKE_DETECTION_ENABLED", "false"),
        ],
        Duration::from_secs(2),
    );
    assert!(run.output.status.success());
    assert!(
        run.recorded.lock().unwrap().events.is_empty(),
        "the agent releases cargo's stderr before it uploads"
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    while events_of(&run.recorded.lock().unwrap().events, "test_session_end").is_empty() {
        assert!(
            Instant::now() < deadline,
            "the agent reports the session after the test binary exits"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    let recorded = run.recorded.lock().unwrap();
    let events = &recorded.events;
    assert_eq!(tests_named(events, "exits", "a_reported_before_exit").len(), 1);
    assert!(tests_named(events, "exits", "c_never_runs_after_exit").is_empty());
    let sessions = events_of(events, "test_session_end");
    assert_eq!(sessions.len(), 1);
    assert_eq!(meta(sessions[0], "test.status"), "fail");
    assert_eq!(
        meta(sessions[0], "error.message"),
        "The test binary exited before reporting all results"
    );
    assert_eq!(events_of(events, "test_suite_end").len(), 1);
}

#[test]
fn reports_and_controls_the_fixture() {
    let run = run_fixture(&["test", "--no-fail-fast", "--quiet"], &[]);
    let output = &run.output;
    assert!(
        output.status.success(),
        "the quarantined failure and the retried flaky test must not fail the run:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let recorded = run.recorded.lock().unwrap();
    let events = &recorded.events;
    let count = |kind: &str| {
        events
            .iter()
            .filter(|e| e.get("type").and_then(Value::as_str) == Some(kind))
            .count()
    };
    assert_eq!(count("test_session_end"), 3, "one session per test binary");
    assert_eq!(count("test_module_end"), 3);
    let requests = |path: &str| recorded.paths.iter().filter(|p| p.as_str() == path).count();
    assert_eq!(
        [
            requests("/api/v2/libraries/tests/services/setting"),
            requests("/api/v2/ci/libraries/tests"),
            requests("/api/v2/test/libraries/test-management/tests"),
        ],
        [1, 1, 1],
        "the three test binaries share the backend responses"
    );
    let entries = std::fs::read_dir(&run.read_cache)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .count();
    assert_eq!(entries, 4, "three backend responses and the local git tags");

    assert!(!recorded.session_names.is_empty());
    assert!(recorded
        .session_names
        .iter()
        .all(|name| name == "cargo test --no-fail-fast --quiet"));
    assert!(events_of(events, "test")
        .iter()
        .all(|t| meta(t, "test.command") == "cargo test --no-fail-fast --quiet"));

    let flaky = tests_named(events, "basic::tests", "flaky_once");
    assert_eq!(flaky.len(), 2, "ATR retries the failed execution once");
    assert_eq!(meta(flaky[0], "test.failure_suppression_reason"), "auto_test_retry");
    assert_eq!(meta(flaky[1], "test.retry_reason"), "auto_test_retry");

    let new = tests_named(events, "basic::tests::nested", "deep");
    assert_eq!(new.len(), 3, "EFD runs the new test 3 times");
    assert!(new.iter().all(|e| meta(e, "test.is_new") == "true"));

    let disabled = tests_named(events, "basic::tests", "passes");
    assert_eq!(disabled.len(), 1);
    assert_eq!(meta(disabled[0], "test.status"), "skip");
    assert_eq!(meta(disabled[0], "test.final_status"), "skip");
    assert_eq!(
        meta(disabled[0], "test.skip_reason"),
        "Flaky test is disabled by Datadog"
    );

    let quarantined = tests_named(events, "integration", "fails");
    assert!(quarantined.iter().all(|e| meta(e, "test.status") == "fail"));
    assert!(quarantined
        .iter()
        .all(|e| meta(e, "test.failure_suppression_reason") == "quarantine"));
    let final_statuses: Vec<_> = quarantined
        .iter()
        .filter_map(|e| e["content"]["meta"]["test.final_status"].as_str())
        .collect();
    assert_eq!(
        final_statuses,
        ["pass"],
        "libtest sees the suppressed failure as a pass"
    );
    assert_eq!(
        meta(quarantined[0], "error.message"),
        "assertion `left == right` failed: math is broken\n  left: 2\n right: 3"
    );

    let ignored = tests_named(events, "basic::tests", "ignored");
    assert_eq!(meta(ignored[0], "test.skip_reason"), "not ready");
    assert_eq!(meta(ignored[0], "test.source.file"), "fixtures/basic/src/lib.rs");

    let sessions: Vec<_> = events
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) == Some("test_session_end"))
        .collect();
    assert!(sessions.iter().all(|s| meta(s, "test.status") == "pass"));
    assert!(sessions
        .iter()
        .all(|s| meta(s, "test.test_management.enabled") == "true"));
}
