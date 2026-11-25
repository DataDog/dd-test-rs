// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! One test session per `cargo nextest run`.
//!
//! nextest runs each test in its own process, so each one would report a
//! session of its own. The agents of one run share a directory in the read
//! cache: the first agent that starts takes the run's lock and becomes the
//! finalizer, and every agent writes a journal of what its process reported.
//! The finalizer waits for nextest to exit, folds the journals into one
//! session, one module per package and one suite per suite name, and sends
//! them. An agent that finds the lock taken only sends its test events.
//!
//! Anything unexpected, such as a lock or a directory that cannot be created,
//! leaves the agent on the per-process path, so a run is never lost quietly.

use crate::config::Config;
use crate::event::{Kind, Metadata, Span};
use crate::ids::{new_id, now_nanos};
use crate::read_cache;
use crate::session::{self, Counts, INCOMPLETE_MESSAGE};
use crate::telemetry::Telemetry;
use crate::transport::Transport;
use dd_test_core::protocol::{Hello, Status};
use dd_test_core::{debug, warn};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const JOURNAL_VERSION: u32 = 1;
const NEXTEST: &str = "nextest";
const LOCK: &str = "finalizer.lock";
const CONTEXT: &str = "context.json";
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// The longest CI job, so the finalizer never outlives the run by much more.
const RUNNER_WAIT_TIMEOUT: Duration = Duration::from_secs(6 * 3600);
/// The harness gives up on the agent after this long.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(120);
const STALE_AFTER: Duration = Duration::from_secs(3600);
const CLEANUP_MAX_ENTRIES: usize = 32;
const CLEANUP_MAX_DURATION: Duration = Duration::from_millis(5);
/// Wrapper scripts and target runners between the test binary and nextest.
const MAX_ANCESTOR_DEPTH: usize = 4;

/// Joins the `cargo nextest run` this test binary is part of, or returns `None`
/// when it is not one, or when the run cannot be shared.
pub(crate) fn join(hello: &Hello, module: &str, working_directory: &str, context: Context) -> Option<Member> {
    let run = RunEnv::from_env()?;
    let Some(runner) = find_runner(hello.parent_pid) else {
        debug!("nextest: no cargo-nextest among the parent processes");
        return None;
    };
    let root = read_cache::default_dir().join(NEXTEST);
    let member = Member::open(&root, run, Some(runner), module, working_directory, context)?;
    if member.lock.is_some() {
        // The finalizer outlives nextest, which kills the process group of a test when it times out.
        detach();
    }
    Some(member)
}

/// What nextest tells each test process about the run.
struct RunEnv {
    run_id: String,
    binary_id: String,
    test_name: String,
    attempt: u32,
    stress: Option<u64>,
}

impl RunEnv {
    fn from_env() -> Option<RunEnv> {
        RunEnv::from_lookup(|key| std::env::var(key).ok())
    }

    fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Option<RunEnv> {
        let value = |key: &str| get(key).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        if value("NEXTEST_EXECUTION_MODE")?.as_str() != "process-per-test" {
            return None;
        }
        let run_id = value("NEXTEST_RUN_ID")?;
        if !is_run_id(&run_id) {
            return None;
        }
        Some(RunEnv {
            run_id,
            binary_id: value("NEXTEST_BINARY_ID")?,
            test_name: value("NEXTEST_TEST_NAME")?,
            attempt: value("NEXTEST_ATTEMPT").and_then(|v| v.parse().ok()).unwrap_or(1),
            stress: value("NEXTEST_STRESS_CURRENT").and_then(|v| v.parse().ok()),
        })
    }
}

/// A run id becomes a directory name, so only the characters nextest uses get through.
fn is_run_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 64 && value.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f' | b'-'))
}

/// What the finalizer needs to report the run, written once per run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Context {
    pub service: String,
    pub env: String,
    pub library_version: String,
    pub rustc_version: String,
    pub command: String,
    pub session_name: String,
    pub provider: String,
    pub early_flake_detection: bool,
    pub test_management: bool,
    pub common: BTreeMap<String, String>,
}

/// One suite of one process, as the finalizer reports it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SuiteEntry {
    pub name: String,
    pub start: i64,
    pub end: i64,
    pub source_file: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct TestEntry {
    suite: String,
    name: String,
    status: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum State {
    Running,
    Done,
}

/// What one test process reported. A running journal is the marker that keeps
/// the finalizer waiting for the process.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Journal {
    v: u32,
    state: State,
    pid: u32,
    pstart: u64,
    binary_id: String,
    test_name: String,
    attempt: u32,
    stress: Option<u64>,
    module: String,
    working_directory: String,
    start: i64,
    end: i64,
    complete: bool,
    early_flake_detection: bool,
    efd_faulty: bool,
    test_management: bool,
    suites: Vec<SuiteEntry>,
    tests: Vec<TestEntry>,
}

/// The part of the run this process has to report.
pub(crate) struct Member {
    run_id: String,
    dir: PathBuf,
    marker: PathBuf,
    /// The run's lock. Held from the moment this process is the finalizer.
    lock: Option<File>,
    runner: Option<ProcessId>,
    context: Context,
    binary_id: String,
    test_name: String,
    attempt: u32,
    stress: Option<u64>,
    module: String,
    working_directory: String,
    pstart: u64,
    start: i64,
    session_id: u64,
    module_id: u64,
    tests: Vec<TestEntry>,
}

