// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use crate::backend::Backend;
use crate::ids::{new_id, now_nanos};
use crate::panic as panics;
use crate::protocol::{
    AutoTestRetries, Bootstrap, EarlyFlakeDetection, ErrorInfo, Finish, Hello, Status, TestId, TestManagement,
    TestProperties, TestRecord,
};
use std::any::Any;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const DISABLED_SKIP_REASON: &str = "Flaky test is disabled by Datadog";
const EFD_MIN_NEW_TESTS_FOR_FAULTY: usize = 30;

pub type Payload = Box<dyn Any + Send>;
/// Result of one call of a libtest test function, as seen by `catch_unwind`.
pub type RawResult = Result<Result<(), String>, Payload>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShouldPanic {
    No,
    Yes,
    YesWithMessage(String),
}

#[derive(Clone, Debug)]
pub struct TestInfo {
    pub suite: String,
    pub name: String,
    pub source_file: Option<String>,
    pub start_line: u32,
    pub end_line: u32,
    pub should_panic: ShouldPanic,
    /// libtest will not run the test in this invocation.
    pub ignored: bool,
    pub ignore_message: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Run,
    Skip { reason: String, disabled: bool },
}

#[derive(Clone, Debug)]
pub struct TestPlan {
    pub id: TestId,
    pub is_new: bool,
    pub properties: TestProperties,
    pub action: Action,
}

#[derive(Default)]
struct Summary {
    retries: usize,
    quarantined_failures: usize,
    disabled: usize,
    attempt_to_fix_failures: usize,
}

pub struct Session {
    backend: Box<dyn Backend>,
    repo_root: Option<PathBuf>,
    auto_test_retries: Option<AutoTestRetries>,
    atr_admitted: AtomicU32,
    efd: Option<EarlyFlakeDetection>,
    efd_faulty: AtomicBool,
    test_management: Option<TestManagement>,
    pending: AtomicUsize,
    finished: AtomicBool,
    summary: Mutex<Summary>,
}

impl Session {
    pub fn start(hello: &Hello, backend: Box<dyn Backend>) -> Result<Session, String> {
        let bootstrap: Bootstrap = backend.bootstrap(hello)?;
        debug!("bootstrap: {bootstrap:?}");
        panics::install_hook();
        Ok(Session {
            repo_root: bootstrap.repo_root.map(PathBuf::from),
            atr_admitted: AtomicU32::new(0),
            auto_test_retries: bootstrap.auto_test_retries,
            efd: bootstrap.early_flake_detection,
            efd_faulty: AtomicBool::new(false),
            test_management: bootstrap.test_management,
            pending: AtomicUsize::new(0),
            finished: AtomicBool::new(false),
            summary: Mutex::new(Summary::default()),
            backend,
        })
    }

    pub fn repo_root(&self) -> Option<&std::path::Path> {
        self.repo_root.as_deref()
    }

    /// Decides how every selected test runs. `tests` must be the tests selected
    /// by the invocation's filters, including ignored ones.
    pub fn plan(&self, tests: &[TestInfo]) -> Vec<TestPlan> {
        let ids: Vec<TestId> = tests.iter().map(|t| (t.suite.clone(), t.name.clone())).collect();
        let mut new_tests: Vec<bool> = match &self.efd {
            Some(efd) => ids.iter().map(|id| !efd.known_tests.contains(id)).collect(),
            None => vec![false; ids.len()],
        };
        if let Some(efd) = &self.efd {
            let new_count = new_tests.iter().filter(|n| **n).count();
            let percentage = new_count as f64 * 100.0 / ids.len().max(1) as f64;
            if new_count > EFD_MIN_NEW_TESTS_FOR_FAULTY && percentage > efd.faulty_session_threshold {
                debug!("early flake detection aborted: {new_count} new tests ({percentage:.1}%)");
                self.efd_faulty.store(true, Ordering::Relaxed);
                new_tests.iter_mut().for_each(|n| *n = false);
            }
        }
        let plans: Vec<TestPlan> = tests
            .iter()
            .zip(ids)
            .zip(new_tests)
            .map(|((test, id), is_new)| {
                let properties = match &self.test_management {
                    Some(tm) if !is_new => tm.tests.get(&id).copied().unwrap_or_default(),
                    _ => TestProperties::default(),
                };
                let action = if test.ignored {
                    let reason = test.ignore_message.clone().unwrap_or_else(|| "ignored".to_string());
                    Action::Skip {
                        reason,
                        disabled: false,
                    }
                } else if properties.disabled && !properties.attempt_to_fix {
                    Action::Skip {
                        reason: DISABLED_SKIP_REASON.to_string(),
                        disabled: true,
                    }
                } else {
                    Action::Run
                };
                TestPlan {
                    id,
                    is_new,
                    properties,
                    action,
                }
            })
            .collect();
        let runnable = plans.iter().filter(|p| p.action == Action::Run).count();
        self.pending.store(runnable, Ordering::SeqCst);
        plans
    }

