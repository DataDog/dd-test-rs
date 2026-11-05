// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Builds the core and harness rlibs with the exact `rustc` Cargo uses, and
//! caches them by compiler, target and source version.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

include!(concat!(env!("OUT_DIR"), "/sources.rs"));

const HARNESS_CRATE: &str = "dd_test_harness";
const CORE_CRATE: &str = "dd_test_core";
const PROBE_CRATE: &str = "dd_test_probe";

pub struct Request {
    pub rustc: PathBuf,
    pub target: Option<String>,
    pub sysroot: Option<String>,
    pub cache_dir: PathBuf,
    /// Embedded in the harness so test binaries can start the agent.
    pub agent: PathBuf,
}

pub struct Built {
    pub dir: PathBuf,
    pub rlib: PathBuf,
}

pub fn ensure(request: &Request) -> Result<Built, String> {
    let version = rustc_output(&request.rustc, &["-vV"])?;
    let release = version
        .lines()
        .find_map(|l| l.strip_prefix("release: "))
        .ok_or("cannot read the rustc release")?
        .trim()
        .to_string();
    let key_input = format!(
        "{version}\n{:?}\n{:?}\n{}\n{}\n{SOURCE_HASH:x}",
        request.target,
        request.sysroot,
        request.agent.display(),
        env!("CARGO_PKG_VERSION")
    );
    let key = format!("{:016x}", fnv(key_input.as_bytes()));

    let dir = request.cache_dir.join(&key);
    let rlib = dir.join(format!("lib{HARNESS_CRATE}.rlib"));
    if rlib.is_file() {
        return Ok(Built { dir, rlib });
    }

    std::fs::create_dir_all(&request.cache_dir)
        .map_err(|e| format!("creating {}: {e}", request.cache_dir.display()))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let staging = request
        .cache_dir
        .join(format!("{key}.tmp-{}-{nonce}", std::process::id()));
    let result = build(request, &staging, &key, &release);
    if let Err(error) = result {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    if std::fs::rename(&staging, &dir).is_err() {
        let _ = std::fs::remove_dir_all(&staging);
        if !rlib.is_file() {
            return Err(format!("could not publish the harness to {}", dir.display()));
        }
    }
    Ok(Built {
        dir: dir.canonicalize().unwrap_or(dir),
        rlib: rlib.canonicalize().unwrap_or(rlib),
    })
}

fn build(request: &Request, staging: &Path, key: &str, release: &str) -> Result<(), String> {
    let core_src = write_sources(&staging.join("src/core"), CORE_SOURCES)?;
    let harness_src = write_sources(&staging.join("src/harness"), HARNESS_SOURCES)?;
    let metadata = format!("dd-test-{key}");

    let api = if has_libtest_item(request, staging, "test::test_main_env_args")? {
        LibtestApi::EnvArgs
    } else if has_libtest_item(request, staging, "test::test_main_with_exit_callback::<fn()>")? {
        LibtestApi::ExitCallback
    } else {
        LibtestApi::Static
    };

    compile(
        rustc(request)
            .args(["--crate-name", CORE_CRATE, "--crate-type", "rlib", "--edition", "2021"])
            .args([
                "-C",
                "opt-level=1",
                "-C",
                &format!("metadata={metadata}"),
                "--cap-lints",
                "allow",
            ])
            .arg("--out-dir")
            .arg(staging)
            .arg(core_src.join("lib.rs")),
        "core",
    )?;

    compile(
        &mut harness_command(request, staging, &harness_src, &metadata, release, api),
        "harness",
    )?;
    let _ = std::fs::remove_dir_all(staging.join("src"));
    let _ = std::fs::remove_dir_all(staging.join("probe"));
    Ok(())
}

/// The libtest entry points the harness must provide.
#[derive(Clone, Copy)]
enum LibtestApi {
    /// `test_main_static`, which exits the process on failure.
    Static,
    /// `test_main_static`, plus `test_main_with_exit_callback` to run code before exiting.
    ExitCallback,
    /// `test_main_env_args` and `test_main`, which return an `ExitCode`.
    EnvArgs,
}

fn has_libtest_item(request: &Request, staging: &Path, item: &str) -> Result<bool, String> {
    let probe = staging.join("src/probe.rs");
    std::fs::write(
        &probe,
        format!("#![feature(test)]\nextern crate test;\npub fn probe() {{ let _ = {item}; }}\n"),
    )
    .map_err(|e| e.to_string())?;
    Ok(rustc(request)
        .env("RUSTC_BOOTSTRAP", PROBE_CRATE)
        .args([
            "--crate-name",
            PROBE_CRATE,
            "--crate-type",
            "lib",
            "--edition",
            "2021",
            "--emit",
            "metadata",
        ])
        .arg("--out-dir")
        .arg(staging.join("probe"))
        .arg(&probe)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false))
}

fn harness_command(
    request: &Request,
    staging: &Path,
    harness_src: &Path,
    metadata: &str,
    release: &str,
    api: LibtestApi,
) -> Command {
    let mut harness = rustc(request);
    harness
        .env("RUSTC_BOOTSTRAP", HARNESS_CRATE)
        .env("DD_TEST_RUSTC_VERSION", release)
        .env("DD_TEST_LIBRARY_VERSION", env!("CARGO_PKG_VERSION"))
        .env("DD_TEST_AGENT_PATH", &request.agent)
        .args([
            "--crate-name",
            HARNESS_CRATE,
            "--crate-type",
            "rlib",
            "--edition",
            "2021",
        ])
        .args([
            "-C",
            "opt-level=1",
            "-C",
            &format!("metadata={metadata}"),
            "--cap-lints",
            "allow",
        ])
        .arg("--extern")
        .arg(format!(
            "{CORE_CRATE}={}",
            staging.join(format!("lib{CORE_CRATE}.rlib")).display()
        ))
        .arg("-L")
        .arg(format!("dependency={}", staging.display()))
        .arg("--out-dir")
        .arg(staging)
        .arg(harness_src.join("lib.rs"));
    match api {
        LibtestApi::Static => {}
        LibtestApi::ExitCallback => {
            harness.args(["--cfg", "dd_test_exit_callback"]);
        }
        LibtestApi::EnvArgs => {
            harness.args(["--cfg", "dd_test_env_args"]);
        }
    }
    harness
}

fn rustc(request: &Request) -> Command {
    let mut command = Command::new(&request.rustc);
    command.env_remove("RUSTC_BOOTSTRAP");
    if let Some(target) = &request.target {
        command.args(["--target", target]);
    }
    if let Some(sysroot) = &request.sysroot {
        command.args(["--sysroot", sysroot]);
    }
    command
}

fn compile(command: &mut Command, what: &str) -> Result<(), String> {
    let output = command
        .output()
        .map_err(|e| format!("running rustc for the {what}: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "building the {what} failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

fn write_sources(root: &Path, files: &[(&str, &str)]) -> Result<PathBuf, String> {
    for (relative, contents) in files {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, contents).map_err(|e| format!("writing {}: {e}", path.display()))?;
    }
    Ok(root.to_path_buf())
}

fn rustc_output(rustc: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new(rustc)
        .args(args)
        .output()
        .map_err(|e| format!("running {}: {e}", rustc.display()))?;
    if !output.status.success() {
        return Err(format!("{} {} failed", rustc.display(), args.join(" ")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ *byte as u64).wrapping_mul(0x100_0000_01b3)
    })
}
