// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use std::sync::atomic::{AtomicUsize, Ordering};

pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

static FLAKY_CALLS: AtomicUsize = AtomicUsize::new(0);

/// Fails the first `BASIC_FLAKY_FAILURES` calls (default 1) in this process.
pub fn flaky() -> bool {
    let failures = std::env::var("BASIC_FLAKY_FAILURES").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    FLAKY_CALLS.fetch_add(1, Ordering::SeqCst) >= failures
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes() {
        assert_eq!(add(1, 2), 3);
    }

    #[test]
    #[should_panic]
    fn panics() {
        panic!("boom");
    }

    #[test]
    #[should_panic(expected = "boom")]
    fn panics_with_message() {
        panic!("big boom");
    }

    #[test]
    fn returns_ok() -> Result<(), String> {
        Ok(())
    }

    #[test]
    #[ignore = "not ready"]
    fn ignored() {}

    #[test]
    fn flaky_once() {
        assert!(flaky(), "flaky failure");
    }

    mod nested {
        #[test]
        fn deep() {}
    }
}
