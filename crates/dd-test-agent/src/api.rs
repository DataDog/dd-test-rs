// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use crate::ids::new_id;
use crate::read_cache::ReadCache;
use crate::telemetry::{self, Telemetry};
use crate::transport::Transport;
use dd_test_core::protocol::TestProperties;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

const MAX_KNOWN_TESTS_PAGES: usize = 1000;

/// Repository and runtime identity shared by backend requests.
#[derive(Clone, Debug, Default)]
pub struct RequestContext {
    pub service: String,
    pub env: String,
    pub repository_url: String,
    pub branch: String,
    pub sha: String,
    pub commit_message: String,
    pub configurations: BTreeMap<String, String>,
    /// `DD_TAGS` entries named `test.configuration.<name>`, by name.
    pub custom_configurations: BTreeMap<String, String>,
}

impl RequestContext {
    fn configurations(&self) -> Value {
        let mut configurations = json!(self.configurations);
        if !self.custom_configurations.is_empty() {
            configurations["custom"] = json!(self.custom_configurations);
        }
        configurations
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    pub itr_enabled: bool,
    pub tests_skipping: bool,
    pub code_coverage: bool,
    pub require_git: bool,
    pub known_tests_enabled: bool,
    pub flaky_test_retries_enabled: bool,
    pub early_flake_detection: EarlyFlakeDetection,
    pub test_management: TestManagementSettings,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct EarlyFlakeDetection {
    pub enabled: bool,
    /// Duration upper bounds and the total number of executions, sorted by bound.
    pub slow_test_retries: Vec<(Duration, u32)>,
    pub faulty_session_threshold: f64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TestManagementSettings {
    pub enabled: bool,
    pub attempt_to_fix_retries: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TestKey {
    pub module: String,
    pub suite: String,
    pub name: String,
}

impl TestKey {
    pub fn new(module: &str, suite: &str, name: &str) -> TestKey {
        TestKey {
            module: module.to_string(),
            suite: suite.to_string(),
            name: name.to_string(),
        }
    }
}

fn request_id() -> String {
    format!("{:x}", new_id())
}

/// Telemetry metric names of one endpoint: requests, their duration and their errors.
type Metrics = (&'static str, &'static str, &'static str);

fn read(
    transport: &Transport,
    cache: Option<&ReadCache>,
    telemetry: Option<&Telemetry>,
    path: &str,
    body: &Value,
    (requests, request_ms, errors): Metrics,
) -> Result<(Value, usize), String> {
    let live = || {
        telemetry::request(telemetry, requests, request_ms, errors, || {
            transport.post_json(path, body)
        })
        .map_err(|e| e.message)
    };
    match cache {
        Some(cache) => cache.get_or_fetch(path, body, live),
        None => live(),
    }
}

pub fn fetch_settings(
    transport: &Transport,
    cache: Option<&ReadCache>,
    ctx: &RequestContext,
    telemetry: Option<&Telemetry>,
) -> Result<Settings, String> {
    let body = json!({
        "data": {
            "id": request_id(),
            "type": "ci_app_test_service_libraries_settings",
            "attributes": {
                "service": ctx.service,
                "env": ctx.env,
                "repository_url": ctx.repository_url,
                "branch": ctx.branch,
                "sha": ctx.sha,
                "test_level": "test",
                "configurations": ctx.configurations(),
            }
        }
    });
    let (response, _) = read(
        transport,
        cache,
        telemetry,
        "/api/v2/libraries/tests/services/setting",
        &body,
        (
            "git_requests.settings",
            "git_requests.settings_ms",
            "git_requests.settings_errors",
        ),
    )?;
    let settings = parse_settings(&response)?;
    if let Some(t) = telemetry {
        let mut tags = Vec::new();
        if settings.early_flake_detection.enabled {
            tags.push("early_flake_detection_enabled:true");
        }
        if settings.flaky_test_retries_enabled {
            tags.push("flaky_test_retries_enabled:true");
        }
        if settings.test_management.enabled {
            tags.push("test_management_enabled:true");
        }
        t.count("git_requests.settings_response", &tags);
    }
    Ok(settings)
}

pub fn parse_settings(response: &Value) -> Result<Settings, String> {
    let attrs = response
        .pointer("/data/attributes")
        .ok_or("settings response has no data.attributes")?;
    let flag = |pointer: &str| attrs.pointer(pointer).and_then(Value::as_bool).unwrap_or(false);
    let mut slow_test_retries: Vec<(Duration, u32)> = attrs
        .pointer("/early_flake_detection/slow_test_retries")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(bound, count)| Some((parse_duration(bound)?, count.as_u64()? as u32)))
                .collect()
        })
        .unwrap_or_default();
    slow_test_retries.sort();
    Ok(Settings {
        itr_enabled: flag("/itr_enabled"),
        tests_skipping: flag("/tests_skipping"),
        code_coverage: flag("/code_coverage"),
        require_git: flag("/require_git"),
        known_tests_enabled: flag("/known_tests_enabled"),
        flaky_test_retries_enabled: flag("/flaky_test_retries_enabled"),
        early_flake_detection: EarlyFlakeDetection {
            enabled: flag("/early_flake_detection/enabled"),
            slow_test_retries,
            faulty_session_threshold: attrs
                .pointer("/early_flake_detection/faulty_session_threshold")
                .and_then(Value::as_f64)
                .unwrap_or(30.0),
        },
        test_management: TestManagementSettings {
            enabled: flag("/test_management/enabled"),
            attempt_to_fix_retries: attrs
                .pointer("/test_management/attempt_to_fix_retries")
                .and_then(Value::as_u64)
                .filter(|n| *n >= 1)
                .map(|n| n as u32),
        },
    })
}

fn parse_duration(text: &str) -> Option<Duration> {
    let split = text.find(|c: char| !c.is_ascii_digit() && c != '.')?;
    let (number, unit) = text.split_at(split);
    let value: f64 = number.parse().ok()?;
    let seconds = match unit {
        "ms" => value / 1000.0,
        "s" => value,
        "m" => value * 60.0,
        "h" => value * 3600.0,
        _ => return None,
    };
    Some(Duration::from_secs_f64(seconds))
}

pub fn fetch_known_tests(
    transport: &Transport,
    cache: Option<&ReadCache>,
    ctx: &RequestContext,
    telemetry: Option<&Telemetry>,
) -> Result<HashSet<TestKey>, String> {
    let mut known = HashSet::new();
    let mut page_state: Option<String> = None;
    for _ in 0..MAX_KNOWN_TESTS_PAGES {
        let page_info = match &page_state {
            Some(state) => json!({ "page_state": state }),
            None => json!({}),
        };
        let body = json!({
            "data": {
                "id": request_id(),
                "type": "ci_app_libraries_tests_request",
                "attributes": {
                    "service": ctx.service,
                    "env": ctx.env,
                    "repository_url": ctx.repository_url,
                    "sha": ctx.sha,
                    "configurations": ctx.configurations(),
                    "page_info": page_info,
                }
            }
        });
        let (response, bytes) = read(
            transport,
            cache,
            telemetry,
            "/api/v2/ci/libraries/tests",
            &body,
            (
                "known_tests.request",
                "known_tests.request_ms",
                "known_tests.request_errors",
            ),
        )?;
        if let Some(t) = telemetry {
            t.distribution("known_tests.response_bytes", bytes as f64, &[]);
        }
        let attrs = response
            .pointer("/data/attributes")
            .ok_or("known tests response has no data.attributes")?;
        if let Some(modules) = attrs.get("tests").and_then(Value::as_object) {
            for (module, suites) in modules {
                for (suite, names) in suites.as_object().into_iter().flatten() {
                    for name in names.as_array().into_iter().flatten().filter_map(Value::as_str) {
                        known.insert(TestKey::new(module, suite, name));
                    }
                }
            }
        }
        let has_next = attrs
            .pointer("/page_info/has_next")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let cursor = attrs.pointer("/page_info/cursor").and_then(Value::as_str).unwrap_or("");
        if !has_next || cursor.is_empty() {
            if let Some(t) = telemetry {
                t.distribution("known_tests.response_tests", known.len() as f64, &[]);
            }
            return Ok(known);
        }
        page_state = Some(cursor.to_string());
    }
    Err("known tests pagination did not finish".into())
}

pub fn fetch_test_management(
    transport: &Transport,
    cache: Option<&ReadCache>,
    ctx: &RequestContext,
    telemetry: Option<&Telemetry>,
) -> Result<HashMap<TestKey, TestProperties>, String> {
    let body = json!({
        "data": {
            "id": request_id(),
            "type": "ci_app_libraries_tests_request",
            "attributes": {
                "repository_url": ctx.repository_url,
                "sha": ctx.sha,
                "commit_message": ctx.commit_message,
            }
        }
    });
    let (response, bytes) = read(
        transport,
        cache,
        telemetry,
        "/api/v2/test/libraries/test-management/tests",
        &body,
        (
            "test_management_tests.request",
            "test_management_tests.request_ms",
            "test_management_tests.request_errors",
        ),
    )?;
    let properties = parse_test_management(&response)?;
    if let Some(t) = telemetry {
        t.distribution("test_management_tests.response_bytes", bytes as f64, &[]);
        t.distribution("test_management_tests.response_tests", properties.len() as f64, &[]);
    }
    Ok(properties)
}

pub fn parse_test_management(response: &Value) -> Result<HashMap<TestKey, TestProperties>, String> {
    let modules = response
        .pointer("/data/attributes/modules")
        .and_then(Value::as_object)
        .ok_or("test management response has no data.attributes.modules")?;
    let mut properties = HashMap::new();
    for (module, module_value) in modules {
        let suites = module_value.get("suites").and_then(Value::as_object);
        for (suite, suite_value) in suites.into_iter().flatten() {
            let tests = suite_value.get("tests").and_then(Value::as_object);
            for (name, test_value) in tests.into_iter().flatten() {
                let flag = |key: &str| {
                    test_value
                        .pointer(&format!("/properties/{key}"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                };
                properties.insert(
                    TestKey::new(module, suite, name),
                    TestProperties {
                        quarantined: flag("quarantined"),
                        disabled: flag("disabled"),
                        attempt_to_fix: flag("attempt_to_fix"),
                    },
                );
            }
        }
    }
    Ok(properties)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_settings_response() {
        let response = json!({"data":{"attributes":{"flaky_test_retries_enabled":true,"known_tests_enabled":true,
            "early_flake_detection":{"enabled":true,"slow_test_retries":{"10s":5,"5s":10,"5m":2,"30s":3},
            "faulty_session_threshold":30},"test_management":{"enabled":true,"attempt_to_fix_retries":2}}}});
        let settings = parse_settings(&response).unwrap();
        assert!(settings.flaky_test_retries_enabled && settings.known_tests_enabled);
        assert_eq!(
            settings.early_flake_detection.slow_test_retries,
            vec![
                (Duration::from_secs(5), 10),
                (Duration::from_secs(10), 5),
                (Duration::from_secs(30), 3),
                (Duration::from_secs(300), 2)
            ]
        );
        assert_eq!(settings.test_management.attempt_to_fix_retries, Some(2));
    }

    #[test]
    fn parses_test_management_response() {
        let response = json!({"data":{"attributes":{"modules":{"m":{"suites":{"s":{"tests":{
            "t":{"properties":{"quarantined":true,"attempt_to_fix":true}}}}}}}}}});
        let props = parse_test_management(&response).unwrap();
        assert_eq!(
            props[&TestKey::new("m", "s", "t")],
            TestProperties {
                quarantined: true,
                disabled: false,
                attempt_to_fix: true
            }
        );
    }
}
