//! Feeds the `memory-pressure` finding for Home (docs/PLAN-health.md, section 4) from the
//! pressure model the add-on already runs. The check itself is the pure `health` module; this
//! file only supplies its inputs and hands its reading to the SDK.
//!
//! The model is advanced from two places, so the finding keeps moving with the page closed:
//! the per-frame `tick` while the page is shown (through the existing `evaluate`), and the
//! 200 ms `tick_background` the runtime calls while the add-on is suspended. A reading is
//! taken at the existing cadences (1 s open, 10 s closed); the finding itself is recomputed
//! at most once a minute (`health::should_refresh`). Everything stays in memory.

use std::time::Instant;

use toolboks_addon_sdk::Finding;

use super::engine::eval_interval;
use super::health::{Source, Usage};
use super::pressure::{Assessment, Signals};
use super::Memory;

/// What the finding says when Windows would not give the basic memory numbers.
const UNREADABLE: &str = "Windows did not return the memory counters.";

/// Unix seconds now (0 if the clock is before 1970).
pub(super) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

impl Memory {
    /// One pressure reading at `now_ms` (the model's clock) and `now_secs` (Unix seconds, for
    /// the finding). Feeds the model and the health tracker; `None` when the source could not
    /// be read, which the finding then reports as unavailable.
    pub(super) fn read_pressure(
        &mut self,
        now_ms: u64,
        now_secs: u64,
    ) -> Option<(Signals, Assessment)> {
        let source = self.signals.as_mut()?;
        let Some(signals) = source.read() else {
            self.health_source = Source::Unreadable(UNREADABLE.to_string());
            return None;
        };
        let assessment = self.pressure.evaluate(now_ms, &signals);
        self.health.observe(now_secs, &assessment);
        self.health.note_usage(Usage {
            available: signals.available,
            total: signals.total_ram,
        });
        self.health_source = Source::Read(assessment.clone());
        Some((signals, assessment))
    }

    /// Recomputes the finding if one is due. Cheap otherwise: no allocation, no reading.
    pub(super) fn update_health(&mut self, now_secs: u64) {
        if self.health.is_due(now_secs, &self.health_source) {
            self.health.refresh(now_secs, self.health_source.clone());
        }
    }

    /// The runtime's background tick (the page is closed): advance the model at the closed-page
    /// cadence, then the finding. Does not run the automatic scheduler: nothing starts while
    /// the window is hidden, as before.
    pub(super) fn health_background_tick(&mut self) {
        if !self.active && self.last_eval.elapsed() >= eval_interval(false) {
            self.last_eval = Instant::now();
            let now_ms = self.now_ms();
            let _ = self.read_pressure(now_ms, unix_now());
        }
        self.update_health(unix_now());
    }

    /// Home's "Check again": the finding is recomputed from the latest reading (at most 10 s
    /// old while the page is closed, 1 s while open) without waiting for the minute.
    pub(super) fn health_check_now(&mut self) {
        self.health
            .refresh_now(unix_now(), self.health_source.clone());
    }