impl Member {
    fn open(
        root: &Path,
        run: RunEnv,
        runner: Option<ProcessId>,
        module: &str,
        working_directory: &str,
        context: Context,
    ) -> Option<Member> {
        let started = Instant::now();
        let dir = root.join(&run.run_id);
        if let Err(error) = read_cache::create_dir(&dir) {
            debug!(
                "nextest: reporting one session per test [run:{} error:{error}]",
                run.run_id
            );
            return None;
        }
        let lock = match read_cache::takeover_guard(&dir.join(LOCK)) {
            Ok(lock) => lock,
            Err(error) => {
                debug!(
                    "nextest: reporting one session per test [run:{} error:{error}]",
                    run.run_id
                );
                return None;
            }
        };
        let member = Member {
            session_id: derived_id(&[&run.run_id, "session"]),
            module_id: derived_id(&[&run.run_id, "module", module]),
            marker: dir.join(format!("j-{}-{:x}.json", std::process::id(), new_id())),
            dir,
            run_id: run.run_id,
            lock,
            runner,
            context,
            binary_id: run.binary_id,
            test_name: run.test_name,
            attempt: run.attempt,
            stress: run.stress,
            module: module.to_string(),
            working_directory: working_directory.to_string(),
            pstart: process_info(std::process::id()).map_or(0, |info| info.start),
            start: now_nanos(),
            tests: Vec::new(),
        };
        if let Err(error) = write_json(&member.marker, &member.journal()) {
            debug!(
                "nextest: reporting one session per test [run:{} error:{error}]",
                member.run_id
            );
            return None;
        }
        debug!(
            "nextest: joined [run:{} finalizer:{} elapsed_us:{}]",
            member.run_id,
            member.lock.is_some(),
            started.elapsed().as_micros()
        );
        Some(member)
    }

    pub(crate) fn session_id(&self) -> u64 {
        self.session_id
    }

    pub(crate) fn module_id(&self) -> u64 {
        self.module_id
    }

    pub(crate) fn suite_id(&self, suite: &str) -> u64 {
        derived_id(&[&self.run_id, "suite", &self.module, suite])
    }

    pub(crate) fn observe(&mut self, suite: &str, name: &str, status: Status) {
        self.tests.push(TestEntry {
            suite: suite.to_string(),
            name: name.to_string(),
            status: status.as_str().to_string(),
        });
    }

    /// Writes the journal of this process, then hands the run to the finalizer
    /// when this process holds the lock or can take it over.
    pub(crate) fn finish(mut self, suites: Vec<SuiteEntry>, complete: bool, efd_faulty: bool) -> Finalizer {
        let mut journal = self.journal();
        journal.state = State::Done;
        journal.end = now_nanos();
        journal.complete = complete;
        journal.efd_faulty = efd_faulty;
        journal.suites = suites;
        journal.tests = std::mem::take(&mut self.tests);
        let started = Instant::now();
        match write_json(&self.marker, &journal) {
            Ok(()) => debug!(
                "nextest: journal written [test:{} elapsed_us:{}]",
                self.test_name,
                started.elapsed().as_micros()
            ),
            Err(error) => warn!(
                "nextest: could not write the results of {}, the suite, module and session of this process count as incomplete: {error}",
                self.test_name
            ),
        }
        write_context(&self.dir, &self.context);
        Finalizer {
            root: self
                .dir
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| self.dir.clone()),
            run_id: self.run_id,
            dir: self.dir,
            runner: self.runner,
            lock: self.lock,
            limits: Limits::default(),
        }
    }

    fn journal(&self) -> Journal {
        Journal {
            v: JOURNAL_VERSION,
            state: State::Running,
            pid: std::process::id(),
            pstart: self.pstart,
            binary_id: self.binary_id.clone(),
            test_name: self.test_name.clone(),
            attempt: self.attempt,
            stress: self.stress,
            module: self.module.clone(),
            working_directory: self.working_directory.clone(),
            start: self.start,
            end: self.start,
            complete: false,
            early_flake_detection: self.context.early_flake_detection,
            efd_faulty: false,
            test_management: self.context.test_management,
            suites: Vec::new(),
            tests: Vec::new(),
        }
    }
}

/// Reports the whole run: suites, modules and the session, once nextest exited.
pub struct Finalizer {
    root: PathBuf,
    run_id: String,
    dir: PathBuf,
    runner: Option<ProcessId>,
    /// The run's lock when this process took it at the start of the run.
    lock: Option<File>,
    limits: Limits,
}

impl Finalizer {
    pub(crate) fn run(mut self) {
        let lock = match self.lock.take() {
            Some(lock) => lock,
            None => {
                let Some(lock) = self.take_over() else {
                    return;
                };
                detach();
                lock
            }
        };
        let log = self.open_log();
        escape(log.as_ref());
        debug!("nextest: finalizing [run:{}]", self.run_id);
        let journals = gather(&self.dir, self.runner, &self.limits);
        if journals.is_empty() {
            debug!("nextest: no journals, nothing to report");
        } else if let Some(context) = read_context(&self.dir) {
            let folded = fold(&journals);
            self.send(&context, &folded);
        } else {
            debug!("nextest: no context, the run is not reported");
        }
        let _ = std::fs::remove_dir_all(&self.dir);
        drop(lock);
        cleanup_stale(&self.root);
    }

    /// The lock of a run whose finalizer is gone: the operating system releases
    /// a flock when its holder dies.
    fn take_over(&self) -> Option<File> {
        read_cache::takeover_guard(&self.dir.join(LOCK)).ok().flatten()
    }

    fn open_log(&self) -> Option<File> {
        if !dd_test_core::log::debug_enabled() {
            return None;
        }
        let path = self.root.join(format!("{}.log", self.run_id));
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        match options.open(&path) {
            Ok(file) => Some(file),
            Err(error) => {
                debug!("nextest: no debug log: {error}");
                None
            }
        }
    }

