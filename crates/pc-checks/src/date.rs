//! Calendar maths without a date crate: only what the checks need.

/// A calendar date (UTC, no time of day).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Date {
    pub year: i32,
    pub month: u32,
    pub day: u32,
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
pub fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let y = i64::from(year) - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (i64::from(month) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The inverse of [`days_from_civil`].
pub fn civil_from_days(days: i64) -> Date {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = (yoe + era * 400 + i64::from(month <= 2)) as i32;
    Date { year, month, day }
}

impl Date {
    /// Parse the registry's `DriverDate` text: month-day-year, with `-` or `/` between.
    /// Returns `None` for anything that is not a plausible date.
    pub fn parse_month_day_year(text: &str) -> Option<Date> {
        let mut parts = text.trim().split(['-', '/']);
        let month: u32 = parts.next()?.trim().parse().ok()?;
        let day: u32 = parts.next()?.trim().parse().ok()?;
        let year: i32 = parts.next()?.trim().parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Date::new(year, month, day)
    }

    /// `None` unless the date is a real one between 1990 and 2200.
    pub fn new(year: i32, month: u32, day: u32) -> Option<Date> {
        if !(1990..=2200).contains(&year) || !(1..=12).contains(&month) || day == 0 {
            return None;
        }
        let candidate = Date { year, month, day };
        // Rejects 31 February and the like: the round trip changes the date.
        if civil_from_days(days_from_civil(year, month, day)) == candidate {
            Some(candidate)
        } else {
            None
        }
    }

    /// From a Windows FILETIME (100 ns ticks since 1601-01-01).
    pub fn from_filetime(ticks: u64) -> Option<Date> {
        const EPOCH_DIFF_SECS: u64 = 11_644_473_600;
        let secs = (ticks / 10_000_000).checked_sub(EPOCH_DIFF_SECS)?;
        let d = civil_from_days((secs / 86_400) as i64);
        Date::new(d.year, d.month, d.day)
    }

    pub fn days_since_epoch(self) -> i64 {
        days_from_civil(self.year, self.month, self.day)
    }
}

/// `2026-11-16T01:02:47Z` for a Unix time in seconds.
pub fn format_iso8601_utc(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let rem = unix_secs % 86_400;
    let d = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        d.year,
        d.month,
        d.day,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_and_known_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(
            civil_from_days(0),
            Date {
                year: 1970,
                month: 1,
                day: 1
            }
        );
        assert_eq!(
            civil_from_days(11_017),
            Date {
                year: 2000,
                month: 3,
                day: 1
            }
        );
    }

    #[test]
    fn round_trips_over_a_wide_range() {
        for days in (-20_000..60_000).step_by(37) {
            let d = civil_from_days(days);
            assert_eq!(days_from_civil(d.year, d.month, d.day), days);
        }
    }

    #[test]
    fn parses_driver_dates() {
        assert_eq!(
            Date::parse_month_day_year("8-20-2026"),
            Some(Date {
                year: 2026,
                month: 8,
                day: 20
            })
        );
        assert_eq!(
            Date::parse_month_day_year(" 11/5/2024 "),
            Some(Date {
                year: 2024,
                month: 11,
                day: 5
            })
        );
    }

    #[test]
    fn rejects_bad_dates() {
        for bad in [
            "",
            "2-31-2024",
            "13-1-2024",
            "0-1-2024",
            "1-1-1800",
            "1-1",
            "1-1-2024-5",
            "a-b-c",
        ] {
            assert_eq!(Date::parse_month_day_year(bad), None, "{bad}");
        }
    }

    #[test]
    fn filetime_is_read() {
        // 2026-08-20 00:00:00 UTC as a FILETIME.
        let secs = days_from_civil(2026, 8, 20) as u64 * 86_400 + 11_644_473_600;
        assert_eq!(
            Date::from_filetime(secs * 10_000_000),
            Some(Date {
                year: 2026,
                month: 8,
                day: 20
            })
        );
        assert_eq!(Date::from_filetime(0), None);
    }

    #[test]
    fn iso_format() {
        assert_eq!(format_iso8601_utc(0), "1970-01-01T00:00:00Z");
        // 2026-11-16 01:02:47 UTC
        let secs = days_from_civil(2026, 11, 16) as u64 * 86_400 + 3_767;
        assert_eq!(format_iso8601_utc(secs), "2026-11-16T01:02:47Z");
    }
}
