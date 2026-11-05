// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Drop-in replacement for libtest, injected with `--extern test=` into test builds.
//!
//! The compiler-generated test `main` calls `test::test_main_static`. This crate
//! re-exports the real libtest and overrides that entry point so every test runs
//! through the Datadog engine. Everything else behaves like libtest.

#![feature(test)]
#![allow(unknown_lints, unexpected_cfgs)]

extern crate test as real;

pub use real::*;

use dd_test_core::backend::{locate_agent, AgentClient};
use dd_test_core::engine::{self, Action, RawResult, Session, TestInfo};
use dd_test_core::protocol::Hello;
use dd_test_core::{debug, warn};
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const RUSTC_VERSION: &str = match option_env!("DD_TEST_RUSTC_VERSION") {
    Some(version) => version,
    None => "unknown",
};
const LIBRARY_VERSION: &str = match option_env!("DD_TEST_LIBRARY_VERSION") {
    Some(version) => version,
    None => "unknown",
};
const AGENT_PATH: Option<&str> = option_env!("DD_TEST_AGENT_PATH");

fn flag(name: &str) -> Option<bool> {
    match std::env::var(name).ok()?.trim().to_ascii_lowercase().as_str() {
        "1" | "true" => Some(true),
        "0" | "false" => Some(false),
        _ => None,
    }
}

fn enabled() -> bool {
    dd_test_core::log::set_debug(flag("DD_TRACE_DEBUG").unwrap_or(false));
    flag("DD_CIVISIBILITY_ENABLED").unwrap_or(true)
}

/// The session finishes after libtest ran every test, instead of in the last test.
const FINISH_AFTER_RUN: bool = cfg!(any(dd_test_exit_callback, dd_test_env_args));

#[cfg(not(dd_test_env_args))]
pub fn test_main_static(tests: &[&TestDescAndFn]) {
    if !enabled() {
        return real::test_main_static(tests);
    }
    let args: Vec<String> = std::env::args().collect();
    let mut owned: Vec<TestDescAndFn> = tests.iter().map(|t| make_owned(t)).collect();
    let session = prepare(&args, &mut owned);
    execute(&args, owned, None, session);
}

#[cfg(not(dd_test_env_args))]
pub fn test_main(args: &[String], mut tests: Vec<TestDescAndFn>, options: Option<Options>) {
    if !enabled() {
        return real::test_main(args, tests, options);
    }
    let session = prepare(args, &mut tests);
    execute(args, tests, options, session);
}

#[cfg(dd_test_env_args)]
pub fn test_main_env_args(tests: &[&TestDescAndFn]) -> ExitCode {
    if !enabled() {
        return real::test_main_env_args(tests);
    }
    let args: Vec<String> = std::env::args().collect();
    test_main(&args, tests)
}

#[cfg(dd_test_env_args)]
pub fn test_main(args: &[String], tests: &[&TestDescAndFn]) -> ExitCode {
    if !enabled() {
        return real::test_main(args, tests);
    }
    let mut owned: Vec<TestDescAndFn> = tests.iter().map(|t| (*t).clone()).collect();
    let Some(session) = prepare(args, &mut owned) else {
        return real::test_main(args, tests);
    };
    let code = real::test_main(args, &owned.iter().collect::<Vec<_>>());
    session.finish();
    code
}

#[cfg(not(dd_test_env_args))]
fn make_owned(test: &TestDescAndFn) -> TestDescAndFn {
    let testfn = match test.testfn {
        StaticTestFn(f) => StaticTestFn(f),
        StaticBenchFn(f) => StaticBenchFn(f),
        _ => panic!("non-static tests passed to test::test_main_static"),
    };
    TestDescAndFn {
        desc: test.desc.clone(),
        testfn,
    }
}

fn start_session(crate_name: &str, working_dir: &Path) -> Result<Session, String> {
    let agent = locate_agent(AGENT_PATH).ok_or("the dd-test-wrapper agent was not found")?;
    let client = AgentClient::spawn(&agent)?;
    let hello = Hello {
        module: std::env::var("CARGO_PKG_NAME")
            .ok()
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| crate_name.to_string()),
        crate_name: crate_name.to_string(),
        binary: std::env::current_exe()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default(),
        rustc_version: RUSTC_VERSION.to_string(),
        library_version: LIBRARY_VERSION.to_string(),
        command: command(),
        working_dir: working_dir.to_string_lossy().to_string(),
        parent_pid: parent_pid(),
    };
    Session::start(&hello, Box::new(client))
}

