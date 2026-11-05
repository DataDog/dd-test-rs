// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Datadog Test Optimization core, compiled into every instrumented test binary.
//!
//! This crate only depends on `std`. The wrapper compiles it with the same
//! `rustc` as the tests it instruments, so it must build on every supported
//! toolchain without Cargo. Everything that needs the network or other
//! dependencies lives in the agent process.

#[macro_use]
pub mod log;
pub mod backend;
pub mod engine;
pub mod ids;
pub mod json;
pub mod panic;
pub mod protocol;
