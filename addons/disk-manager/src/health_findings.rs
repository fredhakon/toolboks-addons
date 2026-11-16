//! The drive-health check as a plain reading, ready to be reported to Home as a finding.
//!
//! Everything here is pure: a drive letter and what [`crate::health`] read for it go in, a
//! [`Reading`] comes out. `Reading` has exactly the fields of the host's `Finding`
//! (`docs/PLAN-health.md` section 1), so the protocol hookup is a one-line conversion once the
//! SDK carries findings. Text is plain words: no markup, no control characters, within the
//! host's limits (the host cuts anything longer).
//!
//! Cadence: read every fixed drive at start and again every [`REFRESH_EVERY_SECS`]; a reading
//! is out of date after [`STALE_AFTER_SECS`].
// Nothing calls this until the host protocol carries findings; the tests exercise it.
#![allow(dead_code)]

use crate::health::{ReadError, Report, Verdict};

/// The id prefix; the drive letter in lower case follows ("drive-health-c").
const ID_PREFIX: &str = "drive-health";
/// Read every fixed drive again after this long (6 hours).
pub const REFRESH_EVERY_SECS: u64 = 6 * 60 * 60;
/// A reading is out of date after this long (24 hours).
pub const STALE_AFTER_SECS: u32 = 24 * 60 * 60;

pub const ID_MAX: usize = 48;
pub const TITLE_MAX: usize = 80;
pub const OBSERVATION_MAX: usize = 240;
pub const LABEL_MAX: usize = 40;

const ACTION_LABEL: &str = "Open drive health";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ok,
    Attention,
    NotChecked,
    Unavailable,
}

