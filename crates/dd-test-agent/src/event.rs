// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Test,
    Suite,
    Module,
    Session,
}

impl Kind {
    pub fn event_type(self) -> &'static str {
        match self {
            Kind::Test => "test",
            Kind::Suite => "test_suite_end",
            Kind::Module => "test_module_end",
            Kind::Session => "test_session_end",
        }
    }

    fn version(self) -> u32 {
        match self {
            Kind::Test => 2,
            _ => 1,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Span {
    pub kind: Kind,
    pub trace_id: u64,
    pub span_id: u64,
    pub session_id: u64,
    pub module_id: u64,
    pub suite_id: u64,
    pub name: String,
    pub resource: String,
    pub service: String,
    pub start: i64,
    pub duration: i64,
    pub error: bool,
    pub meta: BTreeMap<String, String>,
    pub metrics: BTreeMap<String, f64>,
}

impl Span {
    pub fn new(kind: Kind, name: &str, resource: &str, service: &str, start: i64) -> Span {
        Span {
            kind,
            trace_id: 0,
            span_id: 0,
            session_id: 0,
            module_id: 0,
            suite_id: 0,
            name: name.to_string(),
            resource: resource.to_string(),
            service: service.to_string(),
            start,
            duration: 0,
            error: false,
            meta: BTreeMap::new(),
            metrics: BTreeMap::new(),
        }
    }

    pub fn set(&mut self, key: &str, value: impl Into<String>) {
        self.meta.insert(key.to_string(), value.into());
    }

    pub fn set_metric(&mut self, key: &str, value: f64) {
        self.metrics.insert(key.to_string(), value);
    }

    fn event(&self) -> Event<'_> {
        let test = self.kind == Kind::Test;
        Event {
            kind: self.kind.event_type(),
            version: self.kind.version(),
            content: Content {
                trace_id: test.then_some(self.trace_id),
                span_id: test.then_some(self.span_id),
                parent_id: test.then_some(0),
                test_session_id: self.session_id,
                test_module_id: (self.kind != Kind::Session).then_some(self.module_id),
                test_suite_id: matches!(self.kind, Kind::Test | Kind::Suite).then_some(self.suite_id),
                kind: self.kind.event_type(),
                name: &self.name,
                resource: &self.resource,
                service: &self.service,
                start: self.start,
                duration: self.duration,
                error: self.error as u8,
                meta: &self.meta,
                metrics: &self.metrics,
            },
        }
    }
}

#[derive(Serialize)]
struct Event<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    version: u32,
    content: Content<'a>,
}

#[derive(Serialize)]
struct Content<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    trace_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_id: Option<u64>,
    test_session_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    test_module_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    test_suite_id: Option<u64>,
    #[serde(rename = "type")]
    kind: &'static str,
    name: &'a str,
    resource: &'a str,
    service: &'a str,
    start: i64,
    duration: i64,
    error: u8,
    meta: &'a BTreeMap<String, String>,
    metrics: &'a BTreeMap<String, f64>,
}

pub struct Metadata {
    pub runtime_id: String,
    pub library_version: String,
    pub env: String,
    pub session_name: String,
}

#[derive(Serialize)]
struct Payload<'a> {
    version: u32,
    metadata: BTreeMap<&'a str, BTreeMap<&'a str, &'a str>>,
    events: Vec<Event<'a>>,
}

/// A citestcycle MessagePack payload.
pub fn encode_payload(metadata: &Metadata, spans: &[Span]) -> Vec<u8> {
    let mut all = BTreeMap::new();
    all.insert("language", "rust");
    all.insert("runtime-id", metadata.runtime_id.as_str());
    all.insert("library_version", metadata.library_version.as_str());
    all.insert("env", metadata.env.as_str());
    let mut sections = BTreeMap::new();
    sections.insert("*", all);
    for kind in [Kind::Session, Kind::Module, Kind::Suite, Kind::Test] {
        sections.insert(
            kind.event_type(),
            BTreeMap::from([("test_session.name", metadata.session_name.as_str())]),
        );
    }
    let payload = Payload {
        version: 1,
        metadata: sections,
        events: spans.iter().map(Span::event).collect(),
    };
    rmp_serde::to_vec_named(&payload).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_ids_by_event_type() {
        let mut test = Span::new(Kind::Test, "libtest.test", "s.t", "svc", 1);
        test.trace_id = 7;
        let suite = Span::new(Kind::Suite, "libtest.test_suite", "m.s", "svc", 1);
        let metadata = Metadata {
            runtime_id: "r".into(),
            library_version: "0".into(),
            env: "ci".into(),
            session_name: "unit".into(),
        };
        let payload: serde_json::Value = rmp_serde::from_slice(&encode_payload(&metadata, &[test, suite])).unwrap();
        assert_eq!(payload["metadata"]["*"]["language"], "rust");
        assert_eq!(payload["metadata"]["test"]["test_session.name"], "unit");
        assert_eq!(payload["events"][0]["content"]["trace_id"], 7);
        assert_eq!(payload["events"][0]["version"], 2);
        assert!(payload["events"][1]["content"].get("trace_id").is_none());
        assert!(payload["events"][1]["content"].get("test_suite_id").is_some());
    }
}
