// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_reported_before_exit() {}

#[test]
fn b_exits_the_process() {
    if std::env::var_os("BASIC_EXIT").is_some() {
        std::process::exit(0);
    }
}

#[test]
fn c_never_runs_after_exit() {}
