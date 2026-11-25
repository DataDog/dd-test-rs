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
use crate::event::{encode_payload, Kind, Metadata, Span};
use crate::ids::new_id;
use crate::read_cache;
use crate::session::{self, Counts, INCOMPLETE_MESSAGE};
use crate::telemetry::Telemetry;
use crate::transport::Transport;
use crate::uploader;
use dd_test_core::engine::split_test_name;
use dd_test_core::protocol::{Hello, Status};
use dd_test_core::{debug, warn};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const JOURNAL_VERSION: u32 = 2;
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
/// The largest payload the intake accepts, uncompressed.
const MAX_PAYLOAD: usize = 5_100_000;

/// Joins the `cargo nextest run` this test binary is part of, or returns `None`
/// when it is not one, or when the run cannot be shared.
pub(crate) fn join(
    hello: &Hello,
    module: &str,
    repo_root: &Path,
    runtime_id: &str,
    start: i64,
    context: impl FnOnce() -> Context,
) -> Option<Member> {
    let run = RunEnv::from_env()?;
    if !runner_command(&hello.command) {
        debug!("nextest: the command that started the tests is not nextest, reporting one session per test process");
        return None;
    }
    let Some(runner) = nextest_parent(hello.parent_pid) else {
        debug!("nextest: cargo-nextest did not start the test binary");
        return None;
    };
    let Some(working_directory) = run_working_directory(repo_root) else {
        debug!("nextest: NEXTEST_WORKSPACE_ROOT is not set, reporting one session per test process");
        return None;
    };
    let root = read_cache::default_dir().join(NEXTEST);
    let session = SessionInfo {
        module: module.to_string(),
        crate_name: hello.crate_name.clone(),
        working_directory,
        runtime_id: runtime_id.to_string(),
        start,
        context: context(),
    };
    let member = Member::open(&root, run, Some(runner), session)?;
    if member.lock.is_some() {
        // The finalizer outlives nextest, which kills the process group of a test when it times out.
        detach();
    }
    Some(member)
}

/// The harness falls back to the arguments of the test binary when it cannot
/// read the parent process, which differ from test to test. Only the command of
/// the nextest process keeps the session fingerprint of the run the same.
fn runner_command(command: &str) -> bool {
    let mut words = command.split_whitespace();
    match words.next() {
        Some("cargo-nextest") => true,
        Some("cargo") => words.next() == Some("nextest"),
        _ => false,
    }
}

/// The working directory every event of the run reports, relative to the
/// repository root. Tests of different packages run in different directories,
/// and the events of one session must agree on this tag.
fn run_working_directory(repo_root: &Path) -> Option<String> {
    let workspace_root = std::env::var("NEXTEST_WORKSPACE_ROOT").ok();
    workspace_directory(repo_root, workspace_root.as_deref())
}

