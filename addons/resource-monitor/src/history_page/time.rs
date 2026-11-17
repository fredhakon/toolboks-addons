//! Local time for the History page. History is stored in UTC; the page shows and reads the
//! PC's local time (with the daylight saving rule of each date, from Windows).

use crate::recording::disk::{civil, days_from_civil};

pub const SEC: i64 = 1000;
pub const MIN: i64 = 60 * SEC;
pub const HOUR: i64 = 60 * MIN;
pub const DAY: i64 = 24 * HOUR;

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Calendar fields of a time (UTC or local, whichever `ms` is).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fields {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

pub fn fields(ms: i64) -> Fields {
    let (year, month, day) = civil(ms.div_euclid(DAY));
    let in_day = ms.rem_euclid(DAY) / SEC;
    Fields {
        year,
        month,
        day,
        hour: (in_day / 3600) as u32,
        minute: (in_day / 60 % 60) as u32,
        second: (in_day % 60) as u32,
    }
}

pub fn from_fields(f: Fields) -> i64 {
    days_from_civil(f.year, f.month, f.day) * DAY
        + f.hour as i64 * HOUR
        + f.minute as i64 * MIN
        + f.second as i64 * SEC
}

#[cfg(windows)]
mod win {
    use super::{fields, from_fields, Fields};
    use windows::Win32::Foundation::SYSTEMTIME;
    use windows::Win32::System::Time::{
        SystemTimeToTzSpecificLocalTime, TzSpecificLocalTimeToSystemTime,
    };

    fn to_st(ms: i64) -> Option<SYSTEMTIME> {
        let f = fields(ms);
        Some(SYSTEMTIME {
            wYear: u16::try_from(f.year).ok()?,
            wMonth: f.month as u16,
            wDayOfWeek: 0,
            wDay: f.day as u16,
            wHour: f.hour as u16,
            wMinute: f.minute as u16,
            wSecond: f.second as u16,
            wMilliseconds: ms.rem_euclid(1000) as u16,
        })
    }

    fn from_st(st: &SYSTEMTIME) -> i64 {
        from_fields(Fields {
            year: st.wYear as i64,
            month: st.wMonth as u32,
            day: st.wDay as u32,
            hour: st.wHour as u32,
            minute: st.wMinute as u32,
            second: st.wSecond as u32,
        }) + st.wMilliseconds as i64
    }

    pub fn to_local(utc: i64) -> i64 {
        let Some(st) = to_st(utc) else { return utc };
        let mut local = SYSTEMTIME::default();
        // SAFETY: both pointers point at valid SYSTEMTIMEs; no time zone (the PC's own).
        match unsafe { SystemTimeToTzSpecificLocalTime(None, &st, &mut local) } {
            Ok(()) => from_st(&local),
            Err(_) => utc,
        }
    }

    pub fn to_utc(local: i64) -> i64 {
        let Some(st) = to_st(local) else { return local };
        let mut utc = SYSTEMTIME::default();
        // SAFETY: as above.
        match unsafe { TzSpecificLocalTimeToSystemTime(None, &st, &mut utc) } {
            Ok(()) => from_st(&utc),
            Err(_) => local,
        }
    }
}

/// UTC to local wall-clock time (both as ms since 1970).
pub fn to_local(utc: i64) -> i64 {
    #[cfg(windows)]
    return win::to_local(utc);
    #[cfg(not(windows))]
    utc
}

/// Local wall-clock time to UTC.
pub fn to_utc(local: i64) -> i64 {
    #[cfg(windows)]
    return win::to_utc(local);
    #[cfg(not(windows))]
    local
}

/// Local midnight at or before `utc`, as UTC.
pub fn local_midnight(utc: i64) -> i64 {
    let local = to_local(utc);
    to_utc(local - local.rem_euclid(DAY))
}

/// "14:32" (local).
pub fn hm(utc: i64) -> String {
    let f = fields(to_local(utc));
    format!("{:02}:{:02}", f.hour, f.minute)
}

/// "14:32:05" (local).
pub fn hms(utc: i64) -> String {
    let f = fields(to_local(utc));
    format!("{:02}:{:02}:{:02}", f.hour, f.minute, f.second)
}

/// "17 Nov" (local).
pub fn day(utc: i64) -> String {
    let f = fields(to_local(utc));
    format!("{} {}", f.day, MONTHS[(f.month - 1) as usize])
}

/// A moment, with the date when it is not today and seconds when asked: "14:32", "16 Nov 14:32".
pub fn moment(utc: i64, now: i64, seconds: bool) -> String {
    let time = if seconds { hms(utc) } else { hm(utc) };
    if local_midnight(utc) == local_midnight(now) {
        time
    } else {
        format!("{} {time}", day(utc))
    }
}

/// "14:10 to 15:10", "16 Nov 23:00 to 17 Nov 01:00".
pub fn period(from: i64, to: i64, now: i64, seconds: bool) -> String {
    format!(
        "{} to {}",
        moment(from, now, seconds),
        moment(to, now, seconds)
    )
}

/// "2026-11-17 14:32" (local), the form the From and To fields use.
pub fn input(utc: i64) -> String {
    let f = fields(to_local(utc));
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        f.year, f.month, f.day, f.hour, f.minute
    )
}

