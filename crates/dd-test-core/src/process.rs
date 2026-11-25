// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! The command line of another process, read without spawning one.

use std::path::Path;

/// The command line of `pid`, joined like `ps -o command=`, or `None` when the
/// lookup fails or `ps` would escape an argument.
pub fn command(pid: u32) -> Option<String> {
    render(&arguments(pid)?)
}

/// The Cargo command that started `pid`, without the directory of `cargo`, or
/// `None` when it is not Cargo.
#[cfg(unix)]
pub fn cargo_command(pid: u32) -> Option<String> {
    resolve(command(pid), || ps(pid))
}

#[cfg(unix)]
fn resolve(native: Option<String>, ps: impl FnOnce() -> Option<String>) -> Option<String> {
    let line = native.or_else(ps)?;
    let line = line.trim();
    let (program, rest) = line.split_once(' ').unwrap_or((line, ""));
    let program = file_name(program);
    program
        .starts_with("cargo")
        .then(|| format!("{program} {rest}").trim().to_string())
}

#[cfg(unix)]
fn ps(pid: u32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let line = String::from_utf8_lossy(&output.stdout).trim().to_string();
    output.status.success().then_some(line)
}

#[cfg(unix)]
fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string())
}

fn render(args: &[String]) -> Option<String> {
    args.iter()
        .all(|arg| arg.bytes().all(|byte| byte.is_ascii_graphic() || byte == b' '))
        .then(|| args.join(" "))
}

#[cfg(target_os = "macos")]
fn arguments(pid: u32) -> Option<Vec<String>> {
    use std::ffi::{c_int, c_uint, c_void};

    const CTL_KERN: c_int = 1;
    const KERN_ARGMAX: c_int = 8;
    const KERN_PROCARGS2: c_int = 49;

    fn read_sysctl(name: &mut [c_int], value: *mut c_void, size: &mut usize) -> bool {
        extern "C" {
            fn sysctl(
                name: *mut c_int,
                namelen: c_uint,
                oldp: *mut c_void,
                oldlenp: *mut usize,
                newp: *mut c_void,
                newlen: usize,
            ) -> c_int;
        }
        unsafe {
            sysctl(
                name.as_mut_ptr(),
                name.len() as c_uint,
                value,
                size,
                std::ptr::null_mut(),
                0,
            ) == 0
        }
    }

    let pid = c_int::try_from(pid).ok()?;
    let mut argmax = c_int::default();
    let mut size = std::mem::size_of::<c_int>();
    if !read_sysctl(
        &mut [CTL_KERN, KERN_ARGMAX],
        std::ptr::from_mut(&mut argmax).cast(),
        &mut size,
    ) {
        return None;
    }
    let mut buffer = vec![0; usize::try_from(argmax).ok()?];
    let mut size = buffer.len();
    if !read_sysctl(
        &mut [CTL_KERN, KERN_PROCARGS2, pid],
        buffer.as_mut_ptr().cast(),
        &mut size,
    ) {
        return None;
    }
    parse_procargs2(buffer.get(..size)?)
}

/// Parses `KERN_PROCARGS2`: the argument count, the executable path, the
/// arguments and then the environment.
#[cfg(target_os = "macos")]
fn parse_procargs2(buffer: &[u8]) -> Option<Vec<String>> {
    let argc = i32::from_ne_bytes(buffer.get(..4)?.try_into().ok()?);
    let argc = usize::try_from(argc).ok().filter(|argc| *argc > 0)?;
    let mut rest = buffer.get(4..)?;
    let end = rest.iter().position(|byte| *byte == 0)?;
    rest = rest.get(end + 1..)?;
    while let [0, tail @ ..] = rest {
        rest = tail;
    }
    let mut args = Vec::new();
    for _ in 0..argc {
        let end = rest.iter().position(|byte| *byte == 0)?;
        args.push(String::from_utf8(rest.get(..end)?.to_vec()).ok()?);
        rest = rest.get(end + 1..)?;
    }
    Some(args)
}

#[cfg(target_os = "linux")]
fn arguments(pid: u32) -> Option<Vec<String>> {
    parse_cmdline(&std::fs::read(format!("/proc/{pid}/cmdline")).ok()?)
}

