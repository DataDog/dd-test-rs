// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

#[test]
fn adds() {
    assert_eq!(basic::add(2, 3), 5);
}

#[test]
fn fails() {
    assert_eq!(basic::add(1, 1), 3, "math is broken");
}

mod group {
    #[test]
    fn grouped() {}
}