    /// Number of planned tests that have not finished running yet.
    pub fn pending(&self) -> usize {
        self.pending.load(Ordering::SeqCst)
    }

    /// Marks one planned test as finished. Returns true for the last one.
    pub fn complete_one(&self) -> bool {
        self.pending.fetch_sub(1, Ordering::SeqCst) == 1
    }

    pub fn report_skip(&self, test: &TestInfo, plan: &TestPlan) {
        let Action::Skip { reason, disabled } = &plan.action else {
            return;
        };
        let mut record = self.record(test, plan, now_nanos());
        record.tags.insert("test.status".into(), "skip".into());
        record.tags.insert("test.skip_reason".into(), reason.clone());
        record.tags.insert("test.final_status".into(), "skip".into());
        record.outcome = Some(Status::Skip);
        if *disabled {
            self.summary.lock().unwrap_or_else(|e| e.into_inner()).disabled += 1;
        }
        self.backend.send(record);
    }

    /// Runs a planned test with every execution its features require and returns
    /// the result libtest should see.
    pub fn run(
        &self,
        test: &TestInfo,
        plan: &TestPlan,
        retryable: bool,
        attempt: &mut dyn FnMut() -> RawResult,
    ) -> RawResult {
        let props = plan.properties;
        let mode = match &self.test_management {
            Some(tm) if props.attempt_to_fix => Mode::AttemptToFix(tm.attempt_to_fix_retries),
            _ if plan.is_new => Mode::EarlyFlakeDetection,
            _ => Mode::Normal,
        };
        let quarantined = props.quarantined && !matches!(mode, Mode::AttemptToFix(_));

        let mut executions = 0u32;
        let mut any_pass = false;
        let mut any_fail = false;
        let mut efd_max_executions = 0u32;
        let mut atr_admitted = false;
        loop {
            panics::clear();
            let ids = (new_id(), new_id());
            let start = now_nanos();
            let clock = Instant::now();
            let raw = attempt();
            let duration = clock.elapsed();
            let failure = classify(&test.should_panic, &raw);
            let passed = failure.is_none();
            executions += 1;
            any_pass |= passed;
            any_fail |= !passed;

            let retry = retryable
                && match mode {
                    Mode::AttemptToFix(retries) => passed && executions <= retries,
                    Mode::EarlyFlakeDetection => {
                        if executions == 1 {
                            efd_max_executions = self.efd_executions(duration);
                        }
                        executions < efd_max_executions && !(any_pass && any_fail)
                    }
                    Mode::Normal => match self.auto_test_retries {
                        Some(atr) if !passed => {
                            if executions == 1 {
                                atr_admitted = self.admit_auto_test_retry(atr.budget);
                            }
                            atr_admitted && executions <= atr.retries
                        }
                        _ => false,
                    },
                };

            let mut record = self.record(test, plan, start);
            (record.trace_id, record.span_id) = ids;
            record.duration = duration.as_nanos() as i64;
            let tags = &mut record.tags;
            tags.insert("test.status".into(), if passed { "pass" } else { "fail" }.into());
            if executions > 1 {
                tags.insert("test.is_retry".into(), "true".into());
                tags.insert("test.retry_reason".into(), mode.retry_reason().into());
            }
            if mode == Mode::EarlyFlakeDetection && executions == 1 && efd_max_executions == 0 {
                tags.insert("test.early_flake.abort_reason".into(), "slow".into());
            }
            if let Some(failure) = failure {
                if quarantined {
                    tags.insert("test.failure_suppression_reason".into(), "quarantine".into());
                } else if retry && mode == Mode::Normal {
                    tags.insert("test.failure_suppression_reason".into(), "auto_test_retry".into());
                }
                record.error = Some(error_info(failure, &raw));
            }

            let verdict = match mode {
                Mode::AttemptToFix(_) => !any_fail,
                _ => any_pass || quarantined,
            };
            if !retry {
                let tags = &mut record.tags;
                if executions > 1 && !any_pass {
                    tags.insert("test.has_failed_all_retries".into(), "true".into());
                }
                if matches!(mode, Mode::AttemptToFix(_)) {
                    tags.insert(
                        "test.test_management.attempt_to_fix_passed".into(),
                        (!any_fail).to_string(),
                    );
                }
                let outcome = if verdict { Status::Pass } else { Status::Fail };
                tags.insert("test.final_status".into(), outcome.as_str().into());
                record.outcome = Some(outcome);
            }

            {
                let mut summary = self.summary.lock().unwrap_or_else(|e| e.into_inner());
                if executions > 1 {
                    summary.retries += 1;
                }
                if !retry && quarantined && any_fail && !any_pass {
                    summary.quarantined_failures += 1;
                }
                if !retry && matches!(mode, Mode::AttemptToFix(_)) && any_fail {
                    summary.attempt_to_fix_failures += 1;
                }
            }
            self.backend.send(record);

            if !retry {
                return libtest_result(verdict, &test.should_panic, raw);
            }
        }
    }