/// Reads "2026-11-17 14:32" (local; also "2026-11-17 14:32:05" or "2026-11-17") as UTC.
pub fn parse_input(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, time) = match s.split_once(' ') {
        Some((d, t)) => (d.trim(), t.trim()),
        None => (s, "00:00"),
    };
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: u32 = d.next()?.parse().ok()?;
    let day: u32 = d.next()?.parse().ok()?;
    if d.next().is_some() {
        return None;
    }
    let mut t = time.split(':');
    let hour: u32 = t.next()?.parse().ok()?;
    let minute: u32 = t.next()?.parse().ok()?;
    let second: u32 = match t.next() {
        Some(x) => x.parse().ok()?,
        None => 0,
    };
    if t.next().is_some()
        || !(1970..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let days_in_month = civil(days_from_civil(year, month % 12 + 1, 1) - 1).2;
    let days_in_month = if month == 12 { 31 } else { days_in_month };
    if day == 0 || day > days_in_month {
        return None;
    }
    Some(to_utc(from_fields(Fields {
        year,
        month,
        day,
        hour,
        minute,
        second,
    })))
}

/// "45 s", "52 min", "6 h 20 min", "2 d 3 h".
pub fn duration(secs: f64) -> String {
    let s = secs.max(0.0).round() as i64;
    if s < 60 {
        format!("{s} s")
    } else if s < 3600 {
        format!("{} min", s / 60)
    } else if s < 86_400 {
        let (h, m) = (s / 3600, s / 60 % 60);
        if m == 0 {
            format!("{h} h")
        } else {
            format!("{h} h {m} min")
        }
    } else {
        let (d, h) = (s / 86_400, s / 3600 % 24);
        if h == 0 {
            format!("{d} d")
        } else {
            format!("{d} d {h} h")
        }
    }
}

/// Ticks for a time axis: local-time multiples of a step chosen so labels are at least
/// `min_gap_ms` apart. Returns UTC times inside `[from, to]` and whether labels need the date.
pub fn ticks(from: i64, to: i64, min_gap_ms: i64) -> (Vec<i64>, bool) {
    const STEPS: [i64; 13] = [
        MIN,
        2 * MIN,
        5 * MIN,
        10 * MIN,
        15 * MIN,
        30 * MIN,
        HOUR,
        2 * HOUR,
        3 * HOUR,
        6 * HOUR,
        12 * HOUR,
        DAY,
        2 * DAY,
    ];
    let step = STEPS
        .into_iter()
        .find(|s| *s >= min_gap_ms)
        .unwrap_or(2 * DAY);
    let (lf, lt) = (to_local(from), to_local(to));
    let mut out = Vec::new();
    let mut t = lf - lf.rem_euclid(step) + if lf.rem_euclid(step) == 0 { 0 } else { step };
    while t <= lt && out.len() < 200 {
        out.push(to_utc(t));
        t += step;
    }
    (out, step >= DAY)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_round_trip() {
        let t = days_from_civil(2026, 11, 17) * DAY + 14 * HOUR + 32 * MIN + 5 * SEC;
        let f = fields(t);
        assert_eq!(
            (f.year, f.month, f.day, f.hour, f.minute, f.second),
            (2026, 11, 17, 14, 32, 5)
        );
        assert_eq!(from_fields(f), t);
    }

    #[test]
    fn local_time_round_trips_and_inputs_parse() {
        // Any time zone: local and back is the same moment (outside a DST jump).
        let t = days_from_civil(2026, 11, 17) * DAY + 14 * HOUR + 32 * MIN;
        assert_eq!(to_utc(to_local(t)), t);
        assert_eq!(parse_input(&input(t)), Some(t));
        assert_eq!(parse_input(" 2026-11-17 "), Some(local_midnight(t)));
        for bad in [
            "",
            "2026-13-01 10:00",
            "2026-02-30 10:00",
            "2026-11-17 24:00",
            "17.11.2026",
            "2026-11-17 10:00:00:00",
        ] {
            assert_eq!(parse_input(bad), None, "{bad}");
        }
        assert!(parse_input("2024-02-29 10:00").is_some());
        assert!(parse_input("2026-12-31 23:59").is_some());
    }

    #[test]
    fn moments_show_the_date_only_when_not_today() {
        let now = days_from_civil(2026, 11, 17) * DAY + 12 * HOUR;
        let now = to_utc(now);
        assert_eq!(moment(now, now, false), "12:00");
        assert_eq!(moment(now - DAY, now, false), "16 Nov 12:00");
        assert_eq!(moment(now + 5 * SEC, now, true), "12:00:05");
    }

    #[test]
    fn durations_read_plainly() {
        assert_eq!(duration(45.0), "45 s");
        assert_eq!(duration(52.0 * 60.0), "52 min");
        assert_eq!(duration(6.0 * 3600.0 + 20.0 * 60.0), "6 h 20 min");
        assert_eq!(duration(2.0 * 86_400.0 + 3.0 * 3600.0), "2 d 3 h");
    }

    #[test]
    fn ticks_are_round_local_times_far_enough_apart() {
        let from = to_utc(days_from_civil(2026, 11, 17) * DAY + 14 * HOUR + 7 * MIN);
        let (t, dated) = ticks(from, from + HOUR, 9 * MIN);
        assert!(!dated);
        let labels: Vec<_> = t.iter().map(|x| hm(*x)).collect();
        assert_eq!(
            labels,
            vec!["14:10", "14:20", "14:30", "14:40", "14:50", "15:00"]
        );
        let (t, dated) = ticks(from, from + 7 * DAY, 20 * HOUR);
        assert!(dated);
        assert!(t.iter().all(|x| hm(*x) == "00:00"));
    }
}