fn workspace_directory(repo_root: &Path, workspace_root: Option<&str>) -> Option<String> {
    let root = workspace_root.map(str::trim).filter(|root| !root.is_empty())?;
    Some(session::relative_directory(repo_root, &canonical(Path::new(root))))
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
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
    status: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum State {
    Running,
    Done,
}

/// The version of a journal, read alone so that a journal of another version
/// never has to read into this schema.
#[derive(Deserialize)]
struct JournalVersion {
    v: u32,
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
    service: String,
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

/// What the session of this test binary tells the run. Its start is the one the
/// session took before its first test, so no suite or test of the run starts
/// before the session.
struct SessionInfo {
    module: String,
    crate_name: String,
    working_directory: String,
    runtime_id: String,
    start: i64,
    context: Context,
}

pub(crate) struct Member {
    run_id: String,
    dir: PathBuf,
    marker: PathBuf,
    /// Held from the moment this process is the finalizer.
    lock: Option<File>,
    runner: Option<ProcessId>,
    context: Context,
    runtime_id: String,
    binary_id: String,
    test_name: String,
    attempt: u32,
    stress: Option<u64>,
    module: String,
    /// The suite of the test this process runs, known before it reports.
    suite: String,
    working_directory: String,
    pstart: u64,
    start: i64,
    session_id: u64,
    module_id: u64,
    tests: Vec<TestEntry>,
}

impl Member {
    fn open(root: &Path, run: RunEnv, runner: Option<ProcessId>, session: SessionInfo) -> Option<Member> {
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
        let suite = split_test_name(&session.crate_name, &run.test_name).0;
        let member = Member {
            session_id: derived_id(&[&run.run_id, "session"]),
            module_id: derived_id(&[&run.run_id, "module", &session.module]),
            marker: dir.join(format!("j-{}-{:x}.json", std::process::id(), new_id())),
            dir,
            run_id: run.run_id,
            lock,
            runner,
            context: session.context,
            runtime_id: session.runtime_id,
            binary_id: run.binary_id,
            test_name: run.test_name,
            attempt: run.attempt,
            stress: run.stress,
            module: session.module,
            suite,
            working_directory: session.working_directory,
            pstart: process_info(std::process::id()).map_or(0, |info| info.start),
            start: session.start,
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

    pub(crate) fn working_directory(&self) -> &str {
        &self.working_directory
    }

    pub(crate) fn module_id(&self) -> u64 {
        self.module_id
    }

    pub(crate) fn suite_id(&self, suite: &str) -> u64 {
        derived_id(&[&self.run_id, "suite", &self.module, suite])
    }

    pub(crate) fn observe(&mut self, suite: &str, status: Status) {
        self.tests.push(TestEntry {
            suite: suite.to_string(),
            status: status.as_str().to_string(),
        });
    }

    /// Writes the journal of this process, then hands the run to the finalizer
    /// when this process holds the lock or can take it over. `end` is taken
    /// before the test events are sent, so the run does not include upload time.
    pub(crate) fn finish(mut self, end: i64, suites: Vec<SuiteEntry>, complete: bool, efd_faulty: bool) -> Finalizer {
        let mut journal = self.journal();
        journal.state = State::Done;
        journal.end = end;
        journal.complete = complete;
        journal.efd_faulty = efd_faulty;
        journal.suites = suites;
        if !complete && !journal.suites.iter().any(|suite| suite.name == self.suite) {
            // The process did not report what it ran, so the suite it was about to run still counts.
            journal.suites.push(self.suite_entry(end));
        }
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
        self.finalizer()
    }

    /// Leaves the run without reporting anything: the process exits before the
    /// tests finish, so its journal must not count as incomplete. The finalizer
    /// keeps the run, and a member takes it over when the finalizer of the run
    /// is gone, so that the journals of the run are reported either way.
    pub(crate) fn withdraw(self) -> Finalizer {
        if let Err(error) = std::fs::remove_file(&self.marker) {
            debug!("nextest: could not withdraw the marker: {error}");
        }
        self.finalizer()
    }

    fn finalizer(self) -> Finalizer {
        let root = self
            .dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.dir.clone());
        Finalizer {
            root,
            run_id: self.run_id,
            dir: self.dir,
            runner: self.runner,
            lock: self.lock,
            runtime_id: self.runtime_id,
            context: self.context,
            start: self.start,
            working_directory: self.working_directory,
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
            service: self.context.service.clone(),
            working_directory: self.working_directory.clone(),
            start: self.start,
            end: self.start,
            complete: false,
            early_flake_detection: self.context.early_flake_detection,
            efd_faulty: false,
            test_management: self.context.test_management,
            suites: vec![self.suite_entry(self.start)],
            tests: Vec::new(),
        }
    }

    fn suite_entry(&self, end: i64) -> SuiteEntry {
        SuiteEntry {
            name: self.suite.clone(),
            start: self.start,
            end,
            source_file: None,
        }
    }
}

/// Reports the whole run: suites, modules and the session, once nextest exited.
pub(crate) struct Finalizer {
    root: PathBuf,
    run_id: String,
    dir: PathBuf,
    runner: Option<ProcessId>,
    /// The lock when this process took it at the start of the run.
    lock: Option<File>,
    /// The runtime id of this process, so its container events match its tests.
    runtime_id: String,
    /// The context of this process, used when `context.json` is missing.
    context: Context,
    start: i64,
    working_directory: String,
    limits: Limits,
}

impl Finalizer {
    pub(crate) fn run(mut self) {
        let lock = match self.lock.take() {
            Some(lock) => lock,
            None => match self.take_over() {
                Some(lock) => {
                    detach();
                    lock
                }
                // Another process reported the run, or it is gone.
                None => return,
            },
        };
        let log = self.open_log();
        escape(log.as_ref());
        debug!("nextest: finalizing [run:{}]", self.run_id);
        let journals = gather(&self.dir, self.runner, &self.limits);
        if journals.is_empty() {
            debug!("nextest: no journals, nothing to report");
        } else {
            let context = read_context(&self.dir).unwrap_or_else(|| self.context.clone());
            let folded = fold(&journals, self.own());
            self.send(&context, &folded);
        }
        let _ = std::fs::remove_dir_all(&self.dir);
        drop(lock);
        cleanup_stale(&self.root);
    }

    /// The lock of a run whose finalizer is gone: the operating system releases
    /// a flock when its holder dies. A run whose directory is removed is over,
    /// so this never brings the lock file back.
    fn take_over(&self) -> Option<File> {
        read_cache::existing_guard(&self.dir.join(LOCK)).ok().flatten()
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

    /// What this process knows about the run when no journal can tell it: the
    /// session it started with.
    fn own(&self) -> OwnSession<'_> {
        OwnSession {
            start: self.start,
            working_directory: &self.working_directory,
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
            runtime_id: self.runtime_id.clone(),
            library_version: context.library_version.clone(),
            env: context.env.clone(),
            session_name: context.session_name.clone(),
        };
        let batches = batches(&metadata, &spans);
        let mut sent = 0;
        for batch in &batches {
            sent += session::upload(transport.as_ref(), telemetry.as_ref(), &metadata, batch);
        }
        debug!(
            "nextest: sent {sent} of {} container events in {} batches",
            spans.len(),
            batches.len()
        );
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

/// Waits for nextest to exit, then for the processes that are still writing
/// their journal, and collects the journals of the run once they stopped.
fn gather(dir: &Path, runner: Option<ProcessId>, limits: &Limits) -> Vec<Journal> {
    wait_for_runner(runner, limits);
    let mut entries = read_entries(dir);
    let running: Vec<Journal> = entries
        .iter()
        .filter(|(_, journal)| writing(journal))
        .map(|(_, journal)| journal.clone())
        .collect();
    drain(&running, limits);
    reread(&mut entries);
    entries.into_iter().map(|(_, journal)| journal).collect()
}

/// Reads again the journals of the processes that were still writing when the
/// finalizer read the directory. One that wrote its journal during the drain
/// would otherwise count as incomplete, and one that withdrew has nothing to
/// report.
fn reread(entries: &mut Vec<(PathBuf, Journal)>) {
    entries.retain_mut(|(path, journal)| {
        if !writing(journal) {
            return true;
        }
        match read_journal(path) {
            Ok(Some(latest)) if !unreadable(&latest) => {
                *journal = latest;
                true
            }
            // A marker that stops reading keeps what the snapshot knew, as an
            // error does: its module, its attempt and its identity still count.
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(error) => {
                debug!("nextest: could not read {} again: {error}", path.display());
                true
            }
        }
    });
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
fn drain(journals: &[Journal], limits: &Limits) {
    let mut running: Vec<&Journal> = journals.iter().filter(|journal| writing(journal)).collect();
    if running.is_empty() {
        return;
    }
    let started = Instant::now();
    loop {
        running.retain(|journal| alive(journal));
        if running.is_empty() {
            debug!("nextest: every test process finished");
            return;
        }
        if started.elapsed() >= limits.drain {
            warn!(
                "nextest: {} test processes still running after {:?}, they count as incomplete",
                running.len(),
                limits.drain
            );
            return;
        }
        std::thread::sleep(limits.poll);
    }
}

/// A marker of another process that has not written its journal yet: the
/// finalizer's own marker stays running when its journal could not be written.
fn writing(journal: &Journal) -> bool {
    journal.state == State::Running && journal.pid != std::process::id()
}

/// A marker without a start time counts as alive, so a process that could not
/// read its own start time still gets `DRAIN_TIMEOUT` to write its journal.
fn alive(journal: &Journal) -> bool {
    journal.pstart == 0 || ProcessId::new(journal.pid, journal.pstart).is_some_and(|id| id.alive())
}

fn read_journals(dir: &Path) -> Vec<Journal> {
    read_entries(dir).into_iter().map(|(_, journal)| journal).collect()
}

fn read_entries(dir: &Path) -> Vec<(PathBuf, Journal)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut journals = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !(name.starts_with("j-") && name.ends_with(".json")) {
            continue;
        }
        match read_journal(&entry.path()) {
            Ok(Some(journal)) => journals.push((entry.path(), journal)),
            Ok(None) => {}
            Err(error) => {
                debug!("nextest: could not read {}: {error}", entry.path().display());
                journals.push((entry.path(), unreadable_journal()));
            }
        }
    }
    journals
}

/// The journal at `path`, or `None` when the file is gone, which is how a
/// process withdraws after a cancel.
fn read_journal(path: &Path) -> std::io::Result<Option<Journal>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    // The version alone first: a journal of another version may not read into
    // this schema, and its results cannot be trusted either way.
    let version = match serde_json::from_slice::<JournalVersion>(&bytes) {
        Ok(version) => version,
        Err(error) => {
            debug!(
                "nextest: unreadable journal {}: {error}, its process counts as incomplete",
                path.display()
            );
            return Ok(Some(unreadable_journal()));
        }
    };
    if version.v != JOURNAL_VERSION {
        debug!(
            "nextest: journal version {} at {}, its process counts as incomplete",
            version.v,
            path.display()
        );
        return Ok(Some(unreadable_journal()));
    }
    match serde_json::from_slice(&bytes) {
        Ok(journal) => Ok(Some(journal)),
        Err(error) => {
            debug!(
                "nextest: unreadable journal {}: {error}, its process counts as incomplete",
                path.display()
            );
            Ok(Some(unreadable_journal()))
        }
    }
}

/// The journal of a process whose results could not be read: it reported in,
/// though not which module it belongs to, and did not finish.
fn unreadable_journal() -> Journal {
    Journal {
        v: JOURNAL_VERSION,
        state: State::Done,
        pid: 0,
        pstart: 0,
        binary_id: String::new(),
        test_name: String::new(),
        attempt: 0,
        stress: None,
        module: String::new(),
        service: String::new(),
        working_directory: String::new(),
        start: 0,
        end: 0,
        complete: false,
        early_flake_detection: false,
        efd_faulty: false,
        test_management: false,
        suites: Vec::new(),
        tests: Vec::new(),
    }
}

/// A journal whose process reported nothing readable: it names no module, so
/// only its incompleteness counts.
fn unreadable(journal: &Journal) -> bool {
    journal.module.is_empty()
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

fn cleanup_stale(root: &Path) {
    cleanup_stale_within(root, CLEANUP_MAX_DURATION);
}
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
            // The guard stays held until the directory is gone, so no agent
            // joins a run that is being removed.
            match read_cache::takeover_guard(&path.join(LOCK)) {
                Ok(Some(_guard)) if !keeps_writing(&path) => {
                    let _ = std::fs::remove_dir_all(&path);
                }
                _ => {}
            }
        } else if path.extension().is_some_and(|extension| extension == "log") {
            // The log of a run that is still there belongs to its finalizer.
            let run = path.file_stem().map(|name| root.join(name)).unwrap_or_default();
            if !run.exists() {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

/// Whether a test process of the run is still writing its journal.
fn keeps_writing(dir: &Path) -> bool {
    read_journals(dir)
        .iter()
        .any(|journal| writing(journal) && alive(journal))
}

fn batches<'a>(metadata: &Metadata, spans: &'a [Span]) -> Vec<&'a [Span]> {
    batches_within(metadata, spans, MAX_PAYLOAD)
}

fn batches_within<'a>(metadata: &Metadata, spans: &'a [Span], max_payload: usize) -> Vec<&'a [Span]> {
    let mut batches = Vec::new();
    let mut start = 0;
    let mut bytes = 0;
    for (index, span) in spans.iter().enumerate() {
        // A payload of one event carries the whole payload overhead, so the sum
        // over-counts and no batch goes over the limit.
        let size = encode_payload(metadata, std::slice::from_ref(span)).len();
        if index > start && (index - start == uploader::FLUSH_EVENTS || bytes + size > max_payload) {
            batches.push(&spans[start..index]);
            start = index;
            bytes = 0;
        }
        bytes += size;
    }
    if start < spans.len() {
        batches.push(&spans[start..]);
    }
    batches
}

struct Folded {
    suites: Vec<FoldedSuite>,
    modules: Vec<FoldedContainer>,
    session: FoldedContainer,
}

struct FoldedSuite {
    module: String,
    name: String,
    service: String,
    working_directory: String,
    start: i64,
    end: i64,
    source_file: Option<String>,
    counts: Counts,
}

struct FoldedContainer {
    module: Option<String>,
    /// The service of the package, `None` on the session, which keeps the service of the run.
    service: Option<String>,
    working_directory: String,
    start: i64,
    end: i64,
    counts: Counts,
    incomplete: bool,
    early_flake_detection: bool,
    faulty: bool,
    test_management: bool,
}

/// The session of the process that finalizes the run, for when no journal can
/// say when the run started or where it ran.
struct OwnSession<'a> {
    start: i64,
    working_directory: &'a str,
}