    fn efd_executions(&self, duration: Duration) -> u32 {
        let Some(efd) = &self.efd else { return 0 };
        efd.buckets
            .iter()
            .find(|(bound, _)| duration < *bound)
            .map_or(0, |(_, executions)| *executions)
    }

    fn admit_auto_test_retry(&self, budget: u32) -> bool {
        self.atr_admitted.fetch_add(1, Ordering::SeqCst) < budget
    }

    fn record(&self, test: &TestInfo, plan: &TestPlan, start: i64) -> TestRecord {
        let mut tags = BTreeMap::new();
        if plan.is_new {
            tags.insert("test.is_new".to_string(), "true".to_string());
        }
        let props = plan.properties;
        if props.quarantined {
            tags.insert("test.test_management.is_quarantined".to_string(), "true".to_string());
        }
        if props.disabled {
            tags.insert("test.test_management.is_test_disabled".to_string(), "true".to_string());
        }
        if props.attempt_to_fix {
            tags.insert("test.test_management.is_attempt_to_fix".to_string(), "true".to_string());
        }
        TestRecord {
            trace_id: new_id(),
            span_id: new_id(),
            suite: test.suite.clone(),
            name: test.name.clone(),
            source_file: test.source_file.clone(),
            start_line: test.start_line,
            end_line: test.end_line,
            start,
            duration: 0,
            tags,
            error: None,
            outcome: None,
        }
    }

    /// Waits until the agent has handled every result. Only the first call has an effect.
    pub fn finish(&self) {
        if self.finished.swap(true, Ordering::SeqCst) {
            return;
        }
        self.backend.finish(Finish {
            efd_faulty: self.efd_faulty.load(Ordering::Relaxed),
        });
        print_summary(&self.summary.lock().unwrap_or_else(|e| e.into_inner()));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Normal,
    EarlyFlakeDetection,
    AttemptToFix(u32),
}

impl Mode {
    fn retry_reason(self) -> &'static str {
        match self {
            Mode::Normal => "auto_test_retry",
            Mode::EarlyFlakeDetection => "early_flake_detection",
            Mode::AttemptToFix(_) => "attempt_to_fix",
        }
    }
}

/// Why an execution failed, mirroring libtest's own result calculation.
#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    Panicked,
    ReturnedError,
    DidNotPanic,
    WrongPanicMessage(String),
}