/// Only used for ranking when the status is `Attention`; `Watch` otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Watch,
    Problem,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    OpenPage { label: String, route: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reading {
    pub id: String,
    pub title: String,
    pub observation: String,
    pub status: Status,
    pub severity: Severity,
    /// Unix seconds.
    pub measured_at: u64,
    pub stale_after_secs: u32,
    pub action: Option<Action>,
}

/// The drive letter in the form the Optimization page uses ("C:"), or `None` when `raw` is not
/// a single letter A to Z with an optional colon ("c", "C:").
pub fn normalize_letter(raw: &str) -> Option<String> {
    let mut chars = raw.trim().chars();
    let letter = chars.next().filter(char::is_ascii_alphabetic)?;
    match chars.as_str() {
        "" | ":" => Some(format!("{}:", letter.to_ascii_uppercase())),
        _ => None,
    }
}

/// The drives to read: valid letters only, upper case, each once, in letter order.
pub fn drives_to_check<S: AsRef<str>>(fixed: &[S]) -> Vec<String> {
    let mut out: Vec<String> = fixed
        .iter()
        .filter_map(|d| normalize_letter(d.as_ref()))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// True when the drives should be read now: at start (`last` is `None`), when the last read
/// is at least [`REFRESH_EVERY_SECS`] old, or when the clock went backwards.
pub fn should_refresh(now: u64, last: Option<u64>) -> bool {
    match last {
        None => true,
        Some(last) => now < last || now - last >= REFRESH_EVERY_SECS,
    }
}

/// Removes control characters and the characters that open markup (`<`, `>`, backtick), folds
/// runs of white space into one space, trims and cuts to `max` characters.
fn clean(text: &str, max: usize) -> String {
    let folded: String = text
        .chars()
        .filter(|c| !matches!(c, '<' | '>' | '`'))
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    folded
        .chars()
        .take(max)
        .collect::<String>()
        .trim_end()
        .to_string()
}

fn id_for(letter: &str) -> String {
    match normalize_letter(letter) {
        Some(l) => format!("{ID_PREFIX}-{}", &l[..1].to_ascii_lowercase()),
        None => ID_PREFIX.to_string(),
    }
}

/// The reading for one drive from what was read for it. `letter` is "C:" or "C"; `now` is Unix
/// seconds.
pub fn check(letter: &str, read: &Result<Report, ReadError>, now: u64) -> Reading {
    let shown = normalize_letter(letter).unwrap_or_else(|| "This drive".to_string());
    let (status, severity, observation) = match read {
        Ok(report) => {
            let (verdict, reason) = crate::health::verdict(&report.health);
            match verdict {
                Verdict::Healthy => (Status::Ok, Severity::Watch, format!("Healthy. {reason}")),
                Verdict::Watch => (
                    Status::Attention,
                    Severity::Watch,
                    format!("Watch. {reason}"),
                ),
                Verdict::ReplaceSoon => (
                    Status::Attention,
                    Severity::Problem,
                    format!("Replace soon. {reason}"),
                ),
            }
        }
        Err(ReadError::AccessDenied) => (
            Status::Unavailable,
            Severity::Watch,
            "Needs administrator rights. This drive only gives its health data to administrators."
                .to_string(),
        ),
        Err(ReadError::Unsupported) => (
            Status::Unavailable,
            Severity::Watch,
            "This drive does not report health. USB, memory card, RAID and virtual drives often do not."
                .to_string(),
        ),
        Err(ReadError::NotFound) => (
            Status::Unavailable,
            Severity::Watch,
            "This drive letter is not on a single physical drive Disk Manager can read."
                .to_string(),
        ),
        Err(ReadError::Other(e)) => (
            Status::Unavailable,
            Severity::Watch,
            format!("Drive health could not be read: {}", clean(e, 120)),
        ),
    };
    reading(letter, &shown, status, severity, &observation, now)
}

/// The reading while the first read of a drive is still running.
pub fn not_checked(letter: &str, now: u64) -> Reading {
    let shown = normalize_letter(letter).unwrap_or_else(|| "This drive".to_string());
    reading(
        letter,
        &shown,
        Status::NotChecked,
        Severity::Watch,
        "Drive health has not been read yet.",
        now,
    )
}

fn reading(
    letter: &str,
    shown: &str,
    status: Status,
    severity: Severity,
    observation: &str,
    now: u64,
) -> Reading {
    let action = normalize_letter(letter).map(|l| Action::OpenPage {
        label: clean(ACTION_LABEL, LABEL_MAX),
        route: format!("Optimization/{l}"),
    });
    Reading {
        id: id_for(letter),
        title: clean(&format!("Drive {shown} health"), TITLE_MAX),
        observation: clean(observation, OBSERVATION_MAX),
        status,
        severity,
        measured_at: now,
        stale_after_secs: STALE_AFTER_SECS,
        action,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::{parse_ata, parse_nvme, PAGE};

    const NOW: u64 = 1_800_000_000;

    // The same recorded pages as the health.rs tests.
    fn nvme(warning: u8, kelvin: u16, spare: u8, used: u8, media: u64) -> Vec<u8> {
        let mut p = vec![0u8; PAGE];
        p[0] = warning;
        p[1..3].copy_from_slice(&kelvin.to_le_bytes());
        p[3] = spare;
        p[4] = 10;
        p[5] = used;
        p[128..136].copy_from_slice(&8_760u64.to_le_bytes());
        p[160..168].copy_from_slice(&media.to_le_bytes());
        p
    }

    fn ata(attrs: &[(u8, u8, u64)]) -> Vec<u8> {
        let mut p = vec![0u8; PAGE];
        p[0] = 0x10;
        for (i, (id, value, raw)) in attrs.iter().enumerate() {
            let e = &mut p[2 + i * 12..14 + i * 12];
            e[0] = *id;
            e[1] = 0x32;
            e[3] = *value;
            e[4] = *value;
            e[5..11].copy_from_slice(&raw.to_le_bytes()[..6]);
        }
        let sum = p[..511].iter().fold(0u8, |s, b| s.wrapping_add(*b));
        p[511] = sum.wrapping_neg();
        p
    }

    fn report(page: Result<crate::health::DriveHealth, crate::health::ParseError>) -> Report {
        Report {
            model: "Test drive".into(),
            health: page.unwrap(),
        }
    }

    fn open_route(r: &Reading) -> &str {
        match r.action.as_ref().expect("an action") {
            Action::OpenPage { route, .. } => route,
        }
    }

    fn assert_limits(r: &Reading) {
        assert!((1..=ID_MAX).contains(&r.id.len()), "{}", r.id);
        assert!(r
            .id
            .chars()
            .all(|c| matches!(c, 'a'..='z' | '0'..='9' | '.' | '-')));
        assert!(!r.title.is_empty() && r.title.chars().count() <= TITLE_MAX);
        assert!(!r.observation.is_empty() && r.observation.chars().count() <= OBSERVATION_MAX);
        for text in [&r.title, &r.observation] {
            assert!(!text.chars().any(char::is_control), "{text:?}");
            assert!(!text.contains(['<', '>', '`', '*', '[', ']']), "{text}");
            assert!(!text.contains("://"), "{text}");
        }
        if let Some(Action::OpenPage { label, route }) = &r.action {
            assert!(!label.is_empty() && label.chars().count() <= LABEL_MAX);
            assert!(route.starts_with("Optimization/"));
        }
        assert_eq!(r.stale_after_secs, 86_400);
        assert_eq!(r.measured_at, NOW);
    }

    #[test]
    fn healthy_nvme_is_ok() {
        let r = check("C:", &Ok(report(parse_nvme(&nvme(0, 311, 100, 3, 0)))), NOW);
        assert_eq!(r.id, "drive-health-c");
        assert_eq!(r.title, "Drive C: health");
        assert_eq!(r.status, Status::Ok);
        assert!(r.observation.starts_with("Healthy."), "{}", r.observation);
        assert!(r.observation.contains("No warning signs"));
        assert_eq!(open_route(&r), "Optimization/C:");
        assert_limits(&r);
    }

    #[test]
    fn healthy_sata_is_ok() {
        let page = ata(&[(5, 100, 0), (9, 99, 12_345), (194, 67, 33), (197, 100, 0)]);
        let r = check("d", &Ok(report(parse_ata(&page))), NOW);
        assert_eq!((r.id.as_str(), r.status), ("drive-health-d", Status::Ok));
        assert_eq!(open_route(&r), "Optimization/D:");
        assert_limits(&r);
    }

    #[test]
    fn watch_is_attention_watch() {
        let r = check(
            "C:",
            &Ok(report(parse_nvme(&nvme(0, 318, 100, 74, 6)))),
            NOW,
        );
        assert_eq!((r.status, r.severity), (Status::Attention, Severity::Watch));
        assert!(r.observation.starts_with("Watch."), "{}", r.observation);
        assert_limits(&r);
        // SATA with a few pending sectors.
        let page = ata(&[(5, 100, 2), (197, 100, 8), (198, 100, 0)]);
        let r = check("E:", &Ok(report(parse_ata(&page))), NOW);
        assert_eq!((r.status, r.severity), (Status::Attention, Severity::Watch));
        assert!(r.observation.contains("reallocated"), "{}", r.observation);
        assert_limits(&r);
    }

    #[test]
    fn replace_soon_is_attention_problem() {
        let r = check(
            "C:",
            &Ok(report(parse_nvme(&nvme(0, 318, 100, 93, 6)))),
            NOW,
        );
        assert_eq!(
            (r.status, r.severity),
            (Status::Attention, Severity::Problem)
        );
        assert!(
            r.observation.starts_with("Replace soon. "),
            "{}",
            r.observation
        );
        assert!(r.observation.contains("93%"));
        assert_limits(&r);
        let page = ata(&[(5, 80, 140), (197, 100, 12)]);
        let r = check("D:", &Ok(report(parse_ata(&page))), NOW);
        assert_eq!(
            (r.status, r.severity),
            (Status::Attention, Severity::Problem)
        );
        assert_limits(&r);
    }

    #[test]
    fn the_worst_verdict_wins_over_the_text_length() {
        // Several findings at once still fit the observation limit.
        let page = ata(&[(5, 80, 140), (197, 100, 12), (231, 8, 0), (194, 20, 75)]);
        let r = check("D:", &Ok(report(parse_ata(&page))), NOW);
        assert_eq!(r.severity, Severity::Problem);
        assert_limits(&r);
    }

    #[test]
    fn administrator_and_unsupported_are_unavailable_with_the_plain_reason() {
        let r = check("C:", &Err(ReadError::AccessDenied), NOW);
        assert_eq!(r.status, Status::Unavailable);
        assert!(r.observation.starts_with("Needs administrator rights"));
        assert_limits(&r);
        let r = check("C:", &Err(ReadError::Unsupported), NOW);
        assert_eq!(r.status, Status::Unavailable);
        assert!(r
            .observation
            .starts_with("This drive does not report health"));
        assert_limits(&r);
        let r = check("C:", &Err(ReadError::NotFound), NOW);
        assert_eq!(r.status, Status::Unavailable);
        assert_limits(&r);
    }

    #[test]
    fn other_errors_are_cleaned_and_cut() {
        let nasty = format!("<b>bad</b>\u{7}\n\tline {}", "x".repeat(600));
        let r = check("C:", &Err(ReadError::Other(nasty)), NOW);
        assert_eq!(r.status, Status::Unavailable);
        assert!(r
            .observation
            .starts_with("Drive health could not be read: bbad/b line"));
        assert_limits(&r);
    }

    #[test]
    fn not_checked_before_the_first_read() {
        let r = not_checked("c", NOW);
        assert_eq!(r.status, Status::NotChecked);
        assert_eq!(r.id, "drive-health-c");
        assert_limits(&r);
    }

    #[test]
    fn ids_follow_the_letter_and_bad_letters_get_no_action() {
        for (raw, id) in [
            ("C:", "drive-health-c"),
            ("z", "drive-health-z"),
            (" Q: ", "drive-health-q"),
        ] {
            assert_eq!(check(raw, &Err(ReadError::Unsupported), NOW).id, id);
        }
        let r = check("C:\\Windows", &Err(ReadError::Unsupported), NOW);
        assert_eq!(r.id, "drive-health");
        assert_eq!(r.action, None);
        assert_limits(&Reading { action: None, ..r });
    }

    #[test]
    fn drives_are_normalized_sorted_and_unique() {
        assert_eq!(
            drives_to_check(&["d:", "C:", "c", "", "1:", "AB:", "C:\\x", "E:"]),
            vec!["C:", "D:", "E:"]
        );
        assert!(drives_to_check::<&str>(&[]).is_empty());
        assert_eq!(drives_to_check(&["C:".to_string()]), vec!["C:"]);
    }

    #[test]
    fn cadence_is_start_then_every_six_hours() {
        assert_eq!(REFRESH_EVERY_SECS, 21_600);
        assert!(should_refresh(NOW, None), "read at start");
        assert!(!should_refresh(NOW + 1, Some(NOW)));
        assert!(!should_refresh(NOW + REFRESH_EVERY_SECS - 1, Some(NOW)));
        assert!(should_refresh(NOW + REFRESH_EVERY_SECS, Some(NOW)));
        assert!(should_refresh(NOW - 5, Some(NOW)), "clock went backwards");
        // A reading read every 6 h is never stale before the next one.
        assert!(u64::from(STALE_AFTER_SECS) > REFRESH_EVERY_SECS);
    }
}