    fn send(&self, context: &Context, folded: &Folded) {
        let spans = emit(&self.run_id, context, folded);
        let config = Config::from_env();
        let telemetry = Telemetry::start(
            &config,
            &context.service,
            &context.env,
            &context.rustc_version,
            &context.library_version,
        );
        for span in &spans {
            let event_type = match span.kind {
                Kind::Suite => "event_type:suite",
                Kind::Module => "event_type:module",
                _ => "event_type:session",
            };
            session::container_events(telemetry.as_ref(), event_type, folded.session.faulty);
        }
        if let Some(telemetry) = &telemetry {
            let provider = format!("provider:{}", context.provider);
            telemetry.count("test_session", &[provider.as_str()]);
        }
        let transport = match Transport::new(&config, None) {
            Ok(transport) => Some(transport),
            Err(error) => {
                warn!("nextest: test results will not be sent to Datadog: {error}");
                None
            }
        };
        let metadata = Metadata {
            runtime_id: format!("{:016x}{:016x}", new_id(), new_id()),
            library_version: context.library_version.clone(),
            env: context.env.clone(),
            session_name: context.session_name.clone(),
        };
        session::upload(transport.as_ref(), telemetry.as_ref(), &metadata, &spans);
        debug!("nextest: sent {} container events", spans.len());
        if let Some(telemetry) = &telemetry {
            telemetry.stop();
        }
    }
}

/// How long the finalizer waits, measured by the unit tests.
#[derive(Clone, Copy, Debug)]
struct Limits {
    runner_wait: Duration,
    drain: Duration,
    poll: Duration,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            runner_wait: RUNNER_WAIT_TIMEOUT,
            drain: DRAIN_TIMEOUT,
            poll: POLL_INTERVAL,
        }
    }
}

/// Waits for nextest to exit, then for the test processes that are still
/// writing, and reads every journal of the run.
fn gather(dir: &Path, runner: Option<ProcessId>, limits: &Limits) -> Vec<Journal> {
    wait_for_runner(runner, limits);
    drain(dir, limits);
    read_journals(dir)
}

fn wait_for_runner(runner: Option<ProcessId>, limits: &Limits) {
    let Some(runner) = runner else { return };
    let started = Instant::now();
    loop {
        if !runner.alive() {
            debug!("nextest: runner exited after {} us", started.elapsed().as_micros());
            return;
        }
        if started.elapsed() >= limits.runner_wait {
            debug!(
                "nextest: runner still running after {:?}, reporting what there is",
                limits.runner_wait
            );
            return;
        }
        std::thread::sleep(limits.poll);
    }
}

/// Waits for the test processes of the run that are still running. A process
/// that is gone or that runs out the timeout counts as incomplete.
fn drain(dir: &Path, limits: &Limits) {
    let started = Instant::now();
    loop {
        let running = live_markers(dir);
        if running == 0 {
            break;
        }
        if started.elapsed() >= limits.drain {
            warn!(
                "nextest: {running} test processes still running after {:?}, they count as incomplete",
                limits.drain
            );
            break;
        }
        std::thread::sleep(limits.poll);
    }
    debug!("nextest: drained {} journals", read_journals(dir).len());
}

/// Markers of processes that still run, other than this one: the finalizer's own
/// marker stays running when its journal could not be written.
fn live_markers(dir: &Path) -> usize {
    read_journals(dir)
        .iter()
        .filter(|journal| journal.state == State::Running)
        .filter(|journal| journal.pid != std::process::id())
        .filter(|journal| ProcessId::new(journal.pid, journal.pstart).is_some_and(|id| id.alive()))
        .count()
}

fn read_journals(dir: &Path) -> Vec<Journal> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut journals = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !(name.starts_with("j-") && name.ends_with(".json")) {
            continue;
        }
        match std::fs::read(entry.path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        {
            Some(journal) => journals.push(journal),
            None => debug!("nextest: unreadable journal {}", entry.path().display()),
        }
    }
    journals
}

fn read_context(dir: &Path) -> Option<Context> {
    match std::fs::read(dir.join(CONTEXT)) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(context) => Some(context),
            Err(error) => {
                debug!("nextest: unreadable context: {error}");
                None
            }
        },
        Err(error) => {
            debug!("nextest: no context: {error}");
            None
        }
    }
}

/// Publishes the context of the run once. Whichever process gets there first wins.
fn write_context(dir: &Path, context: &Context) {
    let path = dir.join(CONTEXT);
    if path.exists() {
        return;
    }
    let bytes = match serde_json::to_vec(context) {
        Ok(bytes) => bytes,
        Err(error) => {
            debug!("nextest: could not encode the run context: {error}");
            return;
        }
    };
    let temp = dir.join(format!("{CONTEXT}.{}.{:x}.tmp", std::process::id(), new_id()));
    let linked = read_cache::write_new(&temp, &bytes).and_then(|()| std::fs::hard_link(&temp, &path));
    let _ = std::fs::remove_file(&temp);
    if let Err(error) = linked {
        debug!("nextest: could not write the run context: {error}");
    }
}

/// Publishes a complete file, so the finalizer never reads a partial one.
fn write_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let temp = path.with_file_name(format!(".{name}.{:x}.tmp", new_id()));
    read_cache::write_new(&temp, &bytes)?;
    std::fs::rename(&temp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })
}

/// Removes the directories and logs of runs that ended a while ago.
fn cleanup_stale(root: &Path) {
    cleanup_stale_within(root, CLEANUP_MAX_DURATION);
}