fn payload(raw: &RawResult) -> Option<&(dyn Any + Send)> {
    match raw {
        Ok(Ok(())) => None,
        Ok(Err(message)) => Some(message as &(dyn Any + Send)),
        Err(payload) => Some(payload.as_ref()),
    }
}

pub fn classify(should_panic: &ShouldPanic, raw: &RawResult) -> Option<Failure> {
    match (should_panic, payload(raw)) {
        (ShouldPanic::No, None) | (ShouldPanic::Yes, Some(_)) => None,
        (ShouldPanic::YesWithMessage(expected), Some(payload)) => match panics::payload_str(payload) {
            Some(message) if message.contains(expected.as_str()) => None,
            Some(message) => Some(Failure::WrongPanicMessage(format!(
                "panic did not contain expected string\n      panic message: {message:?}\n expected substring: {expected:?}"
            ))),
            None => Some(Failure::WrongPanicMessage(format!(
                "expected panic with string value,\n found non-string value\n     expected substring: {expected:?}"
            ))),
        },
        (_, None) => Some(Failure::DidNotPanic),
        (ShouldPanic::No, Some(_)) => Some(if matches!(raw, Ok(Err(_))) {
            Failure::ReturnedError
        } else {
            Failure::Panicked
        }),
    }
}

/// The result to hand back to libtest so that it reports `pass` as the outcome.
pub fn libtest_result(pass: bool, should_panic: &ShouldPanic, last: RawResult) -> RawResult {
    if classify(should_panic, &last).is_none() == pass {
        return last;
    }
    match (pass, should_panic) {
        (true, ShouldPanic::No) | (false, ShouldPanic::Yes | ShouldPanic::YesWithMessage(_)) => Ok(Ok(())),
        (true, ShouldPanic::Yes) => Err(Box::new("")),
        (true, ShouldPanic::YesWithMessage(expected)) => Err(Box::new(expected.clone())),
        (false, ShouldPanic::No) => Ok(Err("test failed".to_string())),
    }
}

fn error_info(failure: Failure, raw: &RawResult) -> ErrorInfo {
    let (kind, message, stack) = match failure {
        Failure::Panicked => {
            let message = payload(raw).map(panics::payload_message).unwrap_or_default();
            let details = panics::take().filter(|d| d.message == message);
            let stack = details.map(|d| match d.location {
                Some(location) => format!("panicked at {location}\n{}", d.backtrace),
                None => d.backtrace.to_string(),
            });
            ("panic", message, stack)
        }
        Failure::ReturnedError => {
            let message = match raw {
                Ok(Err(message)) => message.clone(),
                _ => String::new(),
            };
            ("error", message, None)
        }
        Failure::DidNotPanic => ("should_panic", "test did not panic as expected".to_string(), None),
        Failure::WrongPanicMessage(message) => ("should_panic", message, None),
    };
    ErrorInfo {
        kind: kind.to_string(),
        message,
        stack,
    }
}

fn print_summary(summary: &Summary) {
    let mut parts = Vec::new();
    if summary.retries > 0 {
        parts.push(format!("{} retried executions", summary.retries));
    }
    if summary.quarantined_failures > 0 {
        parts.push(format!("{} quarantined failures ignored", summary.quarantined_failures));
    }
    if summary.disabled > 0 {
        parts.push(format!("{} disabled tests skipped", summary.disabled));
    }
    if summary.attempt_to_fix_failures > 0 {
        parts.push(format!(
            "{} attempt to fix tests failed",
            summary.attempt_to_fix_failures
        ));
    }
    if !parts.is_empty() {
        use std::io::Write;
        let _ = writeln!(std::io::stderr(), "Datadog Test Optimization: {}", parts.join(", "));
    }
}

