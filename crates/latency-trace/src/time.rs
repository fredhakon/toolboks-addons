//! Timestamp parsing. All times inside the crate are counted in 100-nanosecond ticks since
//! 1601-01-01 (the Windows FILETIME scale), which is what `InitialTime`, `StartTime` and
//! `EndTime` use in tracerpt output.

/// 100-nanosecond ticks in one minute.
pub(crate) const MINUTE: i64 = 600_000_000;
/// FILETIME of 1970-01-01T00:00:00.
const UNIX_EPOCH_FILETIME: i64 = 116_444_736_000_000_000;

/// Parses the `SystemTime` attribute of `<TimeCreated>` as a wall-clock FILETIME.
///
/// tracerpt writes the session's local wall-clock time followed by an offset, and the offset is
/// not reliable (it printed `+00:59` for a UTC+1 machine during development), so the offset is
/// ignored here. The caller works out the real difference to UTC from the trace itself.
/// Accepts `YYYY-MM-DDTHH:MM:SS[.fraction][anything]`; returns `None` for anything else.
pub(crate) fn parse_wall(s: &str) -> Option<u64> {
    let b = s.trim().as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b' ') {
        return None;
    }
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<i64> {
        let mut v: i64 = 0;
        for &c in &b[from..to] {
            if !c.is_ascii_digit() {
                return None;
            }
            v = v * 10 + i64::from(c - b'0');
        }
        Some(v)
    };
    let (year, month, day) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (hour, min, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1601..=9999).contains(&year) || !(1..=12).contains(&month) {
        return None;
    }
    if day < 1 || day > days_in_month(year, month) || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    // Fraction of a second: up to 7 digits count (100 ns); further digits are ignored.
    let mut frac: i64 = 0;
    if b.len() > 19 && b[19] == b'.' {
        let mut digits = 0;
        for &c in &b[20..] {
            if !c.is_ascii_digit() {
                break;
            }
            if digits < 7 {
                frac = frac * 10 + i64::from(c - b'0');
            }
            digits += 1;
        }
        if digits == 0 {
            return None;
        }
        for _ in digits..7 {
            frac *= 10;
        }
    }
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3600 + min * 60 + sec;
    let ft = secs
        .checked_mul(10_000_000)?
        .checked_add(frac)?
        .checked_add(UNIX_EPOCH_FILETIME)?;
    u64::try_from(ft).ok()
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Rounds a tick difference to the nearest whole minute.
pub(crate) fn round_to_minute(delta: i64) -> i64 {
    delta.saturating_add(MINUTE / 2).div_euclid(MINUTE) * MINUTE
}

/// Parses a number as tracerpt prints it: decimal (possibly padded with spaces) or `0x` hex.
/// Negative, empty, overlong or non-numeric text gives `None`.
pub(crate) fn parse_u64(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        if hex.is_empty() || hex.len() > 16 {
            return None;
        }
        return u64::from_str_radix(hex, 16).ok();
    }
    if s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wall_time_matches_the_header_start_time_seen_in_real_tracerpt_output() {
        // From a real tracerpt run (non-kernel trace): TimeCreated was local wall-clock time and
        // the header StartTime (UTC) was 134388816235664517 = 14:40:23.5664517 UTC, one hour less.
        let wall = parse_wall("2026-11-11T15:40:23.566451700+00:59").unwrap();
        let utc = 134_388_816_235_664_517_i64;
        assert_eq!(wall as i64 - utc, 60 * MINUTE);
        assert_eq!(round_to_minute(wall as i64 - utc), 60 * MINUTE);
    }

    #[test]
    fn rejects_bad_times() {
        for bad in [
            "",
            "2026-13-01T00:00:00Z",
            "2026-02-30T00:00:00Z",
            "1500-01-01T00:00:00Z",
            "-001-01-01T00:00:00Z",
            "2026-11-11T25:00:00Z",
            "2026-11-11T10:00:00.",
            "garbage that is long enough",
        ] {
            assert_eq!(parse_wall(bad), None, "{bad}");
        }
        assert_eq!(parse_wall("1601-01-01T00:00:00Z"), Some(0));
    }

    #[test]
    fn numbers() {
        assert_eq!(parse_u64("    4096"), Some(4096));
        assert_eq!(parse_u64("0xFFFFF80612345678"), Some(0xFFFF_F806_1234_5678));
        assert_eq!(parse_u64("-5"), None);
        assert_eq!(parse_u64("99999999999999999999999"), None);
        assert_eq!(parse_u64("0x10000000000000000"), None);
        assert_eq!(parse_u64(""), None);
        assert_eq!(round_to_minute(-1), 0);
        assert_eq!(round_to_minute(MINUTE / 2 + 1), MINUTE);
    }
}
