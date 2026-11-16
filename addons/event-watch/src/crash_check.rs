//! The `recent-crashes` health check: a pure function from the events (or the reason they could
//! not be read) to one plain reading, plus the cadence of when to check again. No Windows calls,
//! no clock reads, no I/O.
//!
//! `Reading` has exactly the fields of the host's `Finding` (docs/PLAN-health.md section 1), so
//! it becomes one with [`Reading::finding`]. `crash_watch` runs it in the background.

use super::analysis::{detect_crashes, ms_to_text, DEFAULT_WINDOW_MS};
use super::bugchecks;
use super::model::{CrashKind, Event};
use toolboks_addon_sdk::{Finding, Severity as FindingSeverity};

/// The finding's id. Stable across runs.
pub const ID: &str = "recent-crashes";
/// How far back the check looks.
pub const WINDOW_DAYS: i64 = 7;
const WINDOW_MS: i64 = WINDOW_DAYS * 24 * 60 * 60 * 1000;
/// Unexpected restarts in the window that are worth a look.
pub const RESTARTS_TO_WATCH: usize = 2;
/// One check at start and then this often (30 minutes).
pub const REFRESH_SECS: u64 = 30 * 60;
/// The reading is out of date after this long (30 minutes).
pub const STALE_AFTER_SECS: u32 = 30 * 60;
/// The page the action opens. Must be one of the add-on's pages.
pub const ROUTE: &str = "Crashes";

pub const TITLE_MAX: usize = 80;
pub const OBSERVATION_MAX: usize = 240;
pub const LABEL_MAX: usize = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ok,
    Attention,
    NotChecked,
    Unavailable,
}

/// Used for ranking only when the status is `Attention`; `Watch` otherwise (ignored there).
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
    pub id: &'static str,
    pub title: String,
    pub observation: String,
    pub status: Status,
    pub severity: Severity,
    /// Unix seconds.
    pub measured_at: u64,
    pub stale_after_secs: u32,
    pub action: Option<Action>,
}

impl Reading {
    /// The SDK finding for this reading, with its own time and staleness kept.
    pub fn finding(&self) -> Finding {
        let (title, text) = (self.title.as_str(), self.observation.as_str());
        let finding = match (self.status, self.severity) {
            (Status::Ok, _) => Finding::ok(self.id, title, text),
            (Status::Attention, severity) => Finding::attention(
                self.id,
                title,
                text,
                match severity {
                    Severity::Watch => FindingSeverity::Watch,
                    Severity::Problem => FindingSeverity::Problem,
                },
            ),
            (Status::NotChecked, _) => Finding::not_checked(self.id, title, text),
            (Status::Unavailable, _) => Finding::unavailable(self.id, title, text),
        }
        .measured_at(self.measured_at)
        .stale_after(self.stale_after_secs);
        match &self.action {
            Some(Action::OpenPage { label, route }) => finding.open_page(label, route),
            None => finding,
        }
    }
}

