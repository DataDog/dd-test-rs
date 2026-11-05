// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Messages between the harness, inside the test binary, and the agent process.
//!
//! One JSON object per line. The harness sends `hello`, then `test` records as
//! executions finish, then `finish`. The agent answers `hello` with `bootstrap`
//! and `finish` with `finished`. When `bootstrap` comes too late, the harness
//! runs the tests without Datadog and sends `cancel` instead.

use crate::json::{self, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

pub const VERSION: u32 = 1;

/// Identifies a test within the module the agent session reports.
pub type TestId = (String, String);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    pub module: String,
    pub crate_name: String,
    pub binary: String,
    pub rustc_version: String,
    pub library_version: String,
    pub command: String,
    pub working_dir: String,
    /// The process that started the test binary, such as cargo.
    pub parent_pid: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Bootstrap {
    pub repo_root: Option<String>,
    pub auto_test_retries: Option<AutoTestRetries>,
    pub early_flake_detection: Option<EarlyFlakeDetection>,
    pub test_management: Option<TestManagement>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AutoTestRetries {
    pub retries: u32,
    pub budget: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EarlyFlakeDetection {
    /// Duration upper bounds and the total number of executions, sorted by bound.
    pub buckets: Vec<(Duration, u32)>,
    pub faulty_session_threshold: f64,
    pub known_tests: HashSet<TestId>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TestManagement {
    pub attempt_to_fix_retries: u32,
    pub tests: HashMap<TestId, TestProperties>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TestProperties {
    pub quarantined: bool,
    pub disabled: bool,
    pub attempt_to_fix: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    Skip,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pass => "pass",
            Status::Fail => "fail",
            Status::Skip => "skip",
        }
    }

    pub fn parse(value: &str) -> Option<Status> {
        match value {
            "pass" => Some(Status::Pass),
            "fail" => Some(Status::Fail),
            "skip" => Some(Status::Skip),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorInfo {
    pub kind: String,
    pub message: String,
    pub stack: Option<String>,
}

/// One test execution. `tags` holds the feature tags the engine decided,
/// including `test.status`. `outcome` is set on the last execution of a test
/// and is the result suites, module and session aggregate.
#[derive(Clone, Debug, PartialEq)]
pub struct TestRecord {
    pub trace_id: u64,
    pub span_id: u64,
    pub suite: String,
    pub name: String,
    pub source_file: Option<String>,
    pub start_line: u32,
    pub end_line: u32,
    pub start: i64,
    pub duration: i64,
    pub tags: BTreeMap<String, String>,
    pub error: Option<ErrorInfo>,
    pub outcome: Option<Status>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Finish {
    pub efd_faulty: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    Hello(Hello),
    Bootstrap(Bootstrap),
    Test(TestRecord),
    Finish(Finish),
    Finished,
    Cancel,
    Error(String),
}

fn s(value: &str) -> Value {
    Value::String(value.to_string())
}

fn n(value: f64) -> Value {
    Value::Number(value)
}

fn pair((suite, name): &TestId) -> Value {
    Value::Array(vec![s(suite), s(name)])
}

impl Message {
    pub fn encode(&self) -> String {
        let (kind, body): (&str, Vec<(&str, Value)>) = match self {
            Message::Hello(h) => (
                "hello",
                vec![
                    ("version", n(VERSION as f64)),
                    ("module", s(&h.module)),
                    ("crate_name", s(&h.crate_name)),
                    ("binary", s(&h.binary)),
                    ("rustc_version", s(&h.rustc_version)),
                    ("library_version", s(&h.library_version)),
                    ("command", s(&h.command)),
                    ("working_dir", s(&h.working_dir)),
                ]
                .into_iter()
                .chain(h.parent_pid.map(|pid| ("parent_pid", n(pid as f64))))
                .collect(),
            ),
            Message::Bootstrap(b) => {
                let mut body = vec![("version", n(VERSION as f64))];
                if let Some(root) = &b.repo_root {
                    body.push(("repo_root", s(root)));
                }
                if let Some(atr) = b.auto_test_retries {
                    body.push((
                        "auto_test_retries",
                        Value::object([("retries", n(atr.retries as f64)), ("budget", n(atr.budget as f64))]),
                    ));
                }
                if let Some(efd) = &b.early_flake_detection {
                    let buckets = efd
                        .buckets
                        .iter()
                        .map(|(d, e)| Value::Array(vec![n(d.as_secs_f64()), n(*e as f64)]))
                        .collect();
                    let mut known: Vec<&TestId> = efd.known_tests.iter().collect();
                    known.sort();
                    body.push((
                        "early_flake_detection",
                        Value::object([
                            ("buckets", Value::Array(buckets)),
                            ("faulty_session_threshold", n(efd.faulty_session_threshold)),
                            ("known_tests", Value::Array(known.into_iter().map(pair).collect())),
                        ]),
                    ));
                }
                if let Some(tm) = &b.test_management {
                    let mut tests: Vec<(&TestId, &TestProperties)> = tm.tests.iter().collect();
                    tests.sort_by(|a, b| a.0.cmp(b.0));
                    let tests = tests
                        .into_iter()
                        .map(|(id, p)| {
                            Value::object([
                                ("test", pair(id)),
                                ("quarantined", Value::Bool(p.quarantined)),
                                ("disabled", Value::Bool(p.disabled)),
                                ("attempt_to_fix", Value::Bool(p.attempt_to_fix)),
                            ])
                        })
                        .collect();
                    body.push((
                        "test_management",
                        Value::object([
                            ("attempt_to_fix_retries", n(tm.attempt_to_fix_retries as f64)),
                            ("tests", Value::Array(tests)),
                        ]),
                    ));
                }
                ("bootstrap", body)
            }
            Message::Test(t) => {
                let mut body = vec![
                    ("trace_id", s(&t.trace_id.to_string())),
                    ("span_id", s(&t.span_id.to_string())),
                    ("suite", s(&t.suite)),
                    ("name", s(&t.name)),
                    ("start_line", n(t.start_line as f64)),
                    ("end_line", n(t.end_line as f64)),
                    ("start", s(&t.start.to_string())),
                    ("duration", s(&t.duration.to_string())),
                    ("tags", Value::object(t.tags.iter().map(|(k, v)| (k.clone(), s(v))))),
                ];
                if let Some(file) = &t.source_file {
                    body.push(("source_file", s(file)));
                }
                if let Some(error) = &t.error {
                    let mut e = vec![("type", s(&error.kind)), ("message", s(&error.message))];
                    if let Some(stack) = &error.stack {
                        e.push(("stack", s(stack)));
                    }
                    body.push(("error", Value::object(e)));
                }
                if let Some(outcome) = t.outcome {
                    body.push(("outcome", s(outcome.as_str())));
                }
                ("test", body)
            }
            Message::Finish(f) => ("finish", vec![("efd_faulty", Value::Bool(f.efd_faulty))]),
            Message::Finished => ("finished", vec![]),
            Message::Cancel => ("cancel", vec![]),
            Message::Error(message) => ("error", vec![("message", s(message))]),
        };
        let mut entries: Vec<(&str, Value)> = vec![("type", s(kind))];
        entries.extend(body);
        Value::object(entries).to_json()
    }

    pub fn decode(line: &str) -> Result<Message, String> {
        let value = json::parse(line.trim())?;
        let str_field = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
        let req = |key: &str| str_field(key).ok_or_else(|| format!("missing {key}"));
        let num = |v: &Value, key: &str| v.get(key).and_then(Value::as_f64).unwrap_or(0.0);
        let kind = req("type")?;
        let check_version = || match value.get("version").and_then(Value::as_f64) {
            Some(v) if v as u32 == VERSION => Ok(()),
            other => Err(format!("unsupported protocol version {other:?}, expected {VERSION}")),
        };
        Ok(match kind.as_str() {
            "hello" => {
                check_version()?;
                Message::Hello(Hello {
                    module: req("module")?,
                    crate_name: req("crate_name")?,
                    binary: req("binary")?,
                    rustc_version: req("rustc_version")?,
                    library_version: req("library_version")?,
                    command: req("command")?,
                    working_dir: req("working_dir")?,
                    parent_pid: value.get("parent_pid").and_then(Value::as_f64).map(|pid| pid as u32),
                })
            }
            "bootstrap" => {
                check_version()?;
                let atr = value.get("auto_test_retries").map(|a| AutoTestRetries {
                    retries: num(a, "retries") as u32,
                    budget: num(a, "budget") as u32,
                });
                let efd = value.get("early_flake_detection").map(|e| EarlyFlakeDetection {
                    buckets: e
                        .get("buckets")
                        .and_then(Value::as_array)
                        .unwrap_or(&[])
                        .iter()
                        .filter_map(|b| {
                            let b = b.as_array()?;
                            Some((
                                Duration::from_secs_f64(b.first()?.as_f64()?),
                                b.get(1)?.as_f64()? as u32,
                            ))
                        })
                        .collect(),
                    faulty_session_threshold: num(e, "faulty_session_threshold"),
                    known_tests: e
                        .get("known_tests")
                        .and_then(Value::as_array)
                        .unwrap_or(&[])
                        .iter()
                        .filter_map(decode_pair)
                        .collect(),
                });
                let tm = value.get("test_management").map(|t| TestManagement {
                    attempt_to_fix_retries: num(t, "attempt_to_fix_retries") as u32,
                    tests: t
                        .get("tests")
                        .and_then(Value::as_array)
                        .unwrap_or(&[])
                        .iter()
                        .filter_map(|entry| {
                            let flag = |key: &str| entry.get(key).and_then(Value::as_bool).unwrap_or(false);
                            Some((
                                decode_pair(entry.get("test")?)?,
                                TestProperties {
                                    quarantined: flag("quarantined"),
                                    disabled: flag("disabled"),
                                    attempt_to_fix: flag("attempt_to_fix"),
                                },
                            ))
                        })
                        .collect(),
                });
                Message::Bootstrap(Bootstrap {
                    repo_root: str_field("repo_root"),
                    auto_test_retries: atr,
                    early_flake_detection: efd,
                    test_management: tm,
                })
            }
            "test" => {
                let error = value.get("error").map(|e| ErrorInfo {
                    kind: e.get("type").and_then(Value::as_str).unwrap_or("").to_string(),
                    message: e.get("message").and_then(Value::as_str).unwrap_or("").to_string(),
                    stack: e.get("stack").and_then(Value::as_str).map(str::to_string),
                });
                Message::Test(TestRecord {
                    trace_id: parsed(&value, "trace_id"),
                    span_id: parsed(&value, "span_id"),
                    suite: req("suite")?,
                    name: req("name")?,
                    source_file: str_field("source_file"),
                    start_line: num(&value, "start_line") as u32,
                    end_line: num(&value, "end_line") as u32,
                    start: parsed(&value, "start"),
                    duration: parsed(&value, "duration"),
                    tags: string_map(value.get("tags")),
                    error,
                    outcome: str_field("outcome").as_deref().and_then(Status::parse),
                })
            }
            "finish" => Message::Finish(Finish {
                efd_faulty: value.get("efd_faulty").and_then(Value::as_bool).unwrap_or(false),
            }),
            "finished" => Message::Finished,
            "cancel" => Message::Cancel,
            "error" => Message::Error(str_field("message").unwrap_or_default()),
            other => return Err(format!("unknown message type {other:?}")),
        })
    }
}

/// Integers that can exceed 2^53 travel as strings.
fn parsed<T: std::str::FromStr + Default>(value: &Value, key: &str) -> T {
    value
        .get(key)
        .and_then(Value::as_str)
        .and_then(|v| v.parse().ok())
        .unwrap_or_default()
}

fn string_map(value: Option<&Value>) -> BTreeMap<String, String> {
    value
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

fn decode_pair(value: &Value) -> Option<TestId> {
    let items = value.as_array()?;
    Some((
        items.first()?.as_str()?.to_string(),
        items.get(1)?.as_str()?.to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(message: Message) {
        let line = message.encode();
        assert!(!line.contains('\n'), "{line}");
        assert_eq!(Message::decode(&line).unwrap(), message);
    }

    #[test]
    fn round_trips_every_message() {
        round_trip(Message::Hello(Hello {
            module: "bytes".into(),
            crate_name: "test_buf".into(),
            binary: "/t/test_buf-0123".into(),
            rustc_version: "1.98.1".into(),
            library_version: "0.1.0".into(),
            command: "cargo test".into(),
            working_dir: "/repo".into(),
            parent_pid: Some(4242),
        }));
        round_trip(Message::Bootstrap(Bootstrap {
            repo_root: Some("/repo".into()),
            auto_test_retries: Some(AutoTestRetries {
                retries: 5,
                budget: 1000,
            }),
            early_flake_detection: Some(EarlyFlakeDetection {
                buckets: vec![(Duration::from_secs(5), 10), (Duration::from_secs(300), 2)],
                faulty_session_threshold: 30.0,
                known_tests: [("s".to_string(), "t".to_string())].into_iter().collect(),
            }),
            test_management: Some(TestManagement {
                attempt_to_fix_retries: 20,
                tests: [(
                    ("s".to_string(), "q".to_string()),
                    TestProperties {
                        quarantined: true,
                        disabled: false,
                        attempt_to_fix: true,
                    },
                )]
                .into_iter()
                .collect(),
            }),
        }));
        round_trip(Message::Bootstrap(Bootstrap::default()));
        round_trip(Message::Test(TestRecord {
            trace_id: u64::MAX - 1,
            span_id: 9_007_199_254_740_993,
            suite: "bytes::tests".into(),
            name: "it".into(),
            source_file: Some("src/lib.rs".into()),
            start_line: 3,
            end_line: 9,
            start: 1_790_000_000_123_456_789,
            duration: 42,
            tags: [("test.status".to_string(), "fail".to_string())].into_iter().collect(),
            error: Some(ErrorInfo {
                kind: "panic".into(),
                message: "a\nb".into(),
                stack: Some("trace".into()),
            }),
            outcome: Some(Status::Fail),
        }));
        round_trip(Message::Finish(Finish { efd_faulty: true }));
        round_trip(Message::Finished);
        round_trip(Message::Cancel);
        round_trip(Message::Error("bad".into()));
    }

    #[test]
    fn rejects_other_versions() {
        let line = Message::Finished.encode().replace("finished", "hello");
        assert!(Message::decode(&line).unwrap_err().contains("version"));
    }
}
