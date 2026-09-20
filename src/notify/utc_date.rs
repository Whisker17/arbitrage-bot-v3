//! Dependency-free UTC calendar-day arithmetic (WHI-1407).
//!
//! This crate has no `chrono`/`time` dependency (see
//! `rpc_probe::runner::now_unix_label`'s doc comment) — the digest's UTC calendar-day
//! window `[00:00, next 00:00)` only needs Gregorian civil-date <-> days-since-epoch
//! conversion, so this module implements that directly rather than adding a new crate
//! dependency for one seam. The conversion is Howard Hinnant's well-known
//! `days_from_civil` / `civil_from_days` algorithm (proleptic Gregorian, valid for any
//! year — this module only exercises the post-1970 range the ledger can ever produce).

use std::fmt;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum UtcDateError {
    #[error("date string {0:?} is not YYYY-MM-DD")]
    Malformed(String),
    #[error("date {year:04}-{month:02}-{day:02} is not a valid Gregorian calendar date")]
    OutOfRange { year: i64, month: u32, day: u32 },
}

/// A UTC calendar day (Gregorian civil date). `year` may be any value the ledger's
/// `recorded_at_unix`/`block_timestamp` fields can map to; the pre-1970 range is
/// unreachable in practice but the arithmetic itself is not artificially restricted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UtcDay {
    year: i64,
    month: u32,
    day: u32,
}

impl UtcDay {
    /// Validates `year-month-day` is a real Gregorian calendar date.
    pub fn new(year: i64, month: u32, day: u32) -> Result<Self, UtcDateError> {
        if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
            return Err(UtcDateError::OutOfRange { year, month, day });
        }
        Ok(Self { year, month, day })
    }

    /// Parses a strict `YYYY-MM-DD` string (4-digit year, zero-padded month/day).
    pub fn parse(s: &str) -> Result<Self, UtcDateError> {
        let bytes = s.as_bytes();
        let malformed = || UtcDateError::Malformed(s.to_string());
        if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
            return Err(malformed());
        }
        let all_digits = |slice: &[u8]| slice.iter().all(|b| b.is_ascii_digit());
        if !all_digits(&bytes[0..4]) || !all_digits(&bytes[5..7]) || !all_digits(&bytes[8..10]) {
            return Err(malformed());
        }
        let year: i64 = s[0..4].parse().map_err(|_| malformed())?;
        let month: u32 = s[5..7].parse().map_err(|_| malformed())?;
        let day: u32 = s[8..10].parse().map_err(|_| malformed())?;
        Self::new(year, month, day)
    }

    /// The UTC day containing `unix_secs` (seconds since the Unix epoch).
    pub fn from_unix(unix_secs: u64) -> Self {
        let days = (unix_secs / 86_400) as i64;
        let (year, month, day) = civil_from_days(days);
        Self { year, month, day }
    }

    pub fn year(self) -> i64 {
        self.year
    }
    pub fn month(self) -> u32 {
        self.month
    }
    pub fn day(self) -> u32 {
        self.day
    }

    /// `[since_unix, until_unix)` — the exact `[00:00, next 00:00)` UTC window this day
    /// covers.
    pub fn bounds_unix(self) -> (u64, u64) {
        let since_days = days_from_civil(self.year, self.month, self.day);
        let since = (since_days * 86_400) as u64;
        (since, since + 86_400)
    }

    /// True when `unix_secs` falls within this day's `[00:00, next 00:00)` window.
    pub fn contains_unix(self, unix_secs: u64) -> bool {
        let (since, until) = self.bounds_unix();
        unix_secs >= since && unix_secs < until
    }

    /// The day immediately before this one.
    pub fn previous(self) -> Self {
        let days = days_from_civil(self.year, self.month, self.day) - 1;
        let (year, month, day) = civil_from_days(days);
        Self { year, month, day }
    }

    /// The day immediately after this one.
    pub fn next(self) -> Self {
        let days = days_from_civil(self.year, self.month, self.day) + 1;
        let (year, month, day) = civil_from_days(days);
        Self { year, month, day }
    }
}