#[cfg(target_os = "linux")]
fn parse_cmdline(bytes: &[u8]) -> Option<Vec<String>> {
    let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
    if bytes.is_empty() {
        return None;
    }
    bytes
        .split(|byte| *byte == 0)
        .map(|arg| String::from_utf8(arg.to_vec()).ok())
        .collect()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn arguments(_pid: u32) -> Option<Vec<String>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_args(args: &[&str]) -> Option<String> {
        render(&args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn renders_only_arguments_that_ps_prints_as_they_are() {
        assert_eq!(
            render_args(&["cargo", "test --lib", ""]),
            Some("cargo test --lib ".to_string())
        );
        for escaped in ["caf\u{e9}", "tab\there", "line\nbreak", "del\u{7f}"] {
            assert_eq!(render_args(&["cargo", escaped]), None, "{escaped:?}");
        }
    }

    #[cfg(target_os = "macos")]
    fn procargs2(argc: i32, executable: &str, args: &[&str], environment: &[&str]) -> Vec<u8> {
        let mut buffer = argc.to_ne_bytes().to_vec();
        buffer.extend(executable.as_bytes());
        buffer.extend([0; 6]);
        for arg in args.iter().chain(environment) {
            buffer.extend(arg.as_bytes());
            buffer.push(0);
        }
        buffer
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parses_the_arguments_of_a_process() {
        let buffer = procargs2(2, "/bin/sleep", &["/bin/sleep", "5"], &["PATH=/usr/bin"]);
        let args = parse_procargs2(&buffer).unwrap();
        assert_eq!(args, ["/bin/sleep", "5"], "the environment is not an argument");
        assert_eq!(render(&args), Some("/bin/sleep 5".to_string()));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn keeps_arguments_that_are_empty_or_hold_spaces() {
        let buffer = procargs2(3, "cargo", &["cargo", "", "test --lib"], &[]);
        let args = parse_procargs2(&buffer).unwrap();
        assert_eq!(args, ["cargo", "", "test --lib"]);
        assert_eq!(render(&args), Some("cargo  test --lib".to_string()));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn rejects_a_truncated_or_garbage_buffer() {
        let buffer = procargs2(2, "/bin/sleep", &["/bin/sleep", "5"], &[]);
        assert_eq!(parse_procargs2(&[]), None);
        assert_eq!(parse_procargs2(&[0; 3]), None);
        assert_eq!(parse_procargs2(&0i32.to_ne_bytes()), None, "no arguments");
        assert_eq!(parse_procargs2(&(-1i32).to_ne_bytes()), None, "a negative count");
        assert_eq!(parse_procargs2(b"\x01\x00\x00\x00"), None, "no executable path");
        for end in 0..buffer.len() {
            assert_eq!(parse_procargs2(&buffer[..end]), None, "truncated at {end}");
        }
        assert!(parse_procargs2(&buffer).is_some());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn rejects_more_arguments_than_the_buffer_holds() {
        let buffer = procargs2(2, "cargo", &["cargo"], &[]);
        assert_eq!(parse_procargs2(&buffer), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_the_arguments_of_a_process() {
        let args = parse_cmdline(b"/bin/sleep\x005\x00").unwrap();
        assert_eq!(args, ["/bin/sleep", "5"]);
        assert_eq!(render(&args), Some("/bin/sleep 5".to_string()));
        assert_eq!(
            parse_cmdline(b"/bin/sleep"),
            Some(vec!["/bin/sleep".to_string()]),
            "the last argument needs no terminator"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn keeps_arguments_that_are_empty_or_hold_spaces() {
        let args = parse_cmdline(b"cargo\x00\x00test --lib\x00").unwrap();
        assert_eq!(args, ["cargo", "", "test --lib"]);
        assert_eq!(render(&args), Some("cargo  test --lib".to_string()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn rejects_a_truncated_or_garbage_buffer() {
        assert_eq!(parse_cmdline(b""), None);
        assert_eq!(parse_cmdline(b"\x00"), None, "an exited process has no arguments");
        assert_eq!(parse_cmdline(b"cargo\x00\xff\x00"), None, "arguments are not UTF-8");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn reads_the_arguments_of_a_child_process() {
        use std::io::{BufRead, BufReader, Write};

        let mut child = std::process::Command::new("cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        let mut stdin = child.stdin.take().unwrap();
        writeln!(stdin, "cat has started").unwrap();
        // The kernel fills in the child's cmdline only once exec is nearly done, and
        // posix_spawn resumes the parent before that. cat echoing a line proves it runs.
        let mut echo = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut echo)
            .unwrap();
        assert_eq!(echo, "cat has started\n");
        let native = command(pid);
        let ps = std::process::Command::new("ps")
            .args(["-o", "command=", "-p", &pid.to_string()])
            .output();
        drop(stdin);
        child.wait().unwrap();
        let native = native.expect("the native lookup reads a child process");
        assert_eq!(native, "cat");
        let Ok(ps) = ps else { return };
        if !ps.status.success() {
            return;
        }
        assert_eq!(native, String::from_utf8_lossy(&ps.stdout).trim());
    }

    #[cfg(unix)]
    fn resolve_line(line: &str) -> Option<String> {
        resolve(Some(line.to_string()), || None)
    }

    #[cfg(unix)]
    #[test]
    fn keeps_cargo_without_its_directory_and_drops_other_commands() {
        assert_eq!(resolve_line("cargo test --lib"), Some("cargo test --lib".to_string()));
        assert_eq!(resolve_line(" /usr/bin/cargo test "), Some("cargo test".to_string()));
        assert_eq!(resolve_line("/usr/bin/cargo"), Some("cargo".to_string()));
        assert_eq!(resolve_line("/usr/bin/python3 -m pytest"), None);
        assert_eq!(resolve_line(""), None);
        assert_eq!(resolve(None, || None), None, "neither lookup found a command");
    }

    #[cfg(unix)]
    #[test]
    fn keeps_the_native_lookup_and_never_runs_ps() {
        let native = Some("cargo test".to_string());
        let resolved = resolve(native.clone(), || panic!("native lookup succeeded"));
        assert_eq!(resolved, native);
    }

    #[cfg(unix)]
    #[test]
    fn falls_back_to_ps_with_the_same_command() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::process::CommandExt;

        // ps reports argv[0], so a cat named cargo prints the line Cargo would.
        let mut child = std::process::Command::new("cat")
            .arg0("/usr/bin/cargo")
            .arg("-")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        let mut stdin = child.stdin.take().unwrap();
        writeln!(stdin, "cat has started").unwrap();
        // posix_spawn resumes the parent before the child has exec'd, and cat
        // echoing a line proves it runs.
        let mut echo = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut echo)
            .unwrap();
        assert_eq!(echo, "cat has started\n");
        let native = resolve(command(pid), || None);
        let fallback = resolve(None, || ps(pid));
        drop(stdin);
        child.wait().unwrap();
        assert_eq!(native.as_deref(), Some("cargo -"));
        let Some(line) = fallback else { return };
        assert_eq!(line, "cargo -");
    }
}
