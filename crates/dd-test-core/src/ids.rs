// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn new_id() -> u64 {
    static STATE: OnceLock<RandomState> = OnceLock::new();
    let state = STATE.get_or_init(RandomState::new);
    loop {
        let mut hasher = state.build_hasher();
        hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        hasher.write_u32(std::process::id());
        let id = hasher.finish() & (i64::MAX as u64);
        if id != 0 {
            return id;
        }
    }
}

pub fn now_nanos() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    #[test]
    fn ids_are_unique_positive_i64() {
        let ids: std::collections::HashSet<u64> = (0..10_000).map(|_| super::new_id()).collect();
        assert_eq!(ids.len(), 10_000);
        assert!(ids.iter().all(|id| *id > 0 && *id <= i64::MAX as u64));
    }
}
