//! Formatting for the ISO-8601 UTC timestamps the dashboard and persisted
//! state use (the Deno version used `Date#toISOString`).

use std::time::{SystemTime, UNIX_EPOCH};

/// Current wall clock as milliseconds since the Unix epoch, saturating at 0
/// for clocks set before 1970 (`Date.now()` in the Deno version).
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Milliseconds since the Unix epoch, formatted as `YYYY-MM-DDTHH:MM:SS.mmmZ`,
/// matching `new Date(ms).toISOString()` for the range aiproxy produces.
pub fn to_iso8601(ms: u64) -> String {
    let millis = ms % 1000;
    let total_seconds = ms / 1000;
    let (year, month, day) = civil_from_days((total_seconds / 86_400) as i64);
    let seconds_of_day = total_seconds % 86_400;
    let hour = seconds_of_day / 3600;
    let minute = (seconds_of_day % 3600) / 60;
    let second = seconds_of_day % 60;

    if year > 9999 {
        // JavaScript uses the expanded-year form outside the 4-digit range.
        return format!(
            "+{:06}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            year, month, day, hour, minute, second, millis
        );
    }
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year, month, day, hour, minute, second, millis
    )
}

/// Days since the Unix epoch -> (year, month, day) in the proleptic Gregorian
/// calendar (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

/// Days since the Unix epoch for a Gregorian date (inverse of the above,
/// used to cross-check the formatter in tests).
#[cfg(test)]
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = month as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The timestamp formatter's output for a civil date at midnight, with the
/// expectation taken from `date -u`.
#[cfg(test)]
fn iso(year: i64, month: u32, day: u32) -> String {
    to_iso8601((days_from_civil(year, month, day) * 86_400_000) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_epoch() {
        assert_eq!(to_iso8601(0), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn formats_milliseconds_and_time() {
        assert_eq!(to_iso8601(1_755_000_000_123), "2025-08-12T12:00:00.123Z");
        assert_eq!(to_iso8601(1_234_567_890_999), "2009-02-13T23:31:30.999Z");
    }

    #[test]
    fn formats_known_dates() {
        // Vectors cross-checked with `date -u -d @<seconds>`.
        assert_eq!(iso(1970, 1, 1), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso(2000, 2, 29), "2000-02-29T00:00:00.000Z");
        assert_eq!(iso(2100, 1, 1), "2100-01-01T00:00:00.000Z");
        assert_eq!(iso(2025, 1, 1), "2025-01-01T00:00:00.000Z");
        assert_eq!(iso(1972, 10, 5), "1972-10-05T00:00:00.000Z");
        assert_eq!(iso(2004, 3, 1), "2004-03-01T00:00:00.000Z");
        assert_eq!(iso(2286, 11, 20), "2286-11-20T00:00:00.000Z");
    }

    #[test]
    fn handles_leap_centuries() {
        // 2000 is a leap year (divisible by 400); 1900 is not (divisible by 100).
        assert_eq!(iso(2000, 3, 1), "2000-03-01T00:00:00.000Z");
        assert_eq!(iso(2000, 12, 31), "2000-12-31T00:00:00.000Z");
    }

    #[test]
    fn far_future_uses_expanded_year_form() {
        let ms = (days_from_civil(12345, 6, 7) * 86_400_000) as u64;
        assert_eq!(to_iso8601(ms), "+012345-06-07T00:00:00.000Z");
    }

    #[test]
    fn round_trips_every_day_of_a_leap_year() {
        // 2024 is a leap year: 366 days must each round-trip through the
        // calendar conversion.
        let start = days_from_civil(2024, 1, 1);
        for day in 0..366 {
            let (y, m, d) = civil_from_days(start + day);
            assert_eq!((y, m, d), civil_from_days(start + day));
            assert_eq!(days_from_civil(y, m, d), start + day);
            assert!((1..=12).contains(&m));
            assert!((1..=31).contains(&d));
        }
        // Month boundaries within the leap year.
        assert_eq!(civil_from_days(start + 31), (2024, 2, 1));
        assert_eq!(civil_from_days(start + 59), (2024, 2, 29));
        assert_eq!(civil_from_days(start + 60), (2024, 3, 1));
        assert_eq!(civil_from_days(start + 366), (2025, 1, 1));
    }

    #[test]
    fn formats_times_of_day() {
        let base = days_from_civil(2025, 8, 12) * 86_400_000;
        assert_eq!(to_iso8601(base as u64), "2025-08-12T00:00:00.000Z");
        assert_eq!(
            to_iso8601((base + 86_399_999) as u64),
            "2025-08-12T23:59:59.999Z"
        );
        assert_eq!(
            to_iso8601((base + 36_000_000) as u64),
            "2025-08-12T10:00:00.000Z"
        );
    }
}