    /// The finding Home shows. Built once per refresh and kept, so it is the same value on
    /// every call between refreshes and the runtime sends nothing new.
    pub(super) fn health_findings(&self) -> Vec<Finding> {
        self.health
            .reading()
            .map(Finding::from)
            .into_iter()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pressure::{limits::GIB, SignalSource};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use toolboks_addon_sdk::{FindingAction, FindingStatus, NativeAddon, Severity};

    /// A signal source the test steers: `None` means "cannot be read".
    #[derive(Clone)]
    struct Fake(Arc<Mutex<Option<Signals>>>);

    impl SignalSource for Fake {
        fn read(&mut self) -> Option<Signals> {
            *self.0.lock().unwrap()
        }
    }

    const RAM: u64 = 16 * GIB;

    fn calm() -> Signals {
        Signals {
            available: 8 * GIB,
            total_ram: RAM,
            commit_used: 10 * GIB,
            commit_limit: 40 * GIB,
            compressed: Some(GIB),
            hard_faults_per_sec: Some(5.0),
            low_memory_signalled: false,
        }
    }

    /// Two signals at once (little memory ready, Windows reading from disk): under pressure
    /// once both have held for a minute.
    fn squeezed() -> Signals {
        Signals {
            available: GIB,
            hard_faults_per_sec: Some(900.0),
            ..calm()
        }
    }

    fn memory(initial: Option<Signals>) -> (Memory, Arc<Mutex<Option<Signals>>>) {
        let shared = Arc::new(Mutex::new(initial));
        let mut m = Memory::new();
        m.signals = Some(Box::new(Fake(shared.clone())));
        (m, shared)
    }

    /// One reading, then the finding, `secs` seconds into the test (clock 1_000_000 + secs).
    fn step(m: &mut Memory, secs: u64) {
        let _ = m.read_pressure(secs * 1000, 1_000_000 + secs);
        m.update_health(1_000_000 + secs);
    }

    fn only(m: &Memory) -> Finding {
        let list = m.findings();
        assert_eq!(list.len(), 1);
        list.into_iter().next().unwrap()
    }

    #[test]
    fn not_checked_until_the_first_reading_and_stable_meanwhile() {
        let (m, _) = memory(Some(calm()));
        let first = only(&m);
        assert_eq!(first.id, "memory-pressure");
        assert_eq!(first.status, FindingStatus::NotChecked);
        assert_eq!(first.action, None);
        assert_eq!(m.findings(), vec![first], "same value on every call");
    }

    #[test]
    fn ok_when_memory_is_fine() {
        let (mut m, _) = memory(Some(calm()));
        step(&mut m, 0);
        let f = only(&m);
        assert_eq!(f.status, FindingStatus::Ok);
        assert_eq!(f.id, "memory-pressure");
        assert_eq!(f.stale_after_secs, 300);
        assert_eq!(f.measured_at, 1_000_000);
        // A short value for Home's row: the share in use, measured (8 GB ready of 16 GB).
        assert_eq!(f.observation, "50% in use");
        assert_eq!(f.action, None);
        assert_eq!((f.why, f.evidence), (None, None));
    }

    #[test]
    fn under_pressure_needs_attention_with_the_reasons_and_opens_home() {
        let (mut m, signals) = memory(Some(calm()));
        step(&mut m, 0);
        *signals.lock().unwrap() = Some(squeezed());
        // Held for a minute, read every 10 s like the closed-page cadence.
        for secs in (10..=120).step_by(10) {
            step(&mut m, secs);
        }
        let f = only(&m);
        assert_eq!(f.status, FindingStatus::Attention);
        assert_eq!(f.severity, Severity::Watch);
        assert_eq!(f.title, "Memory is under pressure");
        assert!(f.observation.contains("Only 1.0 GB is ready to use"));
        assert!(f.observation.contains("reading memory back from disk"));
        // Why it matters, and the memory in use at the latest reading (1 GB ready of 16 GB).
        assert_eq!(f.why.as_deref(), Some(crate::health::WHY));
        assert_eq!(
            f.evidence,
            Some(toolboks_addon_sdk::Evidence::bar(
                0.9375,
                "15.0 GB in use now",
                "1.0 GB available"
            ))
        );
        assert_eq!(
            f.action,
            Some(FindingAction::OpenPage {
                label: "Open Memory Cleaner".into(),
                route: "Home".into()
            })
        );
    }

    #[test]
    fn a_short_spell_between_two_refreshes_is_not_lost() {
        let (mut m, signals) = memory(Some(calm()));
        step(&mut m, 0);
        *signals.lock().unwrap() = Some(squeezed());
        for secs in (10..=80).step_by(10) {
            let _ = m.read_pressure(secs * 1000, 1_000_000 + secs);
        }
        *signals.lock().unwrap() = Some(calm());
        step(&mut m, 90); // pressure cleared; the finding refreshes only now (a minute on)
        step(&mut m, 120);
        let f = only(&m);
        assert_eq!(f.status, FindingStatus::Attention, "{f:?}");
        assert!(f.title.contains("a short while ago"), "{f:?}");
    }

    #[test]
    fn stays_attention_for_30_minutes_after_the_pressure_then_goes_ok() {
        let (mut m, signals) = memory(Some(squeezed()));
        for secs in (0..=70).step_by(10) {
            step(&mut m, secs);
        }
        assert_eq!(only(&m).status, FindingStatus::Attention);
        *signals.lock().unwrap() = Some(calm());
        let cleared = 120;
        step(&mut m, cleared);
        let tail = only(&m);
        assert_eq!(tail.status, FindingStatus::Attention);
        assert!(tail.title.contains("a short while ago"));
        // The picture shows memory now (calm again), labelled as now.
        assert!(tail.why.is_some());
        assert_eq!(
            tail.evidence
                .as_ref()
                .map(toolboks_addon_sdk::Evidence::labels),
            Some(("8.0 GB in use now", "8.0 GB available"))
        );

        // 29 minutes on: still the tail. 31 minutes on: Ok again.
        step(&mut m, cleared + 29 * 60);
        assert_eq!(only(&m).status, FindingStatus::Attention);
        step(&mut m, cleared + 31 * 60);
        let f = only(&m);
        assert_eq!(f.status, FindingStatus::Ok);
        assert_eq!(f.observation, "50% in use");
        assert_eq!(f.why, None);
    }

    #[test]
    fn unreadable_source_is_unavailable() {
        let (mut m, signals) = memory(None);
        step(&mut m, 0);
        let f = only(&m);
        assert_eq!(f.status, FindingStatus::Unavailable);
        assert!(f.observation.contains("cannot read memory use"));
        assert!(f.observation.contains(UNREADABLE));
        assert_eq!(f.action, None);

        // It recovers once the source answers (refreshes at the next minute).
        *signals.lock().unwrap() = Some(calm());
        step(&mut m, 60);
        assert_eq!(only(&m).status, FindingStatus::Ok);
    }

    #[test]
    fn nothing_changes_between_refreshes_so_nothing_new_is_sent() {
        let (mut m, _) = memory(Some(calm()));
        step(&mut m, 0);
        let before = m.findings();
        // The runtime diffs `findings()`; ticks inside the minute leave it equal.
        for secs in 1..60 {
            step(&mut m, secs);
            assert_eq!(m.findings(), before, "changed at {secs} s");
        }
        // A real refresh a minute later has a new time, so it is sent once.
        step(&mut m, 60);
        let after = m.findings();
        assert_ne!(after, before);
        assert_eq!(after[0].measured_at, 1_000_060);
        step(&mut m, 61);
        assert_eq!(m.findings(), after);
    }

    #[test]
    fn check_now_refreshes_at_once() {
        let (mut m, _) = memory(Some(calm()));
        step(&mut m, 0);
        let before = only(&m);
        // Not due: a plain update keeps the old reading.
        m.update_health(1_000_000 + 5);
        assert_eq!(only(&m), before);
        // "Check again": recomputed now (the real clock), whatever the minute says.
        m.health_check_now();
        let after = only(&m);
        assert!(after.measured_at >= unix_now() - 1);
        assert_eq!(after.status, FindingStatus::Ok);
    }

    #[test]
    fn check_now_before_any_reading_stays_honest() {
        let (mut m, _) = memory(Some(calm()));
        m.check_now();
        assert_eq!(only(&m).status, FindingStatus::NotChecked);
    }

    #[test]
    fn the_background_tick_advances_the_model_with_the_page_closed() {
        let (mut m, _) = memory(Some(squeezed()));
        m.suspend();
        assert!(m.has_background_work());
        let every = eval_interval(false);
        // No frames at all: only the runtime's background ticks, one reading each. Each tick
        // is made due by moving the model's clock on by one closed-page interval.
        for _ in 0..8 {
            m.epoch -= every;
            m.last_eval = Instant::now() - every - Duration::from_millis(1);
            m.tick_background();
        }
        // The first tick already replaced "not checked".
        assert_ne!(only(&m).status, FindingStatus::NotChecked);
        // Both signals have held for over a minute by now, so "Check again" says so.
        m.check_now();
        let f = only(&m);
        assert_eq!(f.status, FindingStatus::Attention, "{f:?}");
        assert!(f.observation.contains("Only 1.0 GB"));
    }

    #[test]
    fn a_tick_without_a_due_reading_does_not_read_the_source() {
        struct Counting(Arc<Mutex<u32>>);
        impl SignalSource for Counting {
            fn read(&mut self) -> Option<Signals> {
                *self.0.lock().unwrap() += 1;
                Some(calm())
            }
        }
        let count = Arc::new(Mutex::new(0));
        let mut m = Memory::new();
        m.signals = Some(Box::new(Counting(count.clone())));
        m.suspend();
        // Fresh: the last reading was just now, so 200 ms ticks read nothing.
        m.last_eval = Instant::now();
        for _ in 0..50 {
            m.tick_background();
        }
        assert_eq!(*count.lock().unwrap(), 0);
        m.last_eval = Instant::now() - eval_interval(false) - Duration::from_millis(1);
        m.tick_background();
        assert_eq!(*count.lock().unwrap(), 1);
    }

    #[test]
    fn suspending_keeps_the_pressure_model_and_the_source() {
        let (mut m, _) = memory(Some(squeezed()));
        for secs in (0..=60).step_by(10) {
            step(&mut m, secs);
        }
        m.suspend();
        assert!(m.signals.is_some(), "the source stays for the closed page");
        m.resume();
        assert!(m.signals.is_some());
    }

    #[test]
    fn the_finding_fits_the_protocol_limits() {
        let (mut m, _) = memory(Some(squeezed()));
        for secs in (0..=70).step_by(10) {
            step(&mut m, secs);
        }
        let f = only(&m);
        assert!(Finding::valid_id(&f.id));
        assert!(f.title.chars().count() <= 80);
        assert!(f.observation.chars().count() <= 240);
        assert!((60..=604_800).contains(&f.stale_after_secs));
        assert!(f.why.as_ref().is_some_and(|w| w.chars().count() <= 200));
        let (left, right) = f.evidence.as_ref().expect("a bar").labels();
        assert!(left.chars().count() <= 40 && right.chars().count() <= 40);
        if let Some(FindingAction::OpenPage { label, route }) = &f.action {
            assert!(label.chars().count() <= 40);
            assert!(crate::PAGES.contains(&route.as_str()));
        }
    }
}