/// Only the last attempt of each test counts, since nextest runs a retry in a
/// new process.
fn fold(journals: &[Journal], own: OwnSession<'_>) -> Folded {
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
        service: None,
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
    // A journal the finalizer could not read names no module and no time: its
    // process still fails the session.
    session.incomplete |= journals.iter().any(unreadable);
    for journal in journals.iter().filter(|journal| !journal.module.is_empty()) {
        let (start, end) = range.unwrap_or((journal.start, journal.end));
        range = Some((start.min(journal.start), end.max(journal.end)));
        session.early_flake_detection |= journal.early_flake_detection;
        session.faulty |= journal.efd_faulty;
        session.test_management |= journal.test_management;
        let module = modules
            .entry(journal.module.clone())
            .or_insert_with(|| FoldedContainer {
                module: Some(journal.module.clone()),
                service: Some(journal.service.clone()),
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
                    service: journal.service.clone(),
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

    // A process that did not finish fails the suites it reported, which would
    // otherwise count as skipped while their module and the session fail.
    // A test whose last attempt was killed reported no outcome of its own, so
    // the suites of its earlier attempts fail too.
    for (key, journal) in &kept {
        if journal.complete {
            continue;
        }
        for suite in &journal.suites {
            if let Some(folded) = suites.get_mut(&(journal.module.as_str(), suite.name.as_str())) {
                folded.counts.add(Status::Fail);
            }
        }
        if !journal.tests.is_empty() {
            continue;
        }
        for earlier in journals
            .iter()
            .filter(|earlier| earlier.binary_id == key.0 && earlier.test_name == key.1 && earlier.stress == key.2)
        {
            if std::ptr::eq(earlier, *journal) {
                continue;
            }
            for suite in &earlier.suites {
                if let Some(folded) = suites.get_mut(&(earlier.module.as_str(), suite.name.as_str())) {
                    folded.counts.add(Status::Fail);
                }
            }
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
    // A run whose journals all failed to read still has the session of this
    // process, which knows when it started and where nextest ran it.
    (session.start, session.end) = range.unwrap_or((own.start, own.start));
    session.working_directory = match shared {
        Some(directory) => directory.to_string(),
        None if directories.is_empty() => own.working_directory.to_string(),
        None => ".".to_string(),
    };
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
            &suite.service,
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
            module.service.as_deref().unwrap_or(&context.service),
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
    name: String,
    start: u64,
}

/// Whether nextest started the test binary. A test that runs `cargo test`
/// inherits the `NEXTEST_*` variables of the outer run, but cargo starts its
/// binary, so it does not join the outer run.
fn nextest_parent(parent: Option<u32>) -> Option<ProcessId> {
    let pid = parent?;
    let info = process_info(pid)?;
    if info.name != "cargo-nextest" {
        return None;
    }
    ProcessId::new(pid, info.start)
}

#[cfg(target_os = "linux")]
fn process_info(pid: u32) -> Option<ProcessInfo> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let start = stat_start(&stat)?;
    let name = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    Some(ProcessInfo {
        name: name.trim().to_string(),
        start,
    })
}

/// The start time of the process in a `/proc/<pid>/stat` line, or `None` when
/// the process exited.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn stat_start(stat: &str) -> Option<u64> {
    // The command can hold spaces and parentheses, so the fields start after
    // the last one. A zombie has exited, though its entry is still there.
    let mut fields = stat[stat.rfind(')')? + 1..].split_whitespace();
    if matches!(fields.next()?, "Z" | "X") {
        return None;
    }
    fields.nth(18)?.parse().ok()
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
            test_name: "adds".to_string(),
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

    fn session_info(start: i64) -> SessionInfo {
        SessionInfo {
            module: "fixture".to_string(),
            crate_name: "integration".to_string(),
            working_directory: "fixtures/basic".to_string(),
            runtime_id: "0123456789abcdef".to_string(),
            start,
            context: context(),
        }
    }

    fn join_in(dir: &Path) -> Option<Member> {
        Member::open(
            dir,
            run_env("0b8a2f6e-4f3e-4f0a-9a1e-9d6c7b3f2a10"),
            None,
            session_info(5),
        )
    }

    /// The session of the finalizer of the tests that call `join_in`.
    fn own() -> OwnSession<'static> {
        OwnSession {
            start: 5,
            working_directory: "fixtures/basic",
        }
    }

    fn fold_run(journals: &[Journal]) -> Folded {
        fold(journals, own())
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
            service: "fixture".to_string(),
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
                    status: status.as_str().to_string(),
                })
                .into_iter()
                .collect(),
        }
    }

    fn status_of(folded: &Folded, module: Option<&str>, suite: Option<&str>) -> Status {
        match (module, suite) {
            (module, Some(suite)) => folded
                .suites
                .iter()
                .find(|folded| folded.name == suite && module.is_none_or(|module| folded.module == module))
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
            (None, None) => container_status(&folded.session),
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
        let folded = fold_run(&[failed, passed]);
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
        let folded = fold_run(&[killed, passed]);
        assert_eq!(status_of(&folded, None, Some("integration")), Status::Pass);
        assert!(!folded.session.incomplete);
    }

    #[test]
    fn a_killed_last_attempt_fails_the_suite_of_a_failed_earlier_attempt() {
        let mut failed = journal("fixture", "integration", "a", Some(Status::Fail));
        failed.attempt = 1;
        let mut killed = journal("fixture", "integration", "a", None);
        killed.attempt = 2;
        killed.suites.clear();

        let folded = fold_run(&[failed, killed]);
        assert_eq!(status_of(&folded, None, Some("integration")), Status::Fail);
        assert!(folded.session.incomplete);
    }

    #[test]
    fn a_killed_last_attempt_fails_the_suite_of_a_passed_earlier_attempt() {
        let mut passed = journal("fixture", "integration", "a", Some(Status::Pass));
        passed.attempt = 1;
        let mut killed = journal("fixture", "integration", "a", None);
        killed.attempt = 2;
        killed.suites.clear();

        let folded = fold_run(&[passed, killed]);
        assert_eq!(status_of(&folded, None, Some("integration")), Status::Fail);
        assert_eq!(status_of(&folded, Some("fixture"), None), Status::Fail);
        assert_eq!(status_of(&folded, None, None), Status::Fail);
    }

    #[test]
    fn an_incomplete_process_fails_its_module_and_the_session() {
        let mut killed = journal("fixture", "integration", "a", None);
        killed.start = 5;
        let mut passed = journal("fixture", "basic::tests", "passes", Some(Status::Pass));
        passed.module = "other".to_string();
        let folded = fold_run(&[killed, passed]);
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
    fn an_incomplete_process_fails_the_suites_it_reported() {
        // An Early Flake Detection execution reported an intermediate event, so
        // its journal lists the suite, and the process was killed before it
        // reported any outcome.
        let killed = journal("fixture", "integration", "adds", None);
        assert!(killed.tests.is_empty(), "the process reported no outcome");
        let folded = fold_run(&[killed]);
        assert_eq!(status_of(&folded, Some("fixture"), Some("integration")), Status::Fail);
        assert_eq!(status_of(&folded, Some("fixture"), None), Status::Fail);
        assert_eq!(status_of(&folded, None, None), Status::Fail);
    }

    #[test]
    fn a_process_killed_before_it_reports_fails_the_suite_it_was_about_to_run() {
        let dir = temp_dir("killed-suite");
        let killed = join_in(&dir).unwrap();
        let dir_path = killed.dir.clone();
        // Killed before it reported a test, so only the marker written at join is left.
        drop(killed);
        let passed = journal("fixture", "integration", "passes", Some(Status::Pass));
        write_json(&dir_path.join("j-passed.json"), &passed).unwrap();

        let folded = fold_run(&read_journals(&dir_path));
        assert_eq!(status_of(&folded, None, Some("integration")), Status::Fail);
        assert_eq!(status_of(&folded, Some("fixture"), None), Status::Fail);
        assert_eq!(status_of(&folded, None, None), Status::Fail);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn folds_suites_modules_and_the_session() {
        let passed = journal("fixture", "integration", "adds", Some(Status::Pass));
        let skipped = journal("fixture", "integration", "ignored", Some(Status::Skip));
        let failed = journal("fixture", "basic::tests", "fails", Some(Status::Fail));
        let folded = fold_run(&[passed, skipped, failed]);
        assert_eq!(status_of(&folded, Some("fixture"), Some("integration")), Status::Pass);
        assert_eq!(status_of(&folded, Some("fixture"), Some("basic::tests")), Status::Fail);
        assert_eq!(status_of(&folded, Some("fixture"), None), Status::Fail);
        assert_eq!(status_of(&folded, None, None), Status::Fail);
        assert_eq!(folded.suites.len(), 2);
        assert_eq!(folded.modules.len(), 1);
    }

    #[test]
    fn a_suite_of_skipped_tests_is_skipped() {
        let folded = fold_run(&[journal("fixture", "integration", "ignored", Some(Status::Skip))]);
        assert_eq!(status_of(&folded, None, Some("integration")), Status::Skip);
    }

    #[test]
    fn folds_two_modules_into_one_session() {
        let mut first = journal("one", "tests", "a", Some(Status::Pass));
        first.working_directory = "fixtures/one".to_string();
        let mut second = journal("two", "tests", "b", Some(Status::Pass));
        second.module = "two".to_string();
        second.working_directory = "fixtures/two".to_string();
        let folded = fold_run(&[first, second]);
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
        let folded = fold_run(&[first, second]);
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
        let folded = fold_run(&[without, with]);
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
        let folded = fold_run(&[passed, failed]);
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
    fn a_package_keeps_its_service_on_its_suites_and_its_module() {
        // Two packages of one workspace, each with the service of its package
        // because the run sets neither DD_SERVICE nor a repository URL.
        let dir = temp_dir("services");
        let member = |module: &str, service: &str, test: &str| {
            let mut session = session_info(10);
            session.module = module.to_string();
            session.crate_name = "tests".to_string();
            session.context.service = service.to_string();
            let run = RunEnv {
                run_id: "0b8a2f6e-4f3e-4f0a-9a1e-9d6c7b3f2a10".to_string(),
                binary_id: format!("{module}::tests"),
                test_name: test.to_string(),
                attempt: 1,
                stress: None,
            };
            let suite = SuiteEntry {
                name: "tests".to_string(),
                start: 10,
                end: 20,
                source_file: None,
            };
            Member::open(&dir, run, None, session)
                .unwrap()
                .finish(20, vec![suite], true, false)
        };
        let run_dir = {
            let _first = member("a", "package-a", "one");
            let _second = member("b", "package-b", "two");
            dir.join("0b8a2f6e-4f3e-4f0a-9a1e-9d6c7b3f2a10")
        };

        let context = read_context(&run_dir).unwrap();
        let folded = fold_run(&read_journals(&run_dir));
        let spans = emit("run", &context, &folded);
        let service_of = |kind: Kind, module: &str| -> String {
            spans
                .iter()
                .find(|span| span.kind == kind && span.meta.get("test.module").map(String::as_str) == Some(module))
                .unwrap()
                .service
                .clone()
        };

        for (module, package) in [("a", "package-a"), ("b", "package-b")] {
            assert_eq!(service_of(Kind::Suite, module), package);
            assert_eq!(service_of(Kind::Module, module), package);
        }
        assert_eq!(
            spans.last().unwrap().service,
            "package-a",
            "the session keeps the service of the run context"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reports_a_single_module_on_the_session() {
        let folded = fold_run(&[journal("fixture", "integration", "adds", Some(Status::Pass))]);
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

        let elected = first.finish(1, Vec::new(), true, false);
        let member = second.finish(2, Vec::new(), true, false);
        assert!(member.lock.is_none());
        drop(elected);
        eventually(|| member.take_over());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cancelled_member_withdraws_its_marker() {
        let dir = temp_dir("cancel");
        let elected = join_in(&dir).unwrap();
        let member = join_in(&dir).unwrap();
        let marker = member.marker.clone();
        let dir_path = elected.dir.clone();
        let withdrawn = member.withdraw();
        assert!(withdrawn.lock.is_none(), "the elected agent keeps the run");
        assert!(!marker.exists(), "a process that reports nothing leaves no marker");
        assert!(
            withdrawn.take_over().is_none(),
            "the elected agent reports the run while it lives"
        );

        let finalizer = elected.finish(1, Vec::new(), true, false);
        let journals = read_journals(&dir_path);
        assert_eq!(journals.len(), 1, "only the process that reported counts");
        assert!(!fold_run(&journals).session.incomplete);
        drop(finalizer);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cancelled_member_takes_over_the_run_when_the_elected_agent_is_gone() {
        let dir = temp_dir("cancel-takeover");
        let elected = join_in(&dir).unwrap();
        let member = join_in(&dir).unwrap();
        let dir_path = elected.dir.clone();

        // The elected agent wrote its journal, then died before it could report
        // the run.
        drop(elected.finish(1, Vec::new(), true, false));
        let marker = member.marker.clone();
        let finalizer = member.withdraw();
        assert!(!marker.exists(), "a cancelled member reports nothing of its own");
        eventually(|| finalizer.take_over());

        let journals = gather(
            &dir_path,
            None,
            &limits(Duration::from_secs(10), Duration::from_millis(100)),
        );
        assert_eq!(journals.len(), 1, "the journal of the run is not stranded");
        assert!(!fold_run(&journals).session.incomplete);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cancelled_finalizer_still_reports_the_run() {
        let dir = temp_dir("cancel-finalizer");
        let elected = join_in(&dir).unwrap();
        let member = join_in(&dir).unwrap();
        let marker = elected.marker.clone();
        let dir_path = elected.dir.clone();

        // The member finishes first and finds the run taken by the elected agent.
        let finished = member.finish(2, Vec::new(), true, false);
        assert!(finished.take_over().is_none());

        let finalizer = elected.withdraw();
        assert!(!marker.exists());
        assert!(
            read_cache::existing_guard(&dir_path.join(LOCK)).unwrap().is_none(),
            "the lock stays with the finalizer"
        );
        assert!(finalizer.lock.is_some());
        let journals = gather(
            &dir_path,
            None,
            &limits(Duration::from_secs(10), Duration::from_millis(100)),
        );
        assert_eq!(journals.len(), 1);
        assert!(!fold_run(&journals).session.incomplete, "the run is still reported");
        drop(finalizer);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_member_that_takes_over_after_the_run_ended_does_not_finalize() {
        let dir = temp_dir("ended");
        let elected = join_in(&dir).unwrap();
        let member = join_in(&dir).unwrap();
        let dir_path = elected.dir.clone();
        drop(elected.lock);
        std::fs::remove_dir_all(&dir_path).unwrap();

        let finalizer = member.finish(2, Vec::new(), true, false);
        assert!(finalizer.take_over().is_none(), "the run is over");
        assert!(!dir_path.exists(), "the directory does not come back");
        assert!(!dir_path.join(LOCK).exists(), "the lock file does not come back");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_journal_keeps_the_end_time_it_is_given() {
        let dir = temp_dir("end");
        let member = join_in(&dir).unwrap();
        let dir_path = member.dir.clone();
        let marker = member.marker.clone();
        let finalizer = member.finish(42, Vec::new(), true, true);
        let journal = read_journals(&dir_path)
            .into_iter()
            .find(|journal| journal.end == 42)
            .expect("the journal ends when the process was told to, before its uploads");
        assert_eq!(journal.state, State::Done);
        assert!(journal.complete && journal.efd_faulty);
        assert!(marker.exists());
        drop(finalizer);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_marker_starts_when_the_session_started() {
        let dir = temp_dir("start");
        let member = join_in(&dir).unwrap();
        let journal = read_journals(&member.dir).pop().unwrap();
        assert_eq!(journal.start, 5, "the session starts before its first test");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_journal_of_an_unknown_version_counts_as_incomplete() {
        let dir = temp_dir("version");
        let mut written = journal("fixture", "integration", "a", Some(Status::Pass));
        written.v = JOURNAL_VERSION + 1;
        write_json(&dir.join("j-next.json"), &written).unwrap();

        let journals = read_journals(&dir);
        assert_eq!(journals.len(), 1, "the process of another version is not skipped");
        assert!(!journals[0].complete);
        assert!(
            journals[0].tests.is_empty(),
            "the results of another version are not trusted"
        );
        assert!(fold_run(&journals).session.incomplete);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_journal_counts_as_incomplete() {
        let dir = temp_dir("unreadable");
        std::fs::write(dir.join("j-broken.json"), b"{\"v\":1,").unwrap();

        let journals = read_journals(&dir);
        assert_eq!(journals.len(), 1, "the process is not skipped");
        assert!(!journals[0].complete);
        assert!(fold_run(&journals).session.incomplete);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_run_with_no_readable_journal_reports_the_session_of_its_finalizer() {
        let dir = temp_dir("unreadable-run");
        std::fs::write(dir.join("j-broken.json"), b"{\"v\":1,").unwrap();
        let mut written = journal("fixture", "integration", "a", Some(Status::Pass));
        written.v = JOURNAL_VERSION + 1;
        write_json(&dir.join("j-next.json"), &written).unwrap();

        let folded = fold(&read_journals(&dir), own());
        let spans = emit("run", &context(), &folded);
        let session = spans.last().unwrap();
        assert_eq!(session.kind, Kind::Session);
        assert_eq!(session.start, 5, "the session of this process started the run");
        assert_eq!(session.meta["test.working_directory"], "fixtures/basic");
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

    /// A child that exits when the handle it returns is dropped, so a test can
    /// start a wait and then end it without depending on how long the child takes.
    fn blocking_runner() -> (std::process::Child, Option<ProcessId>, std::process::ChildStdin) {
        let mut child = Command::new("sh")
            .args(["-c", "read line"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let id = ProcessId::new(child.id(), process_info(child.id()).map_or(0, |info| info.start));
        (child, id, stdin)
    }

    /// Runs a wait in the background, so the test can go on with the handshake
    /// that ends it.
    fn waiting<T: Send + 'static>(wait: impl FnOnce() -> T + Send + 'static) -> std::sync::mpsc::Receiver<T> {
        let (done, wait_for) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(wait());
        });
        wait_for
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
        let (mut child, id, stdin) = blocking_runner();
        // Reaped while waiting, or Linux keeps it a zombie with the same start
        // time and the wait never ends.
        let reaper = std::thread::spawn(move || child.wait().unwrap());
        let gathering = dir.clone();
        let gathered = waiting(move || {
            gather(
                &gathering,
                id,
                &limits(Duration::from_secs(10), Duration::from_millis(100)),
            )
        });
        assert!(
            gathered.recv_timeout(Duration::from_millis(100)).is_err(),
            "the finalizer waits for the runner"
        );
        drop(stdin);
        assert!(gathered.recv_timeout(Duration::from_secs(30)).unwrap().is_empty());
        reaper.join().unwrap();
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
        let (mut child, id, stdin) = blocking_runner();
        let id = id.unwrap();
        let mut marker = member.journal();
        marker.pid = id.pid;
        marker.pstart = id.start;
        write_json(&dir_path.join("j-live.json"), &marker).unwrap();
        let journals = read_journals(&dir_path);
        let waited = waiting(move || {
            drain(&journals, &limits(Duration::from_secs(10), Duration::from_secs(10)));
        });
        assert!(
            waited.recv_timeout(Duration::from_millis(100)).is_err(),
            "the drain waits for the test process"
        );
        drop(stdin);
        waited.recv_timeout(Duration::from_secs(30)).unwrap();
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

        let journals = read_journals(&dir_path);
        let started = Instant::now();
        drain(&journals, &limits(Duration::from_secs(10), Duration::from_millis(200)));
        assert!(started.elapsed() >= Duration::from_millis(200));
        assert!(started.elapsed() < Duration::from_secs(3));
        child.kill().unwrap();
        child.wait().unwrap();

        let folded = fold_run(&read_journals(&dir_path));
        assert!(folded.session.incomplete);
        assert!(folded.modules[0].incomplete);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_marker_without_a_start_time_counts_as_alive() {
        let dir = temp_dir("unknown-start");
        let member = join_in(&dir).unwrap();
        let mut marker = member.journal();
        marker.pid = 1;
        marker.pstart = 0;
        let started = Instant::now();
        drain(&[marker], &limits(Duration::from_secs(10), Duration::from_millis(200)));
        assert!(
            started.elapsed() >= Duration::from_millis(200),
            "the drain timeout bounds it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn drain_uses_the_journals_it_was_given() {
        let dir = temp_dir("parse-once");
        let member = join_in(&dir).unwrap();
        let dir_path = member.dir.clone();
        let (mut child, id, stdin) = blocking_runner();
        let id = id.unwrap();
        let mut marker = member.journal();
        marker.pid = id.pid;
        marker.pstart = id.start;
        write_json(&dir_path.join("j-live.json"), &marker).unwrap();
        let journals = read_journals(&dir_path);
        std::fs::remove_dir_all(&dir_path).unwrap();

        let waited = waiting(move || {
            drain(&journals, &limits(Duration::from_secs(10), Duration::from_secs(10)));
        });
        assert!(
            waited.recv_timeout(Duration::from_millis(100)).is_err(),
            "the process that was running decides, not a second read of the directory"
        );
        drop(stdin);
        waited.recv_timeout(Duration::from_secs(30)).unwrap();
        child.wait().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rereads_a_marker_that_finished_during_the_drain() {
        let dir = temp_dir("reread");
        let mut marker = journal("fixture", "integration", "adds", None);
        marker.state = State::Running;
        let path = dir.join("j-live.json");
        write_json(&path, &marker).unwrap();

        // The finalizer reads the directory, then the process writes its
        // journal and exits.
        let mut entries = read_entries(&dir);
        assert_eq!(entries.len(), 1);
        assert!(writing(&entries[0].1), "the process was still writing");
        write_json(&path, &journal("fixture", "integration", "adds", Some(Status::Pass))).unwrap();
        reread(&mut entries);

        let journals: Vec<Journal> = entries.into_iter().map(|(_, journal)| journal).collect();
        assert_eq!(journals.len(), 1);
        let folded = fold_run(&journals);
        assert!(
            !folded.session.incomplete,
            "the journal written during the drain counts"
        );
        assert_eq!(status_of(&folded, None, Some("integration")), Status::Pass);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_marker_that_withdrew_during_the_drain_is_dropped() {
        let dir = temp_dir("withdrawn");
        let mut marker = journal("fixture", "integration", "adds", None);
        marker.state = State::Running;
        let path = dir.join("j-live.json");
        write_json(&path, &marker).unwrap();

        let mut entries = read_entries(&dir);
        std::fs::remove_file(&path).unwrap();
        reread(&mut entries);

        assert!(entries.is_empty(), "a process that withdrew reports nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_marker_that_cannot_be_read_again_counts_as_incomplete() {
        let dir = temp_dir("reread-error");
        let mut marker = journal("fixture", "integration", "adds", None);
        marker.state = State::Running;
        // Reading a directory fails with an error other than a missing file.
        let path = dir.join("j-live.json");
        std::fs::create_dir(&path).unwrap();

        let mut entries = vec![(path, marker)];
        reread(&mut entries);

        assert_eq!(entries.len(), 1, "a process that cannot be read stays incomplete");
        let journals: Vec<Journal> = entries.into_iter().map(|(_, journal)| journal).collect();
        assert!(fold_run(&journals).session.incomplete);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_marker_that_stops_reading_during_the_drain_keeps_what_it_reported() {
        let dir = temp_dir("reread-version");
        let mut marker = journal("fixture", "integration", "adds", None);
        marker.state = State::Running;
        let path = dir.join("j-live.json");
        write_json(&path, &marker).unwrap();

        let mut entries = read_entries(&dir);
        let mut replaced = marker.clone();
        replaced.v = JOURNAL_VERSION + 1;
        write_json(&path, &replaced).unwrap();
        reread(&mut entries);

        assert_eq!(entries.len(), 1, "a process whose journal stops reading is not dropped");
        assert_eq!(
            entries[0].1, marker,
            "the snapshot keeps its module, attempt and identity"
        );
        assert!(!entries[0].1.complete, "the process never reported its results");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn removes_only_stale_directories_that_are_done() {
        let root = temp_dir("cleanup");
        let old = std::time::SystemTime::now() - STALE_AFTER * 2;
        let stale = root.join("stale");
        let held = root.join("held");
        let writing = root.join("writing");
        let fresh = root.join("fresh");
        for dir in [&stale, &held, &writing, &fresh] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let _guard = read_cache::takeover_guard(&held.join(LOCK)).unwrap().unwrap();
        let (mut child, id) = runner(Command::new("sh").args(["-c", "sleep 5"]));
        let done = journal("fixture", "integration", "a", Some(Status::Pass));
        write_json(&stale.join("j-done.json"), &done).unwrap();
        let mut running = journal("fixture", "integration", "b", None);
        running.state = State::Running;
        running.pid = id.unwrap().pid;
        running.pstart = id.unwrap().start;
        write_json(&writing.join("j-live.json"), &running).unwrap();
        for dir in [&stale, &held, &writing] {
            File::open(dir)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(old))
                .unwrap();
        }
        let log = |name: &str| {
            let path = root.join(name);
            File::create(&path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(old))
                .unwrap();
            path
        };
        // The log of a run whose directory is still there belongs to its finalizer.
        let kept_log = log("writing.log");
        let removed_log = log("gone.log");
        let recent = root.join("recent.log");
        File::create(&recent).unwrap();

        cleanup_stale_within(&root, Duration::MAX);
        assert!(!stale.exists(), "an old directory with a free lock goes away");
        assert!(held.exists(), "a held lock keeps its directory");
        assert!(writing.exists(), "a process that still writes keeps its directory");
        assert!(fresh.exists());
        assert!(kept_log.exists());
        assert!(!removed_log.exists());
        assert!(recent.exists());
        child.kill().unwrap();
        child.wait().unwrap();
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

    #[cfg(target_os = "linux")]
    #[test]
    fn a_zombie_counts_as_exited() {
        let mut child = Command::new("sh").args(["-c", "sleep 0.2"]).spawn().unwrap();
        let start = process_info(child.id()).unwrap().start;
        let state = |pid: u32| -> String {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
            stat.rsplit_once(')')
                .map_or(String::new(), |(_, tail)| tail.to_string())
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !state(child.id()).trim_start().starts_with('Z') {
            assert!(Instant::now() < deadline, "the child becomes a zombie");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !ProcessId::new(child.id(), start).unwrap().alive(),
            "a zombie still has its start time, but it has exited"
        );
        child.wait().unwrap();
    }

    #[test]
    fn reads_the_start_time_after_a_command_with_spaces_and_parentheses() {
        let stat = "1234 (a command (with) spaces) S 1 1234 1234 0 -1 4194304 1 0 0 0 0 0 0 0 20 0 1 0 987654 0 0";
        assert_eq!(stat_start(stat), Some(987_654));
        let zombie = "1234 (a command) Z 1 1234 1234 0 -1 4194304 1 0 0 0 0 0 0 0 20 0 1 0 987654 0 0";
        assert_eq!(stat_start(zombie), None, "a zombie has exited");
        assert_eq!(stat_start("not a stat line"), None);
    }

    #[cfg(unix)]
    #[test]
    fn only_a_parent_that_is_nextest_joins_the_run() {
        assert!(nextest_parent(None).is_none());
        assert!(nextest_parent(Some(0)).is_none());
        assert!(
            nextest_parent(Some(std::process::id())).is_none(),
            "the test binary of a nested cargo test does not join the outer run"
        );
    }

    #[test]
    fn only_the_command_of_nextest_joins_the_run() {
        assert!(runner_command(
            "cargo-nextest nextest --user-config-file none run --no-fail-fast"
        ));
        assert!(runner_command("cargo nextest run"));
        assert!(
            !runner_command("basic_fixture-0123456789abcdef --exact integration::adds"),
            "the harness reports the arguments of the test binary when it cannot read the parent"
        );
        assert!(!runner_command("cargo test --no-fail-fast"));
        assert!(!runner_command(""));
    }

    #[test]
    fn a_run_reports_its_workspace_root_as_the_working_directory() {
        let repo = temp_dir("workspace");
        let packages = repo.join("packages");
        std::fs::create_dir_all(packages.join("one")).unwrap();
        std::fs::create_dir_all(packages.join("two")).unwrap();
        let repo = repo.canonicalize().unwrap();
        let workspace = |path: &Path| workspace_directory(&repo, Some(path.to_str().unwrap()));

        // The workspace root, not the directory of the package that runs a
        // test, so every package of the run reports the same directory.
        assert_eq!(workspace(&repo).as_deref(), Some("."));
        assert_eq!(workspace(&packages).as_deref(), Some("packages"));
        assert_eq!(workspace_directory(&repo, None), None);
        assert_eq!(workspace_directory(&repo, Some("   ")), None);
        assert_eq!(workspace_directory(&repo, Some("/")).as_deref(), Some("."));
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn sends_the_container_events_in_batches_the_intake_accepts() {
        let metadata = Metadata {
            runtime_id: "0123456789abcdef".to_string(),
            library_version: "0.2.0".to_string(),
            env: "ci".to_string(),
            session_name: "unit".to_string(),
        };
        let span = |name: String| {
            let mut span = Span::new(Kind::Suite, "libtest.test_suite", &name, "svc", 0);
            span.set("filler", "x".repeat(1000));
            span
        };
        let spans: Vec<Span> = (0..uploader::FLUSH_EVENTS + 3).map(|i| span(format!("s{i}"))).collect();

        let batches = batches(&metadata, &spans);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), uploader::FLUSH_EVENTS);
        assert_eq!(batches[1].len(), 3);

        let limit = 50_000;
        let small = batches_within(&metadata, &spans, limit);
        assert!(small.len() > 2, "the payload limit splits the run further");
        assert!(small
            .iter()
            .all(|batch| encode_payload(&metadata, batch).len() <= limit));
        assert_eq!(small.iter().map(|batch| batch.len()).sum::<usize>(), spans.len());
    }
}