/// Instruments the tests libtest is about to run. Returns `None`, with the
/// tests unchanged, when they run without Datadog.
fn prepare(args: &[String], tests: &mut [TestDescAndFn]) -> Option<Arc<Session>> {
    let opts = match real::test::parse_opts(args) {
        Some(Ok(opts)) if !opts.list && !opts.bench_benchmarks => opts,
        _ => return None,
    };

    let selected: Vec<usize> = (0..tests.len()).filter(|i| is_selected(&opts, &tests[*i])).collect();
    if selected.is_empty() {
        debug!("no test matches the filters, nothing is reported");
        return None;
    }

    let crate_name = crate_name();
    let working_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let session = match start_session(&crate_name, &working_dir) {
        Ok(session) => Arc::new(session),
        Err(error) => {
            warn!("tests are not reported to Datadog: {error}");
            return None;
        }
    };

    let infos: Vec<TestInfo> = selected
        .iter()
        .map(|i| test_info(&tests[*i].desc, &opts, &crate_name, &working_dir, session.repo_root()))
        .collect();
    let plans = session.plan(&infos);

    for ((index, info), plan) in selected.into_iter().zip(infos).zip(plans) {
        let test = &mut tests[index];
        match &plan.action {
            Action::Skip { disabled, .. } => {
                session.report_skip(&info, &plan);
                if *disabled {
                    disable(test, &opts);
                }
            }
            Action::Run => {
                let original = std::mem::replace(&mut test.testfn, dyn_test_fn(|| Ok(())));
                test.testfn = instrument(original, Arc::clone(&session), info, plan);
            }
        }
    }

    if session.pending() == 0 {
        session.finish();
    }
    Some(session)
}

#[cfg(all(dd_test_exit_callback, not(dd_test_env_args)))]
fn execute(args: &[String], tests: Vec<TestDescAndFn>, options: Option<Options>, session: Option<Arc<Session>>) {
    match session {
        Some(session) => real::test_main_with_exit_callback(args, tests, options, move || session.finish()),
        None => real::test_main(args, tests, options),
    }
}

#[cfg(not(any(dd_test_exit_callback, dd_test_env_args)))]
fn execute(args: &[String], tests: Vec<TestDescAndFn>, options: Option<Options>, _session: Option<Arc<Session>>) {
    real::test_main(args, tests, options);
}

type Call = Box<dyn FnMut() -> Result<(), String> + Send>;

/// The test function, and whether it can run more than once.
#[cfg(not(dd_test_env_args))]
fn callable(testfn: TestFn) -> Result<(bool, Call), TestFn> {
    match testfn {
        StaticTestFn(f) => Ok((true, Box::new(f))),
        DynTestFn(f) => {
            let mut f = Some(f);
            Ok((false, Box::new(move || f.take().map_or(Ok(()), |f| f()))))
        }
        other => Err(other),
    }
}

#[cfg(dd_test_env_args)]
fn callable(testfn: TestFn) -> Result<(bool, Call), TestFn> {
    match testfn {
        StaticTestFn(f) => Ok((true, Box::new(f))),
        DynTestFn(f) => Ok((true, Box::new(move || f()))),
        other => Err(other),
    }
}

#[cfg(not(dd_test_env_args))]
fn dyn_test_fn(f: impl Fn() -> Result<(), String> + Send + Sync + 'static) -> TestFn {
    DynTestFn(Box::new(f))
}

#[cfg(dd_test_env_args)]
fn dyn_test_fn(f: impl Fn() -> Result<(), String> + Send + Sync + 'static) -> TestFn {
    DynTestFn(Arc::new(f))
}

fn instrument(original: TestFn, session: Arc<Session>, info: TestInfo, plan: engine::TestPlan) -> TestFn {
    let (retryable, call) = match callable(original) {
        Ok(callable) => callable,
        Err(other) => return other,
    };
    let call = Mutex::new(call);
    dyn_test_fn(move || {
        let raw: RawResult = session.run(&info, &plan, retryable, &mut || {
            let mut call = call.lock().unwrap_or_else(|e| e.into_inner());
            catch_unwind(AssertUnwindSafe(&mut *call))
        });
        if session.complete_one() && !FINISH_AFTER_RUN {
            session.finish();
        }
        match raw {
            Ok(result) => result,
            Err(payload) => resume_unwind(payload),
        }
    })
}

