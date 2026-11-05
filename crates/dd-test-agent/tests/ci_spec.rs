// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use dd_test_agent::ci::{provider_tags, Env};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

struct MapEnv(BTreeMap<String, String>);

impl Env for MapEnv {
    fn get(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned()
    }

    fn vars(&self) -> Vec<(String, String)> {
        self.0.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }
}

fn sort_json(value: &str) -> String {
    serde_json::from_str::<Value>(value)
        .map(|v| v.to_string())
        .unwrap_or_else(|_| value.to_string())
}

#[test]
fn matches_ci_spec_fixtures() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ci");
    let mut failures = Vec::new();
    let mut checked = 0;
    let mut entries: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
    entries.sort();
    for path in entries {
        let provider = path.file_stem().unwrap().to_string_lossy().to_string();
        let cases: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        for (index, case) in cases.as_array().unwrap().iter().enumerate() {
            let pair = case.as_array().unwrap();
            let env: BTreeMap<String, String> = pair[0]
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()),
                    )
                })
                .collect();
            let actual = provider_tags(&MapEnv(env));
            for (key, expected) in pair[1].as_object().unwrap() {
                let expected = match expected {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let actual_value = actual.get(key).cloned();
                let matches = match (key.as_str(), &actual_value) {
                    ("_dd.ci.env_vars", Some(a)) => sort_json(a) == sort_json(&expected),
                    (_, Some(a)) => *a == expected,
                    (_, None) => false,
                };
                checked += 1;
                if !matches {
                    failures.push(format!(
                        "{provider}[{index}] {key}: expected {expected:?}, got {actual_value:?}"
                    ));
                }
            }
        }
    }
    assert!(checked > 1000, "only {checked} expectations checked");
    assert!(
        failures.is_empty(),
        "{} mismatches:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
