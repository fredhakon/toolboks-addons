//! The `memory-pressure` health check (docs/PLAN-health.md, section 4). Pure: time is passed
//! in as Unix seconds and the memory readings come from `pressure::Assessment`, so tests use
//! made-up sequences and never sleep.
//!
//! `Reading` carries exactly the fields of the host protocol's `Finding`, so mapping it to the
//! SDK type once the protocol is available is a one-line conversion. Until that hookup nothing
//! outside the tests calls this module.
#![allow(dead_code)]

use crate::pressure::{Assessment, Verdict};

/// The add-on's own name for this check.
pub const ID: &str = "memory-pressure";
/// How often the reading is recomputed.
pub const REFRESH_SECS: u64 = 60;
/// A reading is out of date after this long.
pub const STALE_AFTER_SECS: u32 = 300;
/// "Under pressure" stays reported for this long after the pressure has cleared.
pub const TAIL_SECS: u64 = 30 * 60;
/// Protocol limits (plan, section 1).
pub const MAX_TITLE: usize = 80;
pub const MAX_OBSERVATION: usize = 240;
pub const MAX_ACTION_LABEL: usize = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ok,
    Attention,
    NotChecked,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Watch,
    Problem,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    OpenPage { label: String, route: String },
}

/// Same fields as the protocol's `Finding`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reading {
    pub id: &'static str,
    pub title: String,
    pub observation: String,
    pub status: Status,
    /// Only meaningful when `status` is `Attention`.
    pub severity: Severity,
    pub measured_at: u64,
    pub stale_after_secs: u32,
    pub action: Option<Action>,
}

/// What the memory source gave at this refresh.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// No reading has been taken yet.
    NotRead,
    /// The source could not be read; the reason is in plain words.
    Unreadable(String),
    Read(Assessment),
}

/// The last time memory was under pressure, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PastPressure {
    pub at_secs: u64,
    pub reasons: Vec<String>,
}

pub struct Input<'a> {
    pub now_secs: u64,
    pub source: &'a Source,
    /// When the first successful reading was taken.
    pub watching_since: Option<u64>,
    pub last_pressure: Option<&'a PastPressure>,
}

/// True when the reading should be recomputed (never read, or a minute has passed; a clock
/// that went backwards also counts, so a stuck reading cannot outlive a clock change).
pub fn should_refresh(now_secs: u64, last_secs: Option<u64>) -> bool {
    match last_secs {
        None => true,
        Some(last) => now_secs < last || now_secs - last >= REFRESH_SECS,
    }
}

/// Computes the finding for one refresh.
pub fn check(input: &Input) -> Reading {
    let now = input.now_secs;
    let reading = |status, title: &str, observation: String, action| Reading {
        id: ID,
        title: clip(title, MAX_TITLE),
        observation: clip(&observation, MAX_OBSERVATION),
        status,
        severity: Severity::Watch,
        measured_at: now,
        stale_after_secs: STALE_AFTER_SECS,
        action,
    };
    let open_page = || {
        Some(Action::OpenPage {
            label: clip("Open Memory Cleaner", MAX_ACTION_LABEL),
            route: "Home".to_string(),
        })
    };

    let assessment = match input.source {
        Source::NotRead => {
            return reading(
                Status::NotChecked,
                "Memory pressure not checked yet",
                "Memory Cleaner has not taken its first memory reading yet.".to_string(),
                None,
            );
        }
        Source::Unreadable(why) => {
            return reading(
                Status::Unavailable,
                "Memory pressure cannot be checked",
                format!("Memory Cleaner cannot read memory use on this PC: {why}"),
                None,
            );
        }
        Source::Read(a) => a,
    };

    if assessment.verdict == Verdict::UnderPressure {
        return reading(
            Status::Attention,
            "Memory is under pressure",
            format!(
                "Memory is under pressure now. {}",
                reasons_text(&assessment.reasons)
            ),
            open_page(),
        );
    }

    if let Some(past) = input.last_pressure {
        let ago = now.saturating_sub(past.at_secs);
        if ago < TAIL_SECS {
            return reading(
                Status::Attention,
                "Memory was under pressure a short while ago",
                format!(
                    "Memory was under pressure {}. {}",
                    ago_text(ago),
                    reasons_text(&past.reasons)
                ),
                open_page(),
            );
        }
    }

    let watched = input.watching_since.map_or(0, |s| now.saturating_sub(s));
    let span = if watched >= TAIL_SECS {
        "in the last 30 minutes".to_string()
    } else if watched >= 60 {
        let m = watched / 60;
        format!("in the last {m} minute{}", if m == 1 { "" } else { "s" })
    } else {
        "since Memory Cleaner started".to_string()
    };
    let mut text = format!("Memory has not been under pressure {span}.");
    if assessment.verdict == Verdict::Busy {
        text.push_str(&format!(
            " Memory is busy right now, but not under pressure. {}",
            reasons_text(&assessment.reasons)
        ));
    }
    reading(Status::Ok, "Memory is not under pressure", text, None)
}

