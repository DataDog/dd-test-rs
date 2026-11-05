// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! `RUSTC_WORKSPACE_WRAPPER` that injects the Datadog test harness into `--test` builds.
//!
//! Cargo runs `dd-test-wrapper <rustc> <args>...`. Every compilation passes
//! through unchanged, except test harness builds, which get
//! `--extern test=<harness>` so the generated `main` calls our libtest shim.
//! Instrumented test binaries run `dd-test-wrapper agent` to report results.

mod harness;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let Some(rustc) = args.next() else {
        eprintln!("usage: RUSTC_WORKSPACE_WRAPPER=dd-test-wrapper cargo test");
        return ExitCode::from(2);
    };
    if rustc == "agent" {
        return ExitCode::from(dd_test_agent::run().clamp(0, 255) as u8);
    }
    let mut args: Vec<OsString> = args.collect();
    let Some(extra) = injection(&rustc, &args) else {
        return run(Command::new(rustc).args(args));
    };
    let dep_info = dep_info_path(&args);
    args.extend(extra);
    let status = match Command::new(rustc).args(args).status() {
        Ok(status) => status,
        Err(error) => {
            eprintln!("dd-test-wrapper: failed to run rustc: {error}");
            return ExitCode::from(1);
        }
    };
    if status.success() {
        if let Some(path) = dep_info {
            track_wrapper(&path);
        }
    }
    ExitCode::from(status.code().unwrap_or(1).clamp(0, 255) as u8)
}

/// The dep-info file Cargo asks rustc to write: `<out-dir>/<crate-name><extra-filename>.d`.
fn dep_info_path(args: &[OsString]) -> Option<PathBuf> {
    let args: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    let out_dir = value(&args, "--out-dir")?;
    let crate_name = value(&args, "--crate-name")?;
    let extra = args
        .iter()
        .find_map(|a| a.strip_prefix("extra-filename="))
        .unwrap_or("");
    Some(PathBuf::from(out_dir).join(format!("{crate_name}{extra}.d")))
}

/// Cargo does not see the injected harness. Listing the wrapper as a source of
/// the test binary makes Cargo rebuild instrumented tests when the wrapper changes.
/// Cargo only reads the first rule of the file, so the path goes there.
fn track_wrapper(dep_info: &std::path::Path) {
    let (Ok(contents), Ok(wrapper)) = (std::fs::read_to_string(dep_info), std::env::current_exe()) else {
        return;
    };
    let wrapper = wrapper
        .canonicalize()
        .unwrap_or(wrapper)
        .to_string_lossy()
        .replace(' ', "\\ ");
    let (first, rest) = contents.split_once('\n').unwrap_or((&contents, ""));
    let _ = std::fs::write(dep_info, format!("{first} {wrapper}\n{rest}"));
}

fn debug(message: impl std::fmt::Display) {
    if std::env::var("DD_TRACE_DEBUG").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true")) {
        eprintln!("[dd-test-wrapper] {message}");
    }
}

