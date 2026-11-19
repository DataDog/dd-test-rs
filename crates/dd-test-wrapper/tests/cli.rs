// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use std::process::Command;

#[test]
fn prints_the_version() {
    for flag in ["--version", "-V"] {
        let output = Command::new(env!("CARGO_BIN_EXE_dd-test-wrapper"))
            .arg(flag)
            .output()
            .unwrap();
        assert!(output.status.success(), "{flag}");
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("dd-test-wrapper {}\n", env!("CARGO_PKG_VERSION")),
        );
    }
}

#[test]
fn passes_version_flags_after_rustc_to_rustc() {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let output = Command::new(env!("CARGO_BIN_EXE_dd-test-wrapper"))
        .args([&rustc, "--version"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8(output.stdout).unwrap().starts_with("rustc "));
}
