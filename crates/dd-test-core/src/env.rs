// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

pub fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" => Some(true),
        "0" | "false" => Some(false),
        _ => None,
    }
}

pub fn flag(name: &str) -> Option<bool> {
    parse_bool(&std::env::var(name).ok()?)
}

#[cfg(test)]
mod tests {
    use super::parse_bool;

    #[test]
    fn parses_booleans_ignoring_case_and_whitespace() {
        for value in ["1", "true", "TRUE", " true", "True\n"] {
            assert_eq!(parse_bool(value), Some(true), "{value:?}");
        }
        for value in ["0", "false", "FALSE", " false "] {
            assert_eq!(parse_bool(value), Some(false), "{value:?}");
        }
        for value in ["", "yes", "2", "t"] {
            assert_eq!(parse_bool(value), None, "{value:?}");
        }
    }
}