/// Skips a disabled test, including when `--ignored` or `--include-ignored` would run it.
fn disable(test: &mut TestDescAndFn, opts: &real::test::TestOpts) {
    if matches!(opts.run_ignored, RunIgnored::No) {
        test.desc.ignore = true;
        test.desc.ignore_message = Some(engine::DISABLED_SKIP_REASON);
    } else {
        test.desc.should_panic = ShouldPanic::No;
        test.testfn = dyn_test_fn(|| Ok(()));
    }
}

/// Mirrors libtest's filtering so we know which tests this invocation selects.
fn is_selected(opts: &real::test::TestOpts, test: &TestDescAndFn) -> bool {
    if !matches!(test.testfn, StaticTestFn(_) | DynTestFn(_)) {
        return false;
    }
    let name = test.desc.name.as_slice();
    let matches = |filter: &String| {
        if opts.filter_exact {
            name == filter.as_str()
        } else {
            name.contains(filter.as_str())
        }
    };
    if !opts.filters.is_empty() && !opts.filters.iter().any(matches) {
        return false;
    }
    if opts.skip.iter().any(matches) {
        return false;
    }
    if opts.exclude_should_panic && !matches!(test.desc.should_panic, ShouldPanic::No) {
        return false;
    }
    !(matches!(opts.run_ignored, RunIgnored::Only) && !test.desc.ignore)
}

fn test_info(
    desc: &TestDesc,
    opts: &real::test::TestOpts,
    crate_name: &str,
    working_dir: &Path,
    repo_root: Option<&Path>,
) -> TestInfo {
    let (suite, name) = engine::split_test_name(crate_name, desc.name.as_slice());
    TestInfo {
        suite,
        name,
        source_file: source_file(desc.source_file, working_dir, repo_root),
        start_line: desc.start_line as u32,
        end_line: desc.end_line as u32,
        should_panic: match desc.should_panic {
            ShouldPanic::No => engine::ShouldPanic::No,
            ShouldPanic::Yes => engine::ShouldPanic::Yes,
            ShouldPanic::YesWithMessage(message) => engine::ShouldPanic::YesWithMessage(message.to_string()),
        },
        ignored: desc.ignore && matches!(opts.run_ignored, RunIgnored::No),
        ignore_message: desc.ignore_message.map(str::to_string),
    }
}

/// libtest source paths are relative to the directory Cargo ran rustc in (the
/// workspace root). Resolve them against the package directory's ancestors.
fn source_file(path: &str, working_dir: &Path, repo_root: Option<&Path>) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    let path = Path::new(path);
    let absolute = if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        working_dir
            .ancestors()
            .map(|base| base.join(path))
            .find(|candidate| candidate.is_file())
    };
    let relative = absolute
        .as_deref()
        .and_then(|p| p.strip_prefix(repo_root?).ok())
        .unwrap_or(path);
    Some(relative.to_string_lossy().replace('\\', "/"))
}

/// The crate name, from the `<crate>-<metadata hash>` executable name Cargo uses.
fn crate_name() -> String {
    let stem = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().to_string()))
        .unwrap_or_else(|| "tests".to_string());
    match stem.rsplit_once('-') {
        Some((name, hash)) if hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()) => name.to_string(),
        _ => stem,
    }
}

/// The command that started the tests, usually `cargo test ...`, read from the parent process.
fn command() -> String {
    parent_command().unwrap_or_else(|| {
        let mut args = std::env::args();
        let program = args.next().map(|p| file_name(&p)).unwrap_or_default();
        std::iter::once(program).chain(args).collect::<Vec<_>>().join(" ")
    })
}

#[cfg(unix)]
fn parent_command() -> Option<String> {
    let parent = std::os::unix::process::parent_id().to_string();
    let output = std::process::Command::new("ps")
        .args(["-o", "command=", "-p", &parent])
        .output()
        .ok()?;
    let line = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let (program, rest) = line.split_once(' ').unwrap_or((&line, ""));
    let program = file_name(program);
    (output.status.success() && program.starts_with("cargo")).then(|| format!("{program} {rest}").trim().to_string())
}

#[cfg(not(unix))]
fn parent_command() -> Option<String> {
    None
}

#[cfg(unix)]
fn parent_pid() -> Option<u32> {
    Some(std::os::unix::process::parent_id())
}

#[cfg(not(unix))]
fn parent_pid() -> Option<u32> {
    None
}

fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string())
}
