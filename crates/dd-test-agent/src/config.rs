// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    /// Empty when `DD_API_KEY` is not set.
    pub api_key: String,
    pub site: String,
    /// Replaces every Datadog host, for testing against a fake intake.
    pub intake_url: Option<String>,
    pub service: Option<String>,
    pub env: Option<String>,
    pub session_name: Option<String>,
    /// `DD_TAGS`, added to every event.
    pub tags: BTreeMap<String, String>,
    pub flaky_retry_enabled: bool,
    pub flaky_retry_count: u32,
    pub total_flaky_retry_count: u32,
    pub early_flake_detection_enabled: bool,
    pub test_management_enabled: bool,
    pub attempt_to_fix_retries: Option<u32>,
    pub debug: bool,
    pub telemetry_enabled: bool,
    pub telemetry_url: Option<String>,
}

impl Config {
    pub fn from_env() -> Config {
        Config::from_lookup(|key| env::var(key).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Config {
        let non_empty = |key: &str| get(key).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let flag = |key: &str, default: bool| non_empty(key).and_then(|v| parse_bool(&v)).unwrap_or(default);
        let count = |key: &str| non_empty(key).and_then(|v| v.parse::<u32>().ok());

        Config {
            api_key: non_empty("DD_API_KEY").unwrap_or_default(),
            site: non_empty("DD_SITE").unwrap_or_else(|| "datadoghq.com".to_string()),
            intake_url: non_empty("DD_CIVISIBILITY_AGENTLESS_URL").map(|u| u.trim_end_matches('/').to_string()),
            service: non_empty("DD_SERVICE"),
            env: non_empty("DD_ENV"),
            session_name: non_empty("DD_TEST_SESSION_NAME"),
            tags: non_empty("DD_TAGS").map(|v| parse_tags(&v)).unwrap_or_default(),
            flaky_retry_enabled: flag("DD_CIVISIBILITY_FLAKY_RETRY_ENABLED", true),
            flaky_retry_count: count("DD_CIVISIBILITY_FLAKY_RETRY_COUNT").unwrap_or(5),
            total_flaky_retry_count: count("DD_CIVISIBILITY_TOTAL_FLAKY_RETRY_COUNT").unwrap_or(1000),
            early_flake_detection_enabled: flag("DD_CIVISIBILITY_EARLY_FLAKE_DETECTION_ENABLED", true),
            test_management_enabled: flag("DD_TEST_MANAGEMENT_ENABLED", true),
            attempt_to_fix_retries: count("DD_TEST_MANAGEMENT_ATTEMPT_TO_FIX_RETRIES").filter(|n| *n > 0),
            debug: flag("DD_TRACE_DEBUG", false),
            telemetry_enabled: flag("DD_INSTRUMENTATION_TELEMETRY_ENABLED", true),
            telemetry_url: non_empty("DD_INSTRUMENTATION_TELEMETRY_URL").map(|u| u.trim_end_matches('/').to_string()),
        }
    }
}

/// `key:value` pairs separated by commas, or by spaces when there is no comma.
fn parse_tags(value: &str) -> BTreeMap<String, String> {
    let separator = if value.contains(',') { ',' } else { ' ' };
    value
        .split(separator)
        .filter_map(|pair| {
            let (key, value) = pair.split_once(':').unwrap_or((pair, ""));
            let key = key.trim();
            (!key.is_empty()).then(|| (key.to_string(), value.trim().to_string()))
        })
        .collect()
}

pub fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" => Some(true),
        "0" | "false" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn config(vars: &[(&str, &str)]) -> Config {
        let map: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Config::from_lookup(|k| map.get(k).cloned())
    }

    #[test]
    fn defaults() {
        let c = config(&[]);
        assert!(c.flaky_retry_enabled && c.test_management_enabled);
        assert_eq!((c.flaky_retry_count, c.total_flaky_retry_count), (5, 1000));
        assert_eq!(
            (c.api_key.as_str(), c.site.as_str(), c.intake_url),
            ("", "datadoghq.com", None)
        );
    }

    #[test]
    fn reads_the_intake_and_feature_switches() {
        let c = config(&[
            ("DD_API_KEY", "key"),
            ("DD_CIVISIBILITY_AGENTLESS_URL", "http://127.0.0.1:5259/"),
            ("DD_CIVISIBILITY_FLAKY_RETRY_ENABLED", "false"),
            ("DD_CIVISIBILITY_FLAKY_RETRY_COUNT", "-3"),
            ("DD_TEST_MANAGEMENT_ATTEMPT_TO_FIX_RETRIES", "0"),
        ]);
        assert!(!c.flaky_retry_enabled);
        assert_eq!(c.flaky_retry_count, 5);
        assert_eq!(c.attempt_to_fix_retries, None);
        assert_eq!(c.api_key, "key");
        assert_eq!(c.intake_url.as_deref(), Some("http://127.0.0.1:5259"));
    }

    #[test]
    fn parses_tags_like_other_libraries() {
        let tags = |value: &str| config(&[("DD_TAGS", value)]).tags.into_iter().collect::<Vec<_>>();
        let pair = |k: &str, v: &str| (k.to_string(), v.to_string());
        assert_eq!(tags("team:ci owner:me"), [pair("owner", "me"), pair("team", "ci")]);
        assert_eq!(
            tags("team:ci, url:http://x:1 ,flag,:skipped"),
            [pair("flag", ""), pair("team", "ci"), pair("url", "http://x:1")]
        );
        assert!(config(&[]).tags.is_empty());
    }
}