/// Keeps what a refresh needs to remember between calls.
#[derive(Debug, Default)]
pub struct Tracker {
    watching_since: Option<u64>,
    last_pressure: Option<PastPressure>,
    last_refresh: Option<u64>,
    last: Option<Reading>,
}

impl Tracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Recomputes the reading if one is due; otherwise returns the previous one. `None` only
    /// before the first call.
    pub fn refresh(&mut self, now_secs: u64, source: Source) -> Option<&Reading> {
        if self.last.is_none() || should_refresh(now_secs, self.last_refresh) {
            if let Source::Read(a) = &source {
                self.watching_since.get_or_insert(now_secs);
                if a.verdict == Verdict::UnderPressure {
                    self.last_pressure = Some(PastPressure {
                        at_secs: now_secs,
                        reasons: a.reasons.clone(),
                    });
                }
            }
            let reading = check(&Input {
                now_secs,
                source: &source,
                watching_since: self.watching_since,
                last_pressure: self.last_pressure.as_ref(),
            });
            self.last_refresh = Some(now_secs);
            self.last = Some(reading);
        }
        self.last.as_ref()
    }
}

fn ago_text(secs: u64) -> String {
    let m = secs / 60;
    match m {
        0 => "less than a minute ago".to_string(),
        1 => "1 minute ago".to_string(),
        _ => format!("{m} minutes ago"),
    }
}

fn reasons_text(reasons: &[String]) -> String {
    if reasons.is_empty() {
        return String::new();
    }
    let joined: Vec<String> = reasons.iter().map(|r| clean(r)).collect();
    format!("{}.", joined.join("; "))
}