/// What the add-on has of the event log right now.
#[derive(Clone, Copy, Debug)]
pub enum Input<'a> {
    /// The first read has not finished (or has not started).
    NotRead,
    /// The log could not be read; the plain reason.
    Unreadable(&'a str),
    /// Events from the System log (any order, any span; older ones are ignored here).
    Events(&'a [Event]),
}

/// Start of the window the check looks at, ms since 1970, for a check made at `now_secs`.
pub fn since_ms(now_secs: u64) -> i64 {
    i64::try_from(now_secs)
        .unwrap_or(i64::MAX / 1000)
        .saturating_mul(1000)
        .saturating_sub(WINDOW_MS)
}

/// Reading for `input` at `now_secs` (Unix seconds).
pub fn check(input: Input<'_>, now_secs: u64) -> Reading {
    match input {
        Input::NotRead => reading(
            now_secs,
            Status::NotChecked,
            Severity::Watch,
            "Recent crashes".into(),
            "Event Watch has not read the Windows event log yet.".into(),
        ),
        Input::Unreadable(reason) => reading(
            now_secs,
            Status::Unavailable,
            Severity::Watch,
            "Recent crashes".into(),
            format!(
                "Crashes could not be checked: {}",
                plain(reason, "the event log could not be read")
            ),
        ),
        Input::Events(events) => from_events(events, now_secs),
    }
}

fn from_events(events: &[Event], now_secs: u64) -> Reading {
    let since = since_ms(now_secs);
    let crashes: Vec<_> = detect_crashes(events, DEFAULT_WINDOW_MS, |_| None)
        .into_iter()
        .filter(|c| c.noticed_ms >= since)
        .collect();

    // `detect_crashes` returns newest first.
    let blue: Vec<_> = crashes
        .iter()
        .filter(|c| matches!(c.kind, CrashKind::BlueScreen { .. }))
        .collect();
    let restarts = crashes.len() - blue.len();

    if let Some(latest) = blue.first() {
        let code = match latest.kind {
            CrashKind::BlueScreen { code, .. } => match bugchecks::lookup(code) {
                Some(b) => format!("{} {}", bugchecks::hex(code), b.name),
                None => bugchecks::hex(code),
            },
            _ => String::new(),
        };
        let n = blue.len();
        return reading(
            now_secs,
            Status::Attention,
            Severity::Problem,
            format!(
                "{} in the last {WINDOW_DAYS} days",
                count(n, "blue screen", "blue screens")
            ),
            format!(
                "{} recorded, the latest on {} UTC (stop code {code}).{}",
                count(n, "blue screen", "blue screens"),
                ms_to_text(latest.noticed_ms),
                if restarts > 0 {
                    format!(
                        " Also {}.",
                        count(restarts, "unexpected restart", "unexpected restarts")
                    )
                } else {
                    String::new()
                }
            ),
        );
    }
    if restarts >= RESTARTS_TO_WATCH {
        let latest = crashes.first().map_or(0, |c| c.noticed_ms);
        return reading(
            now_secs,
            Status::Attention,
            Severity::Watch,
            format!(
                "{} in the last {WINDOW_DAYS} days",
                count(restarts, "unexpected restart", "unexpected restarts")
            ),
            format!(
                "Windows restarted without a clean shutdown {restarts} times, the latest on {} UTC. \
                 No blue screen was recorded.",
                ms_to_text(latest)
            ),
        );
    }
    if restarts == 1 {
        let latest = crashes.first().map_or(0, |c| c.noticed_ms);
        return reading(
            now_secs,
            Status::Ok,
            Severity::Watch,
            format!("One unexpected restart in the last {WINDOW_DAYS} days"),
            format!(
                "Windows restarted once without a clean shutdown, on {} UTC. No blue screen was \
                 recorded. This is not reported as a problem unless it happens again.",
                ms_to_text(latest)
            ),
        );
    }
    reading(
        now_secs,
        Status::Ok,
        Severity::Watch,
        format!("No crashes in the last {WINDOW_DAYS} days"),
        format!("No crashes recorded in the last {WINDOW_DAYS} days."),
    )
}

fn count(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

fn reading(
    now_secs: u64,
    status: Status,
    severity: Severity,
    title: String,
    observation: String,
) -> Reading {
    Reading {
        id: ID,
        title: clip(&title, TITLE_MAX),
        observation: clip(&observation, OBSERVATION_MAX),
        status,
        severity,
        measured_at: now_secs,
        stale_after_secs: STALE_AFTER_SECS,
        action: Some(Action::OpenPage {
            label: clip("Open crashes", LABEL_MAX),
            route: ROUTE.into(),
        }),
    }
}

/// Text from outside (an error message): control characters and markup characters removed,
/// blanks folded, `fallback` when nothing is left. Text is drawn plain by the host, so this only
/// keeps odd characters out of the reading.
fn plain(text: &str, fallback: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .filter(|c| !matches!(c, '<' | '>' | '`' | '*' | '[' | ']'))
        .collect();
    let folded = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let folded = folded.trim_end_matches('.');
    if folded.is_empty() {
        fallback.to_string()
    } else {
        folded.to_string()
    }
}

/// Cut to `max` characters, with a final full stop kept when the text is cut.
fn clip(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out = out.trim_end().to_string();
    out.push('…');
    out
}

/// Whether to check now: at start (never checked), then every [`REFRESH_SECS`]. A clock that
/// went backwards also counts as time to check, so a wrong clock cannot freeze the reading.
pub fn should_refresh(last_checked_secs: Option<u64>, now_secs: u64) -> bool {
    match last_checked_secs {
        None => true,
        Some(last) => now_secs < last || now_secs - last >= REFRESH_SECS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;
    const DAY_MS: i64 = 24 * 60 * 60 * 1000;
    const NOW_MS: i64 = NOW as i64 * 1000;

    fn ev(provider: &str, id: u32, level: u8, t: i64, data: &[(&str, &str)]) -> Event {
        Event {
            record_id: (t / 1000) as u64 + u64::from(id),
            time_ms: t,
            channel: "System".into(),
            provider: provider.into(),
            id,
            level,
            data: data
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            message: None,
        }
    }

    /// A restart with the power button held (Kernel-Power 41, no bug check).
    fn restart(t: i64) -> Event {
        ev(
            "Microsoft-Windows-Kernel-Power",
            41,
            1,
            t,
            &[
                ("BugcheckCode", "0"),
                ("PowerButtonTimestamp", "132800000000000000"),
            ],
        )
    }

    /// A blue screen: Kernel-Power 41 with a bug check code.
    fn blue_screen(t: i64) -> Event {
        ev(
            "Microsoft-Windows-Kernel-Power",
            41,
            1,
            t,
            &[
                ("BugcheckCode", "313"),
                ("BugcheckParameter1", "0x3"),
                ("PowerButtonTimestamp", "0"),
            ],
        )
    }

    fn noise(t: i64) -> Event {
        ev("Disk", 153, 3, t, &[("DiskNumber", "0")])
    }

    fn run(events: &[Event]) -> Reading {
        check(Input::Events(events), NOW)
    }

    fn assert_limits(r: &Reading) {
        assert_eq!(r.id, "recent-crashes");
        assert!(!r.title.trim().is_empty());
        assert!(r.title.chars().count() <= 80, "{}", r.title);
        assert!(r.observation.chars().count() <= 240, "{}", r.observation);
        assert!(!r.observation.trim().is_empty());
        assert_eq!(r.stale_after_secs, 1800);
        assert_eq!(r.measured_at, NOW);
        for s in [&r.title, &r.observation] {
            assert!(
                !s.contains('\n') && !s.chars().any(char::is_control),
                "{s:?}"
            );
            assert!(!s.contains(['<', '>', '`', '*', '[', ']']), "{s:?}");
        }
        match &r.action {
            Some(Action::OpenPage { label, route }) => {
                assert!(label.chars().count() <= 40);
                assert_eq!(route, "Crashes");
                assert!(crate::PAGES.contains(&route.as_str()));
            }
            None => {}
        }
        if r.status != Status::Attention {
            assert_eq!(r.severity, Severity::Watch);
        }
    }

    #[test]
    fn no_crashes_is_ok_and_says_what_was_measured() {
        let r = run(&[noise(NOW_MS - DAY_MS)]);
        assert_eq!(r.status, Status::Ok);
        assert_eq!(r.observation, "No crashes recorded in the last 7 days.");
        assert!(!r.observation.to_lowercase().contains("healthy"));
        assert!(!r.title.to_lowercase().contains("healthy"));
        assert_limits(&r);
    }

    #[test]
    fn empty_log_is_ok() {
        let r = run(&[]);
        assert_eq!(r.status, Status::Ok);
        assert_limits(&r);
    }

    #[test]
    fn one_restart_is_ok_but_not_hidden() {
        let r = run(&[restart(NOW_MS - 2 * DAY_MS)]);
        assert_eq!(r.status, Status::Ok);
        assert!(r.title.contains("One unexpected restart"), "{}", r.title);
        assert!(!r.observation.contains("No crashes recorded"));
        assert_limits(&r);
    }

    #[test]
    fn two_restarts_is_attention_watch() {
        let r = run(&[restart(NOW_MS - 2 * DAY_MS), restart(NOW_MS - DAY_MS)]);
        assert_eq!(r.status, Status::Attention);
        assert_eq!(r.severity, Severity::Watch);
        assert_eq!(r.title, "2 unexpected restarts in the last 7 days");
        assert_limits(&r);
    }

    #[test]
    fn blue_screen_is_attention_problem() {
        let r = run(&[blue_screen(NOW_MS - 3 * DAY_MS)]);
        assert_eq!(r.status, Status::Attention);
        assert_eq!(r.severity, Severity::Problem);
        assert_eq!(r.title, "1 blue screen in the last 7 days");
        assert!(r.observation.contains("0x00000139"), "{}", r.observation);
        assert_limits(&r);
    }

    #[test]
    fn blue_screen_wins_over_restarts_and_mentions_them() {
        let r = run(&[
            restart(NOW_MS - 5 * DAY_MS),
            blue_screen(NOW_MS - 3 * DAY_MS),
            blue_screen(NOW_MS - 2 * DAY_MS),
            restart(NOW_MS - DAY_MS),
        ]);
        assert_eq!(r.severity, Severity::Problem);
        assert_eq!(r.title, "2 blue screens in the last 7 days");
        assert!(r.observation.contains("Also 2 unexpected restarts"));
        assert_limits(&r);
    }

    #[test]
    fn crashes_older_than_seven_days_are_ignored() {
        let r = run(&[
            blue_screen(NOW_MS - 8 * DAY_MS),
            restart(NOW_MS - 9 * DAY_MS),
            restart(NOW_MS - 10 * DAY_MS),
        ]);
        assert_eq!(r.status, Status::Ok);
        assert!(r.observation.starts_with("No crashes recorded"));
    }

    #[test]
    fn the_edge_of_the_window_counts() {
        let r = run(&[blue_screen(NOW_MS - 7 * DAY_MS)]);
        assert_eq!(r.status, Status::Attention);
        let r = run(&[blue_screen(NOW_MS - 7 * DAY_MS - 1)]);
        assert_eq!(r.status, Status::Ok);
    }

    #[test]
    fn restarts_close_together_are_one_crash() {
        // Kernel-Power 41 events within two minutes are one crash (detect_crashes dedupes).
        let t = NOW_MS - DAY_MS;
        let r = run(&[restart(t), restart(t + 30_000)]);
        assert_eq!(r.status, Status::Ok);
    }

    #[test]
    fn unreadable_log_is_unavailable_with_the_reason() {
        let r = check(Input::Unreadable("Access is denied."), NOW);
        assert_eq!(r.status, Status::Unavailable);
        assert!(
            r.observation.contains("Access is denied"),
            "{}",
            r.observation
        );
        assert_limits(&r);
    }

    #[test]
    fn unreadable_reason_is_cleaned_and_cut() {
        let long = format!("<b>bad</b>\r\n`x` [link](http://x) *{}", "y".repeat(500));
        let r = check(Input::Unreadable(&long), NOW);
        assert_eq!(r.status, Status::Unavailable);
        assert_limits(&r);
        assert!(r.observation.ends_with('…'));
        let r = check(Input::Unreadable("  \n "), NOW);
        assert_limits(&r);
        assert!(r.observation.contains("could not be read"));
    }

    #[test]
    fn not_read_yet_is_not_checked() {
        let r = check(Input::NotRead, NOW);
        assert_eq!(r.status, Status::NotChecked);
        assert_limits(&r);
    }

    #[test]
    fn every_status_has_text_within_limits() {
        for r in [
            check(Input::NotRead, NOW),
            check(Input::Unreadable("x"), NOW),
            run(&[]),
            run(&[restart(NOW_MS - DAY_MS)]),
            run(&[restart(NOW_MS - DAY_MS), restart(NOW_MS - 2 * DAY_MS)]),
            run(&[blue_screen(NOW_MS - DAY_MS)]),
        ] {
            assert_limits(&r);
        }
    }

    #[test]
    fn clip_keeps_short_text_and_cuts_long_text() {
        assert_eq!(clip("  hi ", 10), "hi");
        let cut = clip(&"a".repeat(100), 80);
        assert_eq!(cut.chars().count(), 80);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn cadence_checks_at_start_and_every_thirty_minutes() {
        assert!(should_refresh(None, NOW));
        assert!(!should_refresh(Some(NOW), NOW));
        assert!(!should_refresh(Some(NOW - 1799), NOW));
        assert!(should_refresh(Some(NOW - 1800), NOW));
        assert!(should_refresh(Some(NOW - 10_000), NOW));
    }

    #[test]
    fn cadence_recovers_from_a_clock_that_went_backwards() {
        assert!(should_refresh(Some(NOW + 5), NOW));
    }
}