/// Splits a libtest name into Datadog suite and test names: the suite is the
/// crate-qualified module path and the test is the last path segment.
pub fn split_test_name(crate_name: &str, full_name: &str) -> (String, String) {
    match full_name.rsplit_once("::") {
        Some((path, name)) => (format!("{crate_name}::{path}"), name.to_string()),
        None => (crate_name.to_string(), full_name.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Hello;
    use std::sync::Arc;

    fn panic_with(message: &'static str) -> RawResult {
        Err(Box::new(message))
    }

    #[test]
    fn classifies_like_libtest() {
        let msg = ShouldPanic::YesWithMessage("boom".into());
        assert_eq!(classify(&ShouldPanic::No, &Ok(Ok(()))), None);
        assert_eq!(classify(&ShouldPanic::No, &panic_with("x")), Some(Failure::Panicked));
        assert_eq!(
            classify(&ShouldPanic::No, &Ok(Err("e".into()))),
            Some(Failure::ReturnedError)
        );
        assert_eq!(classify(&ShouldPanic::Yes, &panic_with("x")), None);
        assert_eq!(classify(&ShouldPanic::Yes, &Ok(Ok(()))), Some(Failure::DidNotPanic));
        assert_eq!(classify(&msg, &panic_with("big boom")), None);
        assert_eq!(classify(&msg, &Err(Box::new(String::from("boom!")))), None);
        assert!(matches!(
            classify(&msg, &panic_with("bang")),
            Some(Failure::WrongPanicMessage(_))
        ));
        assert!(matches!(
            classify(&msg, &Err(Box::new(1u8))),
            Some(Failure::WrongPanicMessage(_))
        ));
    }

    #[test]
    fn libtest_result_matches_requested_outcome() {
        let cases = [
            ShouldPanic::No,
            ShouldPanic::Yes,
            ShouldPanic::YesWithMessage("boom".into()),
        ];
        for should_panic in &cases {
            for last in [Ok(Ok(())), panic_with("boom"), Ok(Err("e".into()))] {
                for pass in [true, false] {
                    let result = libtest_result(pass, should_panic, last_clone(&last));
                    assert_eq!(
                        classify(should_panic, &result).is_none(),
                        pass,
                        "{should_panic:?} {last:?} {pass}"
                    );
                }
            }
        }
    }

    fn last_clone(raw: &RawResult) -> RawResult {
        match raw {
            Ok(r) => Ok(r.clone()),
            Err(p) => Err(Box::new(panics::payload_message(p.as_ref()))),
        }
    }

    #[test]
    fn splits_names_into_suite_and_test() {
        assert_eq!(
            split_test_name("bytes", "tests::nested::it"),
            ("bytes::tests::nested".into(), "it".into())
        );
        assert_eq!(
            split_test_name("test_buf", "fresh"),
            ("test_buf".into(), "fresh".into())
        );
    }

    #[derive(Default)]
    struct Recorder {
        bootstrap: Bootstrap,
        records: Mutex<Vec<TestRecord>>,
        finished: Mutex<Option<Finish>>,
    }

    impl Backend for Arc<Recorder> {
        fn bootstrap(&self, _: &Hello) -> Result<Bootstrap, String> {
            Ok(self.bootstrap.clone())
        }
        fn send(&self, record: TestRecord) {
            self.records.lock().unwrap().push(record);
        }
        fn finish(&self, finish: Finish) {
            *self.finished.lock().unwrap() = Some(finish);
        }
    }

    fn hello() -> Hello {
        Hello {
            module: "m".into(),
            crate_name: "c".into(),
            binary: "b".into(),
            rustc_version: "1".into(),
            library_version: "0".into(),
            command: "cargo test".into(),
            working_dir: ".".into(),
            parent_pid: None,
        }
    }

    fn info(name: &str) -> TestInfo {
        TestInfo {
            suite: "c".into(),
            name: name.into(),
            source_file: None,
            start_line: 1,
            end_line: 2,
            should_panic: ShouldPanic::No,
            ignored: false,
            ignore_message: None,
        }
    }

    #[test]
    fn auto_test_retries_stop_at_the_first_pass() {
        let recorder = Arc::new(Recorder {
            bootstrap: Bootstrap {
                auto_test_retries: Some(AutoTestRetries { retries: 5, budget: 10 }),
                ..Bootstrap::default()
            },
            ..Recorder::default()
        });
        let session = Session::start(&hello(), Box::new(Arc::clone(&recorder))).unwrap();
        let test = info("flaky");
        let plan = session.plan(std::slice::from_ref(&test)).remove(0);
        let mut calls = 0;
        let result = session.run(&test, &plan, true, &mut || {
            calls += 1;
            if calls == 1 {
                Ok(Err("first".into()))
            } else {
                Ok(Ok(()))
            }
        });
        assert!(matches!(result, Ok(Ok(()))));
        session.finish();
        let records = recorder.records.lock().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].tags["test.failure_suppression_reason"], "auto_test_retry");
        assert_eq!(records[0].outcome, None);
        assert_eq!(records[1].tags["test.retry_reason"], "auto_test_retry");
        assert_eq!(records[1].outcome, Some(Status::Pass));
        assert!(recorder.finished.lock().unwrap().is_some());
    }

    #[test]
    fn the_retry_budget_limits_the_tests_admitted_to_auto_test_retries() {
        let recorder = Arc::new(Recorder {
            bootstrap: Bootstrap {
                auto_test_retries: Some(AutoTestRetries { retries: 2, budget: 1 }),
                ..Bootstrap::default()
            },
            ..Recorder::default()
        });
        let session = Session::start(&hello(), Box::new(Arc::clone(&recorder))).unwrap();
        let tests = [info("first"), info("second")];
        let plans = session.plan(&tests);
        for (test, plan) in tests.iter().zip(&plans) {
            let result = session.run(test, plan, true, &mut || Ok(Err("fails".into())));
            assert!(matches!(result, Ok(Err(_))));
        }
        let records = recorder.records.lock().unwrap();
        let executions = |name: &str| records.iter().filter(|r| r.name == name).count();
        assert_eq!((executions("first"), executions("second")), (3, 1));
    }

    fn panic_error(attempt: &mut dyn FnMut() -> RawResult) -> ErrorInfo {
        let recorder = Arc::new(Recorder::default());
        let session = Session::start(&hello(), Box::new(Arc::clone(&recorder))).unwrap();
        let test = info("panics");
        let plan = session.plan(std::slice::from_ref(&test)).remove(0);
        let _ = session.run(&test, &plan, true, attempt);
        let records = recorder.records.lock().unwrap();
        records[0].error.clone().unwrap()
    }

    #[test]
    fn reports_the_panic_that_failed_the_test() {
        let error = panic_error(&mut || std::panic::catch_unwind(|| panic!("boom {}", 1)).map(Ok));
        assert_eq!(error.message, "boom 1");
        assert!(error.stack.unwrap().starts_with("panicked at "));
    }

    #[test]
    fn ignores_the_details_of_an_earlier_caught_panic() {
        let error = panic_error(&mut || {
            std::panic::catch_unwind(|| {
                assert!(std::panic::catch_unwind(|| panic!("expected")).is_err());
                let child = std::thread::spawn(|| panic!("actual"));
                std::panic::resume_unwind(child.join().unwrap_err());
            })
            .map(Ok)
        });
        assert_eq!(error.message, "actual");
        assert_eq!(error.stack, None);
    }

    #[test]
    fn faulty_sessions_disable_early_flake_detection() {
        let recorder = Arc::new(Recorder {
            bootstrap: Bootstrap {
                early_flake_detection: Some(EarlyFlakeDetection {
                    buckets: vec![(Duration::from_secs(5), 3)],
                    faulty_session_threshold: 30.0,
                    known_tests: [("c".to_string(), "known".to_string())].into_iter().collect(),
                }),
                ..Bootstrap::default()
            },
            ..Recorder::default()
        });
        let session = Session::start(&hello(), Box::new(Arc::clone(&recorder))).unwrap();
        let tests: Vec<TestInfo> = (0..31)
            .map(|i| info(&format!("new_{i}")))
            .chain([info("known")])
            .collect();
        assert!(session.plan(&tests).iter().all(|p| !p.is_new));
        session.finish();
        assert_eq!(*recorder.finished.lock().unwrap(), Some(Finish { efd_faulty: true }));

        let few: Vec<TestInfo> = vec![info("known"), info("new")];
        let session = Session::start(&hello(), Box::new(Arc::clone(&recorder))).unwrap();
        let plans = session.plan(&few);
        assert!(!plans[0].is_new && plans[1].is_new);
    }
}