/// Removes control characters and markup characters, trims, and collapses whitespace.
fn clean(text: &str) -> String {
    let kept: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .filter(|c| !matches!(c, '<' | '>' | '`' | '*' | '[' | ']' | '#' | '&'))
        .collect();
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Cleans `text` and cuts it to `max` characters, ending in "..." when it was cut.
fn clip(text: &str, max: usize) -> String {
    let text = clean(text);
    if text.chars().count() <= max {
        return text;
    }
    let mut out: String = text.chars().take(max - 3).collect();
    out = out.trim_end().to_string();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assessment(verdict: Verdict, reasons: &[&str]) -> Assessment {
        Assessment {
            verdict,
            reasons: reasons.iter().map(|r| r.to_string()).collect(),
        }
    }

    fn healthy() -> Source {
        Source::Read(assessment(Verdict::Healthy, &[]))
    }

    fn pressure() -> Source {
        Source::Read(assessment(
            Verdict::UnderPressure,
            &[
                "Only 1.1 GB is ready to use",
                "Windows is reading memory back from disk a lot",
            ],
        ))
    }

    fn check_at(
        now: u64,
        source: &Source,
        since: Option<u64>,
        past: Option<&PastPressure>,
    ) -> Reading {
        check(&Input {
            now_secs: now,
            source,
            watching_since: since,
            last_pressure: past,
        })
    }

    fn assert_limits(r: &Reading) {
        assert_eq!(r.id, "memory-pressure");
        assert!(!r.title.is_empty() && r.title.chars().count() <= MAX_TITLE);
        assert!(r.observation.chars().count() <= MAX_OBSERVATION);
        assert!(!r.observation.is_empty());
        assert_eq!(r.stale_after_secs, 300);
        for text in [&r.title, &r.observation] {
            assert!(!text.chars().any(|c| c.is_control()));
            assert!(!text.chars().any(|c| "<>`*[]#&".contains(c)), "{text}");
        }
        if let Some(Action::OpenPage { label, route }) = &r.action {
            assert!(label.chars().count() <= MAX_ACTION_LABEL);
            assert_eq!(route, "Home");
        }
    }

    #[test]
    fn not_checked_before_the_first_reading() {
        let r = check_at(1000, &Source::NotRead, None, None);
        assert_eq!(r.status, Status::NotChecked);
        assert_eq!(r.action, None);
        assert_limits(&r);
    }

    #[test]
    fn unreadable_source_is_unavailable_with_the_reason() {
        let src = Source::Unreadable("Windows refused the memory counters".to_string());
        let r = check_at(1000, &src, None, None);
        assert_eq!(r.status, Status::Unavailable);
        assert!(r
            .observation
            .contains("Windows refused the memory counters"));
        assert_limits(&r);
    }

    #[test]
    fn unreadable_wins_over_an_old_pressure_tail() {
        let past = PastPressure {
            at_secs: 900,
            reasons: vec!["x".into()],
        };
        let src = Source::Unreadable("no access".to_string());
        let r = check_at(1000, &src, Some(0), Some(&past));
        assert_eq!(r.status, Status::Unavailable);
    }

    #[test]
    fn ok_says_what_was_measured_and_never_claims_a_healthy_pc() {
        let r = check_at(5000, &healthy(), Some(0), None);
        assert_eq!(r.status, Status::Ok);
        assert_eq!(
            r.observation,
            "Memory has not been under pressure in the last 30 minutes."
        );
        assert!(!r.observation.to_lowercase().contains("healthy"));
        assert_eq!(r.action, None);
        assert_eq!(r.measured_at, 5000);
        assert_limits(&r);
    }

    #[test]
    fn ok_does_not_claim_more_time_than_was_watched() {
        let start = check_at(1000, &healthy(), Some(1000), None);
        assert!(start.observation.contains("since Memory Cleaner started"));
        let early = check_at(1000 + 5 * 60, &healthy(), Some(1000), None);
        assert!(early.observation.contains("in the last 5 minutes"));
        let one = check_at(1000 + 60, &healthy(), Some(1000), None);
        assert!(one.observation.contains("in the last 1 minute."));
    }

    #[test]
    fn busy_is_ok_and_says_so_in_the_observation() {
        let src = Source::Read(assessment(
            Verdict::Busy,
            &["Programs have reserved 88% of the memory Windows can promise"],
        ));
        let r = check_at(5000, &src, Some(0), None);
        assert_eq!(r.status, Status::Ok);
        assert!(r.observation.contains("busy"));
        assert!(r.observation.contains("reserved 88%"));
        assert_limits(&r);
    }

    #[test]
    fn under_pressure_needs_attention_with_the_reasons() {
        let r = check_at(5000, &pressure(), Some(0), None);
        assert_eq!(r.status, Status::Attention);
        assert_eq!(r.severity, Severity::Watch);
        assert!(r.observation.contains("Only 1.1 GB is ready to use"));
        assert!(r.observation.contains("reading memory back from disk"));
        assert_eq!(
            r.action,
            Some(Action::OpenPage {
                label: "Open Memory Cleaner".into(),
                route: "Home".into()
            })
        );
        assert_limits(&r);
    }

    #[test]
    fn tail_stays_attention_for_30_minutes_then_goes_ok() {
        let past = PastPressure {
            at_secs: 10_000,
            reasons: vec!["Only 1.1 GB is ready to use".to_string()],
        };
        let at = |now| check_at(now, &healthy(), Some(0), Some(&past));

        let just_after = at(10_030);
        assert_eq!(just_after.status, Status::Attention);
        assert!(just_after.observation.contains("less than a minute ago"));
        assert!(just_after.observation.contains("Only 1.1 GB"));

        let mid = at(10_000 + 10 * 60);
        assert_eq!(mid.status, Status::Attention);
        assert!(mid.observation.contains("10 minutes ago"));

        let last_second = at(10_000 + TAIL_SECS - 1);
        assert_eq!(last_second.status, Status::Attention);

        let after = at(10_000 + TAIL_SECS);
        assert_eq!(after.status, Status::Ok);
        assert_limits(&mid);
        assert_limits(&after);
    }

    #[test]
    fn text_is_cut_to_the_limits_and_markup_is_removed() {
        let long = "x".repeat(400);
        let src = Source::Read(assessment(
            Verdict::UnderPressure,
            &[&long, "<b>bold</b> [link](http://x) `code` *em* #tag &amp;"],
        ));
        let r = check_at(5000, &src, Some(0), None);
        assert_eq!(r.observation.chars().count(), MAX_OBSERVATION);
        assert!(r.observation.ends_with("..."));
        assert_limits(&r);

        let src = Source::Read(assessment(
            Verdict::UnderPressure,
            &["<b>bold</b> [link](http://x) `code` *em* #tag &amp;\nnext\u{7}line"],
        ));
        let r = check_at(5000, &src, Some(0), None);
        assert!(r.observation.contains("bbold/b link(http://x)"));
        assert_limits(&r);

        let src = Source::Unreadable("é".repeat(500));
        let r = check_at(5000, &src, None, None);
        assert_eq!(r.observation.chars().count(), MAX_OBSERVATION);
        assert_limits(&r);
    }

    #[test]
    fn should_refresh_once_a_minute() {
        assert!(should_refresh(100, None));
        assert!(!should_refresh(100, Some(100)));
        assert!(!should_refresh(159, Some(100)));
        assert!(should_refresh(160, Some(100)));
        assert!(should_refresh(50, Some(100)), "clock went back");
    }

    #[test]
    fn tracker_runs_a_fake_sequence() {
        let mut t = Tracker::new();
        let r = t.refresh(0, Source::NotRead).unwrap();
        assert_eq!(r.status, Status::NotChecked);

        // Not due yet: the old reading is kept, the new source is not looked at.
        let r = t.refresh(30, healthy()).unwrap();
        assert_eq!(r.status, Status::NotChecked);

        let r = t.refresh(60, healthy()).unwrap();
        assert_eq!(r.status, Status::Ok);
        assert!(r.observation.contains("since Memory Cleaner started"));

        let r = t.refresh(120, pressure()).unwrap();
        assert_eq!(r.status, Status::Attention);

        // Pressure cleared: still Attention for the tail, with the old reasons.
        let r = t.refresh(180, healthy()).unwrap();
        assert_eq!(r.status, Status::Attention);
        assert!(r.observation.contains("Only 1.1 GB"));

        let r = t.refresh(120 + TAIL_SECS - 60, healthy()).unwrap();
        assert_eq!(r.status, Status::Attention);

        let r = t.refresh(120 + TAIL_SECS, healthy()).unwrap();
        assert_eq!(r.status, Status::Ok);
        assert!(r.observation.contains("in the last 30 minutes"));

        let r = t
            .refresh(120 + TAIL_SECS + 60, Source::Unreadable("denied".into()))
            .unwrap();
        assert_eq!(r.status, Status::Unavailable);
    }
}
