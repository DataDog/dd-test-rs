// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static DEBUG: AtomicBool = AtomicBool::new(false);

pub fn set_debug(enabled: bool) {
    DEBUG.store(enabled, Ordering::Relaxed);
}

pub fn debug_enabled() -> bool {
    DEBUG.load(Ordering::Relaxed)
}

pub fn write(level: &str, message: std::fmt::Arguments<'_>) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "[dd-test {level}] {message}");
}

/// Debug lines carry the time and the process, since the test binaries and
/// agents of one run share stderr.
pub fn write_debug(message: std::fmt::Arguments<'_>) {
    use std::io::Write;
    let _ = writeln!(
        std::io::stderr(),
        "[dd-test debug] {} pid={} {message}",
        timestamp(SystemTime::now()),
        std::process::id()
    );
}

/// RFC 3339 UTC time with milliseconds.
fn timestamp(time: SystemTime) -> String {
    let since_epoch = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = since_epoch.as_secs();
    let (days, time_of_day) = ((seconds / 86_400) as i64, seconds % 86_400);
    let z = days + 719_468;
    let era = z / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        time_of_day / 3600,
        time_of_day / 60 % 60,
        time_of_day % 60,
        since_epoch.subsec_millis()
    )
}

#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {
        if $crate::log::debug_enabled() {
            $crate::log::write_debug(format_args!($($arg)*));
        }
    };
}

#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => {
        $crate::log::write("warn", format_args!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn formats_timestamps_in_utc() {
        let at = |seconds: u64, millis: u64| {
            timestamp(UNIX_EPOCH + Duration::from_secs(seconds) + Duration::from_millis(millis))
        };
        assert_eq!(at(0, 0), "1970-01-01T00:00:00.000Z");
        assert_eq!(at(946_684_799, 5), "1999-12-31T23:59:59.005Z");
        assert_eq!(at(1_709_210_096, 789), "2024-02-29T12:34:56.789Z");
    }
}