impl fmt::Display for UtcDay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Howard Hinnant's `days_from_civil`: days since 1970-01-01 for a valid Gregorian
/// civil date. `y`/`m`/`d` must already be validated (see [`UtcDay::new`]).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let m = m as i64;
    let d = d as i64;
    let mp = (m + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`]: civil date for `z` days since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_well_formed_date() {
        let day = UtcDay::parse("2026-03-01").unwrap();
        assert_eq!((day.year(), day.month(), day.day()), (2026, 3, 1));
    }

    #[test]
    fn rejects_malformed_strings() {
        for bad in [
            "2026-3-1",
            "26-03-01",
            "2026/03/01",
            "not-a-date",
            "2026-03-01x",
            "",
        ] {
            assert!(
                matches!(UtcDay::parse(bad), Err(UtcDateError::Malformed(_))),
                "expected malformed for {bad:?}"
            );
        }
    }

    #[test]
    fn rejects_calendar_impossible_dates() {
        assert!(matches!(
            UtcDay::parse("2026-02-30"),
            Err(UtcDateError::OutOfRange { .. })
        ));
        assert!(matches!(
            UtcDay::parse("2026-13-01"),
            Err(UtcDateError::OutOfRange { .. })
        ));
        assert!(matches!(
            UtcDay::parse("2026-00-01"),
            Err(UtcDateError::OutOfRange { .. })
        ));
    }

    #[test]
    fn accepts_leap_day_only_in_leap_years() {
        assert!(UtcDay::parse("2024-02-29").is_ok()); // leap
        assert!(matches!(
            UtcDay::parse("2023-02-29"),
            Err(UtcDateError::OutOfRange { .. })
        )); // not leap
        assert!(UtcDay::parse("2000-02-29").is_ok()); // divisible by 400
        assert!(matches!(
            UtcDay::parse("1900-02-29"),
            Err(UtcDateError::OutOfRange { .. })
        )); // divisible by 100 but not 400
    }

    #[test]
    fn bounds_unix_is_exactly_one_day_wide_and_matches_known_epoch_values() {
        let day = UtcDay::parse("1970-01-01").unwrap();
        assert_eq!(day.bounds_unix(), (0, 86_400));

        // 2026-01-01T00:00:00Z is a known value (cross-checked against `date -u`).
        let day = UtcDay::parse("2026-01-01").unwrap();
        let (since, until) = day.bounds_unix();
        assert_eq!(since, 1_767_225_600);
        assert_eq!(until, since + 86_400);
    }

    #[test]
    fn from_unix_round_trips_through_bounds() {
        let day = UtcDay::parse("2026-06-15").unwrap();
        let (since, until) = day.bounds_unix();
        assert_eq!(UtcDay::from_unix(since), day);
        assert_eq!(UtcDay::from_unix(until - 1), day);
        assert_eq!(UtcDay::from_unix(until), day.next());
    }

    #[test]
    fn contains_unix_is_half_open() {
        let day = UtcDay::parse("2026-06-15").unwrap();
        let (since, until) = day.bounds_unix();
        assert!(day.contains_unix(since));
        assert!(day.contains_unix(until - 1));
        assert!(!day.contains_unix(until));
        assert!(!day.contains_unix(since.saturating_sub(1)));
    }

    #[test]
    fn previous_and_next_cross_month_and_year_boundaries() {
        assert_eq!(
            UtcDay::parse("2026-03-01").unwrap().previous(),
            UtcDay::parse("2026-02-28").unwrap()
        );
        assert_eq!(
            UtcDay::parse("2024-03-01").unwrap().previous(),
            UtcDay::parse("2024-02-29").unwrap()
        );
        assert_eq!(
            UtcDay::parse("2025-12-31").unwrap().next(),
            UtcDay::parse("2026-01-01").unwrap()
        );
        assert_eq!(
            UtcDay::parse("2026-01-01").unwrap().previous(),
            UtcDay::parse("2025-12-31").unwrap()
        );
    }

    #[test]
    fn display_is_zero_padded_iso_date() {
        assert_eq!(
            UtcDay::parse("2026-03-05").unwrap().to_string(),
            "2026-03-05"
        );
    }

    #[test]
    fn ordering_follows_calendar_order() {
        let a = UtcDay::parse("2026-01-31").unwrap();
        let b = UtcDay::parse("2026-02-01").unwrap();
        assert!(a < b);
    }
}
