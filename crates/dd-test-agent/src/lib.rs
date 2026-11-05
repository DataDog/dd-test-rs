// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! The agent process behind instrumented test binaries.
//!
//! The harness inside each test binary starts `dd-test-wrapper agent` and
//! streams test results to it over stdin. The agent owns configuration, CI
//! and git metadata, backend requests, event encoding and transport.

pub mod api;
pub mod ci;
pub mod config;
pub mod event;
pub mod read_cache;
pub use dd_test_core::ids;
pub mod server;
pub mod session;
pub mod telemetry;
pub mod transport;
pub mod uploader;

/// Runs the agent on the process's stdin and stdout.
pub fn run() -> i32 {
    let stdin = std::io::stdin();
    server::serve(stdin.lock(), std::io::stdout())
}
