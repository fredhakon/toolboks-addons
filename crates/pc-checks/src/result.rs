//! What a check returns. Provisional: the design for how findings reach the Home page
//! (docs/PLAN-health.md in the host repo) was not merged when this was written, so these are
//! small, plain types that can be mapped onto whatever that design settles on.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::date::format_iso8601_utc;

/// Longest observation or action, in characters. Long enough for a BIOS how-to, short enough
/// for one card on the Home page.
pub const MAX_FINDING_CHARS: usize = 320;
/// Longest "not checked" or "unavailable" reason, in characters.
pub const MAX_REASON_CHARS: usize = 200;

/// Characters that could be read as markup by a renderer. They are dropped from every text.
const MARKUP_CHARS: &[char] = &['<', '>', '`', '*', '[', ']', '~', '|'];

/// Make `text` safe to show: one line, no markup characters, at most `max_chars` characters
/// (the last one is an ellipsis when it was cut).
pub fn plain(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut pending_space = false;
    for c in text.chars() {
        if c.is_whitespace() || c.is_control() {
            pending_space = !out.is_empty();
            continue;
        }
        if MARKUP_CHARS.contains(&c) {
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(c);
    }
    if out.chars().count() > max_chars {
        let keep = max_chars.saturating_sub(1);
        out = out
            .chars()
            .take(keep)
            .collect::<String>()
            .trim_end()
            .to_string();
        out.push('…');
    }
    out
}

/// The six checks. The id is stable; the title is for people.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckId {
    RamSpeed,
    PowerPlan,
    MemoryIntegrity,
    CpuThrottling,
    PendingRestart,
    GpuDriverAge,
}

impl CheckId {
    pub const ALL: [CheckId; 6] = [
        CheckId::RamSpeed,
        CheckId::PowerPlan,
        CheckId::MemoryIntegrity,
        CheckId::CpuThrottling,
        CheckId::PendingRestart,
        CheckId::GpuDriverAge,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            CheckId::RamSpeed => "ram-speed",
            CheckId::PowerPlan => "power-plan",
            CheckId::MemoryIntegrity => "memory-integrity",
            CheckId::CpuThrottling => "cpu-throttling",
            CheckId::PendingRestart => "pending-restart",
            CheckId::GpuDriverAge => "gpu-driver-age",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            CheckId::RamSpeed => "Memory speed",
            CheckId::PowerPlan => "Power plan and mode",
            CheckId::MemoryIntegrity => "Memory integrity",
            CheckId::CpuThrottling => "CPU throttling",
            CheckId::PendingRestart => "Pending restart",
            CheckId::GpuDriverAge => "Graphics driver age",
        }
    }
}

/// How much a finding matters. There is no score: a finding is one observation with one action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Worth knowing, nothing to fix.
    Info,
    Low,
    Medium,
    High,
}

/// One plain observation and the one thing to do about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub severity: Severity,
    pub observation: String,
    pub action: String,
}

impl Finding {
    /// Both texts are cleaned and bounded here, so no check can hand out markup or an essay.
    pub fn new(severity: Severity, observation: &str, action: &str) -> Self {
        Finding {
            severity,
            observation: plain(observation, MAX_FINDING_CHARS),
            action: plain(action, MAX_FINDING_CHARS),
        }
    }
}

/// The only four things a check can say. A missing reading is never `Ok`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Read, and nothing to report.
    Ok,
    Finding(Finding),
    /// This PC has the thing but it could not be read (access denied, counter missing, ...).
    NotChecked {
        reason: String,
    },
    /// This check does not apply here (no such feature, no such hardware).
    Unavailable {
        reason: String,
    },
}

impl Outcome {
    pub fn not_checked(reason: &str) -> Self {
        Outcome::NotChecked {
            reason: plain(reason, MAX_REASON_CHARS),
        }
    }

    pub fn unavailable(reason: &str) -> Self {
        Outcome::Unavailable {
            reason: plain(reason, MAX_REASON_CHARS),
        }
    }
}

/// One check's outcome and when it was measured.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckResult {
    pub check: CheckId,
    /// Seconds since 1970-01-01 UTC, taken when the reading finished.
    pub measured_at: u64,
    pub outcome: Outcome,
}

impl CheckResult {
    /// `measured_at` as `2026-11-16T01:02:47Z`.
    pub fn measured_at_iso(&self) -> String {
        format_iso8601_utc(self.measured_at)
    }
}

/// Why a data source could not give a reading.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadError {
    /// Windows refused to read it without administrator rights.
    AccessDenied,
    /// The key, value, counter or table that should hold the reading is not there.
    NotPresent(String),
    /// This Windows or this hardware does not have the feature at all.
    Unsupported(String),
    /// Anything else.
    Failed(String),
}

pub type Reading<T> = Result<T, ReadError>;

/// Turn a read error into the outcome it deserves. `what` names the thing that was read.
pub fn outcome_for_error(err: &ReadError, what: &str) -> Outcome {
    match err {
        ReadError::AccessDenied => Outcome::not_checked(&format!(
            "Windows did not allow reading {what} without administrator rights."
        )),
        ReadError::NotPresent(detail) => {
            Outcome::not_checked(&format!("Could not find {what}: {detail}."))
        }
        ReadError::Unsupported(detail) => Outcome::unavailable(detail),
        ReadError::Failed(detail) => {
            Outcome::not_checked(&format!("Reading {what} failed: {detail}."))
        }
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_drops_markup_and_newlines() {
        assert_eq!(
            plain("a <b>bold</b>\n`code` [x](y)  *z*", 100),
            "a bbold/b code x(y) z"
        );
    }

    #[test]
    fn plain_is_bounded_with_an_ellipsis() {
        let long = "word ".repeat(200);
        let out = plain(&long, 50);
        assert_eq!(out.chars().count(), 50);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn plain_keeps_short_text_as_is() {
        assert_eq!(plain("  hello   world ", 50), "hello world");
    }

    #[test]
    fn finding_texts_are_cleaned_and_bounded() {
        let f = Finding::new(Severity::Low, &"x<y> ".repeat(500), "do **this**");
        assert!(f.observation.chars().count() <= MAX_FINDING_CHARS);
        assert!(!f.observation.contains('<'));
        assert_eq!(f.action, "do this");
    }

    #[test]
    fn errors_map_to_not_checked_or_unavailable_never_ok() {
        for err in [
            ReadError::AccessDenied,
            ReadError::NotPresent("x".into()),
            ReadError::Failed("y".into()),
        ] {
            assert!(matches!(
                outcome_for_error(&err, "thing"),
                Outcome::NotChecked { .. }
            ));
        }
        assert!(matches!(
            outcome_for_error(&ReadError::Unsupported("no".into()), "thing"),
            Outcome::Unavailable { .. }
        ));
    }

    #[test]
    fn check_ids_are_unique() {
        let mut ids: Vec<_> = CheckId::ALL.iter().map(|c| c.as_str()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), CheckId::ALL.len());
    }
}