/// The same, waiting up to `max_duration` and checking a few entries, so the
/// cache does not grow one directory per run.
fn cleanup_stale_within(root: &Path, max_duration: Duration) {
    let started = Instant::now();
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for (checked, entry) in entries.flatten().enumerate() {
        if checked == CLEANUP_MAX_ENTRIES || started.elapsed() > max_duration {
            break;
        }
        let old = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > STALE_AFTER);
        if !old {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            if matches!(read_cache::takeover_guard(&path.join(LOCK)), Ok(Some(_))) {
                let _ = std::fs::remove_dir_all(&path);
            }
        } else if path.extension().is_some_and(|extension| extension == "log") {
            let _ = std::fs::remove_file(&path);
        }
    }
}

struct Folded {
    suites: Vec<FoldedSuite>,
    modules: Vec<FoldedContainer>,
    session: FoldedContainer,
}

struct FoldedSuite {
    module: String,
    name: String,
    working_directory: String,
    start: i64,
    end: i64,
    source_file: Option<String>,
    counts: Counts,
}

struct FoldedContainer {
    module: Option<String>,
    working_directory: String,
    start: i64,
    end: i64,
    counts: Counts,
    incomplete: bool,
    early_flake_detection: bool,
    faulty: bool,
    test_management: bool,
}

/// Folds the journals of one run into one suite per suite name, one module per
/// package and one session. Only the last attempt of each test counts, since
/// nextest runs a retry in a new process.
fn fold(journals: &[Journal]) -> Folded {
    let mut kept: BTreeMap<(&str, &str, Option<u64>), &Journal> = BTreeMap::new();
    for journal in journals {
        let key = (journal.binary_id.as_str(), journal.test_name.as_str(), journal.stress);
        let replaced = kept.get(&key).is_none_or(|current| journal.attempt > current.attempt);
        if replaced {
            kept.insert(key, journal);
        }
    }

    let mut session = FoldedContainer {
        module: None,
        working_directory: String::new(),
        start: 0,
        end: 0,
        counts: Counts::default(),
        incomplete: false,
        early_flake_detection: false,
        faulty: false,
        test_management: false,
    };
    let mut range: Option<(i64, i64)> = None;
    let mut modules: BTreeMap<String, FoldedContainer> = BTreeMap::new();
    let mut suites: BTreeMap<(&str, &str), FoldedSuite> = BTreeMap::new();
    for journal in journals {
        let (start, end) = range.unwrap_or((journal.start, journal.end));
        range = Some((start.min(journal.start), end.max(journal.end)));
        session.early_flake_detection |= journal.early_flake_detection;
        session.faulty |= journal.efd_faulty;
        session.test_management |= journal.test_management;
        let module = modules
            .entry(journal.module.clone())
            .or_insert_with(|| FoldedContainer {
                module: Some(journal.module.clone()),
                working_directory: journal.working_directory.clone(),
                start: journal.start,
                end: journal.end,
                counts: Counts::default(),
                incomplete: false,
                early_flake_detection: false,
                faulty: false,
                test_management: false,
            });
        module.start = module.start.min(journal.start);
        module.end = module.end.max(journal.end);
        for suite in &journal.suites {
            let folded = suites
                .entry((&journal.module, &suite.name))
                .or_insert_with(|| FoldedSuite {
                    module: journal.module.clone(),
                    name: suite.name.clone(),
                    working_directory: journal.working_directory.clone(),
                    start: suite.start,
                    end: suite.end,
                    source_file: suite.source_file.clone(),
                    counts: Counts::default(),
                });
            folded.start = folded.start.min(suite.start);
            folded.end = folded.end.max(suite.end);
            folded.source_file = folded.source_file.take().or(suite.source_file.clone());
        }
    }

    for journal in kept.values() {
        let Some(module) = modules.get_mut(&journal.module) else {
            continue;
        };
        module.incomplete |= !journal.complete;
        module.early_flake_detection |= journal.early_flake_detection;
        module.faulty |= journal.efd_faulty;
        module.test_management |= journal.test_management;
        session.incomplete |= !journal.complete;
        for test in &journal.tests {
            let status = Status::parse(&test.status).unwrap_or(Status::Fail);
            if let Some(folded) = suites.get_mut(&(journal.module.as_str(), test.suite.as_str())) {
                folded.counts.add(status);
            }
            module.counts.add(status);
            session.counts.add(status);
        }
    }

    let directories: Vec<&str> = modules
        .values()
        .map(|module| module.working_directory.as_str())
        .collect();
    let shared = directories
        .first()
        .copied()
        .filter(|first| directories.iter().all(|directory| directory == first));
    session.working_directory = shared.unwrap_or(".").to_string();
    (session.start, session.end) = range.unwrap_or((0, 0));
    Folded {
        suites: suites.into_values().collect(),
        modules: modules.into_values().collect(),
        session,
    }
}