/// Arguments to add to this compilation, if it builds a libtest harness we can instrument.
fn injection(rustc: &OsString, args: &[OsString]) -> Option<Vec<OsString>> {
    let args: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    if !args.iter().any(|a| a == "--test") {
        return None;
    }
    let crate_name = value(&args, "--crate-name").unwrap_or_default();
    if let Some(root) = crate_root(&args) {
        if std::fs::read_to_string(&root).is_ok_and(|s| uses_custom_test_runner(&s)) {
            debug(format!("{crate_name}: custom test runner, not instrumented"));
            return None;
        }
    }
    let out_dir = value(&args, "--out-dir")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let agent = match std::env::current_exe() {
        Ok(exe) => exe.canonicalize().unwrap_or(exe),
        Err(error) => {
            eprintln!(
                "warning: Datadog test harness unavailable for {crate_name}, tests are not instrumented: {error}"
            );
            return None;
        }
    };
    let request = harness::Request {
        rustc: PathBuf::from(rustc),
        target: value(&args, "--target"),
        sysroot: value(&args, "--sysroot"),
        cache_dir: std::env::var_os("DD_TEST_HARNESS_CACHE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| out_dir.join("..").join(".dd-test-harness")),
        agent,
    };
    match harness::ensure(&request) {
        Ok(built) => {
            debug(format!("{crate_name}: using harness {}", built.rlib.display()));
            Some(vec![
                "--extern".into(),
                format!("test={}", built.rlib.display()).into(),
                "-L".into(),
                format!("dependency={}", built.dir.display()).into(),
            ])
        }
        Err(error) => {
            eprintln!(
                "warning: Datadog test harness unavailable for {crate_name}, tests are not instrumented: {error}"
            );
            None
        }
    }
}

fn value(args: &[String], flag: &str) -> Option<String> {
    let prefix = format!("{flag}=");
    args.iter().enumerate().find_map(|(i, arg)| {
        if arg == flag {
            args.get(i + 1).cloned()
        } else {
            arg.strip_prefix(&prefix).map(str::to_string)
        }
    })
}

fn crate_root(args: &[String]) -> Option<PathBuf> {
    args.iter()
        .find(|a| a.ends_with(".rs") && !a.starts_with('-'))
        .map(PathBuf::from)
}

/// Whether the crate attributes, at the top of the crate root, include
/// `#![test_runner(...)]`, possibly inside `#![cfg_attr(...)]`.
fn uses_custom_test_runner(source: &str) -> bool {
    let mut rest = source.strip_prefix('\u{feff}').unwrap_or(source);
    if let Some(after) = rest.strip_prefix("#!") {
        if !skip_trivia(after).starts_with('[') {
            rest = after.find('\n').map_or("", |end| &after[end..]);
        }
    }
    loop {
        let Some(attribute) = skip_trivia(rest)
            .strip_prefix('#')
            .and_then(|a| skip_trivia(a).strip_prefix('!'))
            .and_then(|a| skip_trivia(a).strip_prefix('['))
        else {
            return false;
        };
        let Some((tokens, tail)) = attribute_tokens(attribute) else {
            return false;
        };
        if is_test_runner(&tokens) {
            return true;
        }
        rest = tail;
    }
}

fn is_test_runner(attribute: &str) -> bool {
    let attribute = attribute.strip_prefix("r#").unwrap_or(attribute);
    attribute.starts_with("test_runner(")
        || attribute
            .strip_prefix("cfg_attr(")
            .and_then(|a| a.strip_suffix(')'))
            .is_some_and(|arguments| top_level_items(arguments).into_iter().skip(1).any(is_test_runner))
}

/// The comma-separated items of `tokens` that are not nested in delimiters.
fn top_level_items(tokens: &str) -> Vec<&str> {
    let mut items = Vec::new();
    let (mut depth, mut start) = (0, 0);
    for (i, c) in tokens.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                items.push(&tokens[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    items.push(&tokens[start..]);
    items
}

/// The attribute up to its closing bracket without whitespace, comments and
/// literal contents, and the source after it.
fn attribute_tokens(attribute: &str) -> Option<(String, &str)> {
    let mut tokens = String::new();
    let mut depth = 0usize;
    let mut rest = attribute;
    loop {
        rest = skip_trivia(rest);
        if let Some(length) = literal_length(rest) {
            tokens.push_str("\"\"");
            rest = &rest[length..];
            continue;
        }
        let c = rest.chars().next()?;
        rest = &rest[c.len_utf8()..];
        match c {
            '[' | '(' | '{' => depth += 1,
            ']' if depth == 0 => return Some((tokens, rest)),
            ']' | ')' | '}' => depth = depth.checked_sub(1)?,
            _ => {}
        }
        tokens.push(c);
    }
}

/// The length of the string or character literal `source` starts with.
fn literal_length(source: &str) -> Option<usize> {
    let prefix = usize::from(source.starts_with(['b', 'c']));
    let literal = &source[prefix..];
    if let Some(body) = literal.strip_prefix('\'') {
        let first = body.chars().next()?;
        let end = if first == '\\' {
            2 + body.get(2..)?.find('\'')?
        } else if body[first.len_utf8()..].starts_with('\'') {
            first.len_utf8()
        } else {
            return None;
        };
        return Some(prefix + 1 + end + 1);
    }
    if let Some(raw) = literal.strip_prefix('r') {
        let hashes = raw.len() - raw.trim_start_matches('#').len();
        let body = raw[hashes..].strip_prefix('"')?;
        let end = body.find(&format!("\"{}", "#".repeat(hashes)))?;
        return Some(prefix + 1 + hashes + 1 + end + 1 + hashes);
    }
    let body = literal.strip_prefix('"')?;
    let mut escaped = false;
    let end = body.find(|c| {
        let closes = c == '"' && !escaped;
        escaped = c == '\\' && !escaped;
        closes
    })?;
    Some(prefix + 1 + end + 1)
}

fn skip_trivia(mut source: &str) -> &str {
    loop {
        source = source.trim_start();
        if let Some(comment) = source.strip_prefix("//") {
            source = comment.find('\n').map_or("", |end| &comment[end..]);
        } else if source.starts_with("/*") {
            source = skip_block_comment(source);
        } else {
            return source;
        }
    }
}

fn skip_block_comment(source: &str) -> &str {
    let bytes = source.as_bytes();
    let (mut i, mut depth) = (0, 0);
    while i + 1 < bytes.len() {
        match &bytes[i..i + 2] {
            b"/*" => (depth, i) = (depth + 1, i + 2),
            b"*/" => {
                (depth, i) = (depth - 1, i + 2);
                if depth == 0 {
                    return &source[i..];
                }
            }
            _ => i += 1,
        }
    }
    ""
}

#[cfg(unix)]
fn run(command: &mut Command) -> ExitCode {
    use std::os::unix::process::CommandExt;
    let error = command.exec();
    eprintln!("dd-test-wrapper: failed to run rustc: {error}");
    ExitCode::from(1)
}

#[cfg(not(unix))]
fn run(command: &mut Command) -> ExitCode {
    match command.status() {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(error) => {
            eprintln!("dd-test-wrapper: failed to run rustc: {error}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn reads_flag_values_in_both_forms() {
        let args = strings(&["--crate-name", "foo", "--target=aarch64-apple-darwin", "src/lib.rs"]);
        assert_eq!(value(&args, "--crate-name").as_deref(), Some("foo"));
        assert_eq!(value(&args, "--target").as_deref(), Some("aarch64-apple-darwin"));
        assert_eq!(value(&args, "--sysroot"), None);
        assert_eq!(crate_root(&args), Some(PathBuf::from("src/lib.rs")));
    }

    #[test]
    fn detects_custom_test_runner_attributes() {
        for source in [
            "#![feature(custom_test_frameworks)]\n#![test_runner(crate::runner)]\n",
            "#! [ test_runner ( crate::runner ) ]",
            "#![cfg_attr(test, test_runner(crate::runner))]",
            "#![no_std]\n#![cfg_attr(all(test, target_os = \"none\"), test_runner(crate::runner))]",
            "//! Docs\n/* note */\n#![doc = \"/*\"]\n#![test_runner(crate::runner)]",
            "#![doc = r#\"a \"]\" b\"#]\n#![doc = \"\\\"]\"]\n#![cfg_attr(test, /* why */ test_runner(crate::runner))]",
            "\u{feff}#![test_runner(crate::runner)]",
            "#![doc = concat!(']', '\\'', '\\u{5d}', b']', \" docs\")]\n#![test_runner(crate::runner)]",
            "#![cfg_attr(test, allow(dead_code), cfg_attr(unix, test_runner(crate::runner)))]",
            "#![no_std]\n# /* why */ ![test_runner(crate::runner)]",
            "#![r#cfg_attr(test, r#test_runner(crate::runner))]",
        ] {
            assert!(uses_custom_test_runner(source), "{source}");
        }
        for source in [
            "// #![test_runner(crate::runner)]\nfn main() {}",
            "/* #![test_runner(crate::runner)] /* nested */ */",
            "fn setup_test_runner() {}\n#[test]\nfn uses() { setup_test_runner(); }",
            "#![cfg_attr(test, allow(dead_code))]\nfn test_runner(_: &[&dyn Fn()]) {}",
            "#!/usr/bin/env rust-script\nfn test_runner() {}",
            "//! #![test_runner(crate::runner)]\nfn main() {}",
            "const EXAMPLE: &str = \"#![test_runner (crate::runner)]\";",
            "#![doc = r#\"#![test_runner(crate::runner)]\"#]",
            "#![doc = \"\\\"]#![test_runner(crate::runner)]\"]",
            "fn main() {}\n#![test_runner(crate::runner)]",
            "#![cfg_attr(test, doc = stringify!(example, test_runner (example)))]",
            "#![cfg_attr(test_runner(x), allow(dead_code))]",
        ] {
            assert!(!uses_custom_test_runner(source), "{source}");
        }
    }
}