fn emit(run_id: &str, context: &Context, folded: &Folded) -> Vec<Span> {
    let session_id = derived_id(&[run_id, "session"]);
    let mut common = context.common.clone();
    common.remove("span.kind");
    let module_id = |module: &str| derived_id(&[run_id, "module", module]);
    let mut spans = Vec::with_capacity(folded.suites.len() + folded.modules.len() + 1);

    for suite in &folded.suites {
        let resource = format!("libtest.test_suite.{}", suite.name);
        let mut span = Span::new(
            Kind::Suite,
            "libtest.test_suite",
            &resource,
            &context.service,
            suite.start,
        );
        span.session_id = session_id;
        span.module_id = module_id(&suite.module);
        span.suite_id = derived_id(&[run_id, "suite", &suite.module, &suite.name]);
        span.duration = suite.end - suite.start;
        span.meta = common.clone();
        span.set("test.module", suite.module.clone());
        span.set("test.command", context.command.clone());
        span.set("test.working_directory", suite.working_directory.clone());
        span.set("test.suite", suite.name.clone());
        if let Some(file) = &suite.source_file {
            span.set("test.source.file", file.clone());
        }
        span.set_metric("_dd.top_level", 1.0);
        session::set_status(&mut span, suite.counts.status());
        spans.push(span);
    }

    for module in &folded.modules {
        let name = module.module.as_deref().unwrap_or_default();
        let resource = format!("libtest.test_module.{name}");
        let mut span = Span::new(
            Kind::Module,
            "libtest.test_module",
            &resource,
            &context.service,
            module.start,
        );
        span.session_id = session_id;
        span.module_id = module_id(name);
        span.duration = module.end - module.start;
        span.meta = common.clone();
        span.set("test.module", name);
        span.set("test.command", context.command.clone());
        span.set("test.working_directory", module.working_directory.clone());
        span.set_metric("_dd.top_level", 1.0);
        session::set_status(&mut span, container_status(module));
        if module.incomplete {
            span.set("error.message", INCOMPLETE_MESSAGE);
        }
        session::set_container_tags(
            &mut span,
            module.early_flake_detection,
            module.faulty,
            module.test_management,
        );
        spans.push(span);
    }

    let session = &folded.session;
    let resource = format!("libtest.test_session.{}", context.command);
    let mut span = Span::new(
        Kind::Session,
        "libtest.test_session",
        &resource,
        &context.service,
        session.start,
    );
    span.session_id = session_id;
    span.duration = session.end - session.start;
    span.meta = common;
    if let [module] = folded.modules.as_slice() {
        span.set("test.module", module.module.clone().unwrap_or_default());
    }
    span.set("test.command", context.command.clone());
    span.set("test.working_directory", session.working_directory.clone());
    span.set_metric("_dd.top_level", 1.0);
    session::set_status(&mut span, container_status(session));
    if session.incomplete {
        span.set("error.message", INCOMPLETE_MESSAGE);
    }
    session::set_container_tags(
        &mut span,
        session.early_flake_detection,
        session.faulty,
        session.test_management,
    );
    spans.push(span);
    spans
}

/// A process that did not report all its results fails its module and the session.
fn container_status(container: &FoldedContainer) -> Status {
    if container.incomplete {
        Status::Fail
    } else {
        container.counts.status()
    }
}

/// Identifies a process across pid reuse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProcessId {
    pid: u32,
    start: u64,
}

impl ProcessId {
    fn new(pid: u32, start: u64) -> Option<ProcessId> {
        (pid != 0 && start != 0).then_some(ProcessId { pid, start })
    }

    fn alive(self) -> bool {
        process_info(self.pid).is_some_and(|info| info.start == self.start)
    }
}

struct ProcessInfo {
    parent: u32,
    name: String,
    start: u64,
}

/// The parent process of the test binary, such as nextest or a wrapper of it.
fn find_runner(parent: Option<u32>) -> Option<ProcessId> {
    let mut pid = parent?;
    for _ in 0..MAX_ANCESTOR_DEPTH {
        let info = process_info(pid)?;
        if info.name == "cargo-nextest" {
            return ProcessId::new(pid, info.start);
        }
        if info.parent == 0 || info.parent == pid {
            return None;
        }
        pid = info.parent;
    }
    None
}

#[cfg(target_os = "linux")]
fn process_info(pid: u32) -> Option<ProcessInfo> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tail = &stat[stat.rfind(')')? + 1..];
    let mut fields = tail.split_whitespace();
    fields.next()?;
    let parent = fields.next()?.parse().ok()?;
    let start = fields.nth(17)?.parse().ok()?;
    let name = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    Some(ProcessInfo {
        parent,
        name: name.trim().to_string(),
        start,
    })
}

#[cfg(target_os = "macos")]
fn process_info(pid: u32) -> Option<ProcessInfo> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let read = unsafe {
        libc::proc_pidinfo(
            libc::c_int::try_from(pid).ok()?,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if read != size {
        return None;
    }
    let info = unsafe { info.assume_init() };
    let name = info
        .pbi_comm
        .iter()
        .map(|c| *c as u8)
        .take_while(|b| *b != 0)
        .map(char::from)
        .collect();
    Some(ProcessInfo {
        parent: info.pbi_ppid,
        name,
        start: info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec,
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[allow(dead_code)]
fn process_info(_pid: u32) -> Option<ProcessInfo> {
    None
}

/// Ids shared by every process of a run, derived from the run id so that two
/// processes that join the same run agree on them.
fn derived_id(parts: &[&str]) -> u64 {
    let text = parts.join("\u{0}");
    let digest = ring::digest::digest(&ring::digest::SHA256, text.as_bytes());
    let bytes: [u8; 8] = digest.as_ref()[..8].try_into().unwrap_or_default();
    (u64::from_be_bytes(bytes) & i64::MAX as u64).max(1)
}

/// Leaves the process group of the test, so that nextest does not kill the
/// finalizer when the run times out.
#[cfg(unix)]
fn detach() {
    if unsafe { libc::setsid() } < 0 {
        debug!(
            "nextest: could not start a session: {}",
            std::io::Error::last_os_error()
        );
    }
}

#[cfg(not(unix))]
fn detach() {}

/// Releases the pipes of the test binary and of nextest, so that neither sees a
/// leak, and sends the debug log of the run to its file.
#[cfg(unix)]
fn escape(log: Option<&File>) {
    use std::os::fd::AsRawFd;
    let null = match OpenOptions::new().read(true).write(true).open("/dev/null") {
        Ok(null) => null,
        Err(_) => return,
    };
    for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO] {
        unsafe { libc::dup2(null.as_raw_fd(), fd) };
    }
    let stderr = log.map_or(null.as_raw_fd(), |log| log.as_raw_fd());
    unsafe { libc::dup2(stderr, libc::STDERR_FILENO) };
}

#[cfg(not(unix))]
fn escape(_log: Option<&File>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dd-test-nextest-{name}-{:x}", new_id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn run_env(run_id: &str) -> RunEnv {
        RunEnv {
            run_id: run_id.to_string(),
            binary_id: "fixture::integration".to_string(),
            test_name: "integration::adds".to_string(),
            attempt: 1,
            stress: None,
        }
    }

    fn context() -> Context {
        Context {
            service: "fixture".to_string(),
            env: "ci".to_string(),
            library_version: "0.2.0".to_string(),
            rustc_version: "1.98.1".to_string(),
            command: "cargo-nextest nextest run".to_string(),
            session_name: "unit-cargo-nextest nextest run".to_string(),
            provider: "github".to_string(),
            early_flake_detection: true,
            test_management: true,
            common: [("language".to_string(), "rust".to_string())].into_iter().collect(),
        }
    }

    fn join_in(dir: &Path) -> Option<Member> {
        Member::open(
            dir,
            run_env("0b8a2f6e-4f3e-4f0a-9a1e-9d6c7b3f2a10"),
            None,
            "fixture",
            "fixtures/basic",
            context(),
        )
    }

    fn journal(module: &str, suite: &str, test: &str, status: Option<Status>) -> Journal {
        Journal {
            v: JOURNAL_VERSION,
            state: State::Done,
            pid: 1,
            pstart: 1,
            binary_id: format!("{module}::{suite}"),
            test_name: format!("{suite}::{test}"),
            attempt: 1,
            stress: None,
            module: module.to_string(),
            working_directory: format!("fixtures/{module}"),
            start: 10,
            end: 20,
            complete: status.is_some(),
            early_flake_detection: false,
            efd_faulty: false,
            test_management: false,
            suites: vec![SuiteEntry {
                name: suite.to_string(),
                start: 10,
                end: 20,
                source_file: None,
            }],
            tests: status
                .map(|status| TestEntry {
                    suite: suite.to_string(),
                    name: test.to_string(),
                    status: status.as_str().to_string(),
                })
                .into_iter()
                .collect(),
        }
    }

    fn status_of(folded: &Folded, module: Option<&str>, suite: Option<&str>) -> Status {
        match (module, suite) {
            (Some(module), Some(suite)) => folded
                .suites
                .iter()
                .find(|folded| folded.module == module && folded.name == suite)
                .unwrap()
                .counts
                .status(),
            (Some(module), None) => container_status(
                folded
                    .modules
                    .iter()
                    .find(|folded| folded.module.as_deref() == Some(module))
                    .unwrap(),
            ),
            _ => container_status(&folded.session),
        }
    }

    #[test]
    fn derives_stable_ids() {
        let session = derived_id(&["run", "session"]);
        assert_eq!(session, derived_id(&["run", "session"]));
        assert!(session > 0 && session <= i64::MAX as u64);
        assert_ne!(session, derived_id(&["run", "module", "a"]));
        assert_ne!(
            derived_id(&["run", "suite", "a", "b"]),
            derived_id(&["run", "suite", "ab"])
        );
        assert_ne!(derived_id(&[]), 0);
    }

    #[test]
    fn keeps_the_last_attempt() {
        let mut failed = journal("fixture", "integration", "flaky", Some(Status::Fail));
        failed.attempt = 1;
        let mut passed = journal("fixture", "integration", "flaky", Some(Status::Pass));
        passed.attempt = 2;
        let folded = fold(&[failed, passed]);
        assert_eq!(status_of(&folded, None, Some("integration")), Status::Pass);
        assert_eq!(status_of(&folded, None, None), Status::Pass);
        assert!(!folded.session.incomplete);
    }

    #[test]
    fn ignores_a_killed_attempt_before_a_later_pass() {
        let mut killed = journal("fixture", "integration", "a", None);
        killed.attempt = 1;
        let mut passed = journal("fixture", "integration", "a", Some(Status::Pass));
        passed.attempt = 2;
        let folded = fold(&[killed, passed]);
        assert_eq!(status_of(&folded, None, Some("integration")), Status::Pass);
        assert!(!folded.session.incomplete);
    }

    #[test]
    fn an_incomplete_process_fails_its_module_and_the_session() {
        let mut killed = journal("fixture", "integration", "a", None);
        killed.start = 5;
        let mut passed = journal("fixture", "basic::tests", "passes", Some(Status::Pass));
        passed.module = "other".to_string();
        let folded = fold(&[killed, passed]);
        let failed = folded
            .modules
            .iter()
            .find(|module| module.module.as_deref() == Some("fixture"));
        assert!(failed.unwrap().incomplete);
        assert_eq!(status_of(&folded, Some("fixture"), None), Status::Fail);
        assert!(folded.session.incomplete);
        assert_eq!(status_of(&folded, Some("other"), None), Status::Pass);
        assert_eq!(folded.session.start, 5, "the killed process starts the session");
    }

    #[test]
    fn folds_suites_modules_and_the_session() {
        let passed = journal("fixture", "integration", "adds", Some(Status::Pass));
        let skipped = journal("fixture", "integration", "ignored", Some(Status::Skip));
        let failed = journal("fixture", "basic::tests", "fails", Some(Status::Fail));
        let folded = fold(&[passed, skipped, failed]);
        assert_eq!(status_of(&folded, Some("fixture"), Some("integration")), Status::Pass);
        assert_eq!(status_of(&folded, Some("fixture"), Some("basic::tests")), Status::Fail);
        assert_eq!(status_of(&folded, Some("fixture"), None), Status::Fail);
        assert_eq!(status_of(&folded, None, None), Status::Fail);
        assert_eq!(folded.suites.len(), 2);
        assert_eq!(folded.modules.len(), 1);
    }

    #[test]
    fn a_suite_of_skipped_tests_is_skipped() {
        let folded = fold(&[journal("fixture", "integration", "ignored", Some(Status::Skip))]);
        assert_eq!(status_of(&folded, None, Some("integration")), Status::Skip);
    }

    #[test]
    fn folds_two_modules_into_one_session() {
        let mut first = journal("one", "tests", "a", Some(Status::Pass));
        first.working_directory = "fixtures/one".to_string();
        let mut second = journal("two", "tests", "b", Some(Status::Pass));
        second.module = "two".to_string();
        second.working_directory = "fixtures/two".to_string();
        let folded = fold(&[first, second]);
        assert_eq!(folded.modules.len(), 2);
        assert_eq!(status_of(&folded, None, None), Status::Pass);
        assert_eq!(folded.session.working_directory, ".", "modules disagree");
    }

    #[test]
    fn takes_the_first_start_and_the_last_end() {
        let mut first = journal("fixture", "integration", "a", Some(Status::Pass));
        first.start = 30;
        first.end = 40;
        first.suites[0].start = 30;
        first.suites[0].end = 40;
        let mut second = journal("fixture", "integration", "b", Some(Status::Pass));
        second.start = 10;
        second.end = 50;
        second.suites[0].start = 10;
        second.suites[0].end = 50;
        let folded = fold(&[first, second]);
        assert_eq!((folded.session.start, folded.session.end), (10, 50));
        assert_eq!((folded.modules[0].start, folded.modules[0].end), (10, 50));
        assert_eq!((folded.suites[0].start, folded.suites[0].end), (10, 50));
    }

    #[test]
    fn combines_the_feature_flags() {
        let mut without = journal("fixture", "integration", "a", Some(Status::Pass));
        without.early_flake_detection = false;
        without.test_management = false;
        let mut with = journal("fixture", "integration", "b", Some(Status::Pass));
        with.attempt = 2;
        with.binary_id = "fixture::other".to_string();
        with.test_name = "integration::b".to_string();
        with.early_flake_detection = true;
        with.efd_faulty = true;
        with.test_management = true;
        let folded = fold(&[without, with]);
        assert!(folded.session.early_flake_detection && folded.session.test_management);
        assert!(folded.session.faulty);
        assert!(folded.modules[0].early_flake_detection && folded.modules[0].test_management);
    }

    #[test]
    fn reports_one_session_for_the_run() {
        let passed = journal("fixture", "integration", "adds", Some(Status::Pass));
        let mut failed = journal("fixture", "basic::tests", "fails", Some(Status::Fail));
        failed.module = "fixture".to_string();
        failed.early_flake_detection = true;
        let folded = fold(&[passed, failed]);
        let spans = emit("run", &context(), &folded);
        assert_eq!(spans.len(), 4, "two suites, one module and one session");
        let session = spans.last().unwrap();
        assert_eq!(session.kind, Kind::Session);
        assert_eq!(session.resource, "libtest.test_session.cargo-nextest nextest run");
        assert_eq!(session.meta["test.status"], "fail");
        assert_eq!(session.meta["test.early_flake.enabled"], "true");
        assert!(!session.meta.contains_key("span.kind"));
        assert!(spans.iter().all(|span| span.session_id == session.session_id));
        let module = spans.iter().find(|span| span.kind == Kind::Module).unwrap();
        assert_eq!(module.meta["test.module"], "fixture");
        assert!(!module.meta.contains_key("error.message"));
    }

    #[test]
    fn reports_a_single_module_on_the_session() {
        let folded = fold(&[journal("fixture", "integration", "adds", Some(Status::Pass))]);
        let spans = emit("run", &context(), &folded);
        assert_eq!(spans.last().unwrap().meta["test.module"], "fixture");
    }

    #[test]
    fn the_first_agent_finalizes_and_the_second_takes_over() {
        let dir = temp_dir("election");
        let first = join_in(&dir).unwrap();
        let second = join_in(&dir).unwrap();
        assert!(first.lock.is_some(), "the first agent finalizes");
        assert!(second.lock.is_none(), "the second agent only reports its own tests");

        let elected = first.finish(Vec::new(), true, false);
        let member = second.finish(Vec::new(), true, false);
        assert!(member.lock.is_none());
        drop(elected);
        eventually(|| member.take_over());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Retries `f` for a while. A child process forked by another test shares
    /// open files until it execs, so it can briefly hold a released lock.
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

    fn runner(command: &mut Command) -> (std::process::Child, Option<ProcessId>) {
        let child = command.spawn().unwrap();
        let id = ProcessId::new(child.id(), process_info(child.id()).map_or(0, |info| info.start));
        (child, id)
    }

    fn limits(runner_wait: Duration, drain: Duration) -> Limits {
        Limits {
            runner_wait,
            drain,
            poll: Duration::from_millis(10),
        }
    }

    #[test]
    fn waits_for_the_runner_to_exit() {
        let dir = temp_dir("waits");
        let (mut child, id) = runner(Command::new("sh").args(["-c", "sleep 0.3"]));
        let started = Instant::now();
        let journals = gather(&dir, id, &limits(Duration::from_secs(10), Duration::from_millis(100)));
        assert!(started.elapsed() >= Duration::from_millis(300));
        assert!(started.elapsed() < Duration::from_secs(5));
        child.wait().unwrap();
        assert!(journals.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_reused_pid_counts_as_exited() {
        let dir = temp_dir("reuse");
        let (mut child, id) = runner(Command::new("sh").args(["-c", "sleep 5"]));
        let start = id.unwrap().start + 1;
        let reused = ProcessId::new(child.id(), start).unwrap();
        let started = Instant::now();
        gather(
            &dir,
            Some(reused),
            &limits(Duration::from_secs(10), Duration::from_millis(100)),
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        child.kill().unwrap();
        child.wait().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn waits_for_a_live_test_process() {
        let dir = temp_dir("drain");
        let member = join_in(&dir).unwrap();
        let dir_path = member.dir.clone();
        let (mut child, id) = runner(Command::new("sh").args(["-c", "sleep 0.3"]));
        let mut marker = member.journal();
        marker.pid = id.unwrap().pid;
        marker.pstart = id.unwrap().start;
        write_json(&dir_path.join("j-live.json"), &marker).unwrap();
        let started = Instant::now();
        drain(&dir_path, &limits(Duration::from_secs(10), Duration::from_secs(10)));
        assert!(started.elapsed() >= Duration::from_millis(300));
        child.wait().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dead_marker_is_incomplete_and_a_live_one_runs_out_the_timeout() {
        let dir = temp_dir("timeout");
        let member = join_in(&dir).unwrap();
        let dir_path = member.dir.clone();
        let mut dead = member.journal();
        dead.pid = 1;
        dead.pstart = 1;
        write_json(&dir_path.join("j-dead.json"), &dead).unwrap();
        let (mut child, id) = runner(Command::new("sh").args(["-c", "sleep 5"]));
        let mut live = member.journal();
        live.pid = id.unwrap().pid;
        live.pstart = id.unwrap().start;
        write_json(&dir_path.join("j-live.json"), &live).unwrap();

        let started = Instant::now();
        drain(&dir_path, &limits(Duration::from_secs(10), Duration::from_millis(200)));
        assert!(started.elapsed() >= Duration::from_millis(200));
        assert!(started.elapsed() < Duration::from_secs(3));
        child.kill().unwrap();
        child.wait().unwrap();

        let folded = fold(&read_journals(&dir_path));
        assert!(folded.session.incomplete);
        assert!(folded.modules[0].incomplete);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn removes_only_stale_directories_whose_lock_is_free() {
        let root = temp_dir("cleanup");
        let old = std::time::SystemTime::now() - STALE_AFTER * 2;
        let stale = root.join("stale");
        let held = root.join("held");
        let fresh = root.join("fresh");
        for dir in [&stale, &held, &fresh] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let _guard = read_cache::takeover_guard(&held.join(LOCK)).unwrap().unwrap();
        for dir in [&stale, &held] {
            File::open(dir)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(old))
                .unwrap();
        }
        let log = root.join("run.log");
        File::create(&log)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
        let recent = root.join("recent.log");
        File::create(&recent).unwrap();

        cleanup_stale_within(&root, Duration::MAX);
        assert!(!stale.exists(), "an old directory with a free lock goes away");
        assert!(held.exists(), "a held lock keeps its directory");
        assert!(fresh.exists());
        assert!(!log.exists());
        assert!(recent.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn writes_the_context_once() {
        let dir = temp_dir("context");
        let mut first = context();
        write_context(&dir, &first);
        first.service = "other".to_string();
        write_context(&dir, &first);
        assert_eq!(read_context(&dir).unwrap().service, "fixture");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_run_ids_that_are_not_safe_directory_names() {
        let env = |run_id: &str| {
            let values = [
                ("NEXTEST_EXECUTION_MODE", "process-per-test"),
                ("NEXTEST_RUN_ID", run_id),
                ("NEXTEST_BINARY_ID", "fixture::integration"),
                ("NEXTEST_TEST_NAME", "integration::adds"),
            ];
            RunEnv::from_lookup(|key| {
                values
                    .iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, value)| value.to_string())
            })
        };
        assert!(env("0b8a2f6e-4f3e-4f0a-9a1e-9d6c7b3f2a10").is_some());
        assert!(env("../../etc").is_none());
        assert!(env("0B8A2F6E").is_none());
        assert!(env("").is_none());
        assert!(env(&"a".repeat(65)).is_none());
        let without_mode = RunEnv::from_lookup(|key| match key {
            "NEXTEST_RUN_ID" => Some("0b8a2f6e".to_string()),
            "NEXTEST_BINARY_ID" => Some("b".to_string()),
            "NEXTEST_TEST_NAME" => Some("t".to_string()),
            _ => None,
        });
        assert!(without_mode.is_none(), "plain cargo test never joins");
    }

    #[cfg(unix)]
    #[test]
    fn reads_the_start_time_of_processes() {
        let me = process_info(std::process::id()).unwrap();
        assert!(me.start > 0);
        let parent = std::os::unix::process::parent_id();
        let parent = process_info(parent).unwrap();
        assert!(parent.start > 0 && !parent.name.is_empty());
        assert!(ProcessId::new(std::process::id(), me.start).unwrap().alive());
        assert!(!ProcessId::new(std::process::id(), me.start + 1).unwrap().alive());
        assert!(!ProcessId::new(0, 0).is_some_and(|id| id.alive()));
    }
}
