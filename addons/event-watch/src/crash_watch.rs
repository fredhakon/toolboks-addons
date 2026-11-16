//! Runs the `recent-crashes` check (see `crash_check`) in the background: once at start and then
//! every 30 minutes, always on a worker thread, never on the drawing thread.
//!
//! No timer thread and no polling between checks. The add-on runtime asks `has_background_work`
//! and only then ticks every 200 ms, so [`CrashWatch::wants_tick`] is true only while a read runs
//! or a check is due. Between checks the process is asleep until the host sends its next message.
//! The result is kept in memory only; nothing is written.

use crate::crash_check::{self, Input, Reading};
use crate::source::{self, EventSource, Query, WinEvents};
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// The Event Log reader. The real one in the add-on, a fake in tests.
pub type Source = Arc<dyn EventSource + Send + Sync>;
/// Unix seconds now. The real clock in the add-on, a fake in tests.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Look a little before the window so a blue screen's report that precedes its restart marker
/// is still found.
const MARGIN_MS: i64 = 2 * 60 * 1000;
/// How often a visible page looks at a running read.
const POLL_WHILE_SHOWN: Duration = Duration::from_millis(250);

pub struct CrashWatch {
    source: Source,
    clock: Clock,
    /// The last result; "not read yet" until the first read has finished. Kept as it is between
    /// checks so `findings()` returns the same value every time.
    reading: Reading,
    /// When the last read started (Unix seconds).
    last_started: Option<u64>,
    /// "Check again" was pressed: read as soon as no read is running.
    forced: bool,
    worker: Option<mpsc::Receiver<Reading>>,
}

fn system_clock() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Default for CrashWatch {
    fn default() -> Self {
        Self::with(Arc::new(WinEvents), Arc::new(system_clock))
    }
}

impl CrashWatch {
    pub fn with(source: Source, clock: Clock) -> Self {
        let reading = crash_check::check(Input::NotRead, clock());
        Self {
            source,
            clock,
            reading,
            last_started: None,
            forced: false,
            worker: None,
        }
    }

    /// The current reading as the SDK finding Home shows.
    pub fn finding(&self) -> toolboks_addon_sdk::Finding {
        self.reading.finding()
    }

    /// A read should start now: the first one, the 30-minute one, or one the user asked for.
    pub fn due(&self) -> bool {
        self.worker.is_none()
            && (self.forced || crash_check::should_refresh(self.last_started, (self.clock)()))
    }

    /// True while a read runs or one is due; false while the check is waiting for its next time.
    pub fn wants_tick(&self) -> bool {
        self.worker.is_some() || self.due()
    }

    /// "Check again": reads now, unless a read is already running.
    pub fn request(&mut self) {
        self.forced = true;
    }

    /// Takes a finished read and starts the next one when due. `ctx` is given while the page is
    /// shown, so the page is redrawn when the next check is due or the read has finished.
    pub fn step(&mut self, ctx: Option<&egui::Context>) {
        if let Some(rx) = &self.worker {
            match rx.try_recv() {
                Ok(reading) => {
                    self.reading = reading;
                    self.worker = None;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    if let Some(ctx) = ctx {
                        ctx.request_repaint_after(POLL_WHILE_SHOWN);
                    }
                    return;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.reading = crash_check::check(
                        Input::Unreadable("the check stopped unexpectedly"),
                        (self.clock)(),
                    );
                    self.worker = None;
                }
            }
        }
        if self.due() {
            self.start();
            if let Some(ctx) = ctx {
                ctx.request_repaint_after(POLL_WHILE_SHOWN);
            }
        } else if let (Some(ctx), Some(last)) = (ctx, self.last_started) {
            let wait = (last + crash_check::REFRESH_SECS).saturating_sub((self.clock)());
            ctx.request_repaint_after(Duration::from_secs(wait.max(1)));
        }
    }

    fn start(&mut self) {
        let now = (self.clock)();
        self.last_started = Some(now);
        self.forced = false;
        let (tx, rx) = mpsc::channel();
        let source = self.source.clone();
        let spawned = std::thread::Builder::new()
            .name("crash-check".into())
            .spawn(move || {
                let _ = tx.send(read(&*source, now));
            });
        match spawned {
            Ok(_) => self.worker = Some(rx),
            Err(error) => {
                self.reading = crash_check::check(Input::Unreadable(&error.to_string()), now);
            }
        }
    }
}

/// One read of the System log's crash markers over the check's window, and the reading for it.
/// A log that cannot be read, or only in part, is `Unavailable` rather than "no crashes". A
/// partial read that already found crashes still reports them.
pub fn read(src: &dyn EventSource, now_secs: u64) -> Reading {
    let query = Query {
        channel: "System".into(),
        since_ms: crash_check::since_ms(now_secs) - MARGIN_MS,
        until_ms: None,
        levels: vec![source::CRASH_MARKERS],
        max: source::LOAD_MAX_MARKERS,
        newest_first: true,
    };
    match src.query_partial(&query) {
        Err(error) => crash_check::check(Input::Unreadable(&error), now_secs),
        Ok(read) => {
            let reading = crash_check::check(Input::Events(&read.events), now_secs);
            match read.error {
                Some(error) if reading.status != crash_check::Status::Attention => {
                    crash_check::check(Input::Unreadable(&error), now_secs)
                }
                _ => reading,
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::{ChannelStatus, Event};
    use crate::source::QueryRead;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use toolboks_addon_sdk::{FindingAction, FindingStatus, Severity};

    pub const NOW: u64 = 1_800_000_000;
    const NOW_MS: i64 = NOW as i64 * 1000;
    const HOUR_MS: i64 = 3_600_000;
    const DAY_MS: i64 = 24 * HOUR_MS;

    /// A fake Event Log: the same answer every time, and a count of reads.
    pub struct Fake {
        pub events: Vec<Event>,
        pub error: Option<String>,
        pub fail: bool,
        pub reads: AtomicUsize,
        pub queries: std::sync::Mutex<Vec<Query>>,
    }

    impl Fake {
        pub fn new(events: Vec<Event>) -> Self {
            Self {
                events,
                error: None,
                fail: false,
                reads: AtomicUsize::new(0),
                queries: Default::default(),
            }
        }
    }

    impl EventSource for Fake {
        fn query(&self, q: &Query) -> Result<Vec<Event>, String> {
            self.query_partial(q).map(|r| r.events)
        }
        fn query_partial(&self, q: &Query) -> Result<QueryRead, String> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.queries.lock().unwrap().push(q.clone());
            if self.fail {
                return Err("Access is denied.".into());
            }
            Ok(QueryRead {
                events: self.events.clone(),
                error: self.error.clone(),
            })
        }
        fn channel_status(&self, channel: &str) -> ChannelStatus {
            ChannelStatus {
                name: channel.into(),
                readable: true,
                error: None,
                oldest_ms: None,
            }
        }
    }

    fn marker(t: i64, bugcheck: &str) -> Event {
        Event {
            record_id: (t / 1000) as u64,
            time_ms: t,
            channel: "System".into(),
            provider: "Microsoft-Windows-Kernel-Power".into(),
            id: 41,
            level: 1,
            data: vec![("BugcheckCode".into(), bugcheck.into())],
            message: None,
        }
    }
    fn restart(t: i64) -> Event {
        marker(t, "0")
    }
    fn blue_screen(t: i64) -> Event {
        marker(t, "313")
    }

    /// A watch over `fake` with a clock the test moves.
    fn watch(fake: Arc<Fake>) -> (CrashWatch, Arc<AtomicU64>) {
        let time = Arc::new(AtomicU64::new(NOW));
        let clock = time.clone();
        let watch = CrashWatch::with(fake, Arc::new(move || clock.load(Ordering::SeqCst)));
        (watch, time)
    }

    /// Ticks the way the runtime does while suspended, until the add-on has nothing left to do.
    fn settle(watch: &mut CrashWatch) {
        for _ in 0..2000 {
            if !watch.wants_tick() {
                return;
            }
            watch.step(None);
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("the check never finished");
    }

    #[test]
    fn before_the_first_read_the_finding_says_not_checked_and_a_read_is_due() {
        let (watch, _) = watch(Arc::new(Fake::new(vec![])));
        let f = watch.finding();
        assert_eq!(f.id, "recent-crashes");
        assert_eq!(f.status, FindingStatus::NotChecked);
        assert!(watch.wants_tick());
    }

    #[test]
    fn no_crashes_is_ok_and_the_check_then_goes_to_sleep() {
        let fake = Arc::new(Fake::new(vec![]));
        let (mut watch, _) = watch(fake.clone());
        settle(&mut watch);
        let f = watch.finding();
        assert_eq!(f.status, FindingStatus::Ok);
        assert_eq!(f.title, "No crashes in the last 7 days");
        assert_eq!(f.measured_at, NOW);
        assert_eq!(f.stale_after_secs, 1800);
        assert_eq!(
            f.action,
            Some(FindingAction::OpenPage {
                label: "Open crashes".into(),
                route: "Crashes".into()
            })
        );
        assert!(!watch.wants_tick(), "asleep until the next check");
        assert_eq!(fake.reads.load(Ordering::SeqCst), 1, "one read per cycle");
    }

    #[test]
    fn one_restart_is_ok_and_says_so() {
        let fake = Arc::new(Fake::new(vec![restart(NOW_MS - DAY_MS)]));
        let (mut watch, _) = watch(fake);
        settle(&mut watch);
        let f = watch.finding();
        assert_eq!(f.status, FindingStatus::Ok);
        assert!(f.title.starts_with("One unexpected restart"), "{}", f.title);
    }

    #[test]
    fn two_restarts_are_attention_watch() {
        let fake = Arc::new(Fake::new(vec![
            restart(NOW_MS - DAY_MS),
            restart(NOW_MS - 3 * DAY_MS),
        ]));
        let (mut watch, _) = watch(fake);
        settle(&mut watch);
        let f = watch.finding();
        assert_eq!(f.status, FindingStatus::Attention);
        assert_eq!(f.severity, Severity::Watch);
        assert!(f.title.starts_with("2 unexpected restarts"), "{}", f.title);
    }

    #[test]
    fn a_blue_screen_is_attention_problem() {
        let fake = Arc::new(Fake::new(vec![blue_screen(NOW_MS - 2 * DAY_MS)]));
        let (mut watch, _) = watch(fake);
        settle(&mut watch);
        let f = watch.finding();
        assert_eq!(f.status, FindingStatus::Attention);
        assert_eq!(f.severity, Severity::Problem);
        assert!(f.title.starts_with("1 blue screen"), "{}", f.title);
        assert!(f.observation.contains("00000139"), "{}", f.observation);
    }

    #[test]
    fn crashes_older_than_seven_days_do_not_count() {
        let fake = Arc::new(Fake::new(vec![blue_screen(NOW_MS - 8 * DAY_MS)]));
        let (mut watch, _) = watch(fake);
        settle(&mut watch);
        assert_eq!(watch.finding().status, FindingStatus::Ok);
    }

    #[test]
    fn a_log_that_cannot_be_read_is_unavailable_not_a_crash() {
        let mut fake = Fake::new(vec![]);
        fake.fail = true;
        let (mut watch, _) = watch(Arc::new(fake));
        settle(&mut watch);
        let f = watch.finding();
        assert_eq!(f.status, FindingStatus::Unavailable);
        assert!(
            f.observation.contains("Access is denied"),
            "{}",
            f.observation
        );
    }

    #[test]
    fn a_partial_read_with_no_crash_found_is_unavailable_not_ok() {
        let mut fake = Fake::new(vec![]);
        fake.error = Some("The log was cleared while reading".into());
        let (mut watch, _) = watch(Arc::new(fake));
        settle(&mut watch);
        assert_eq!(watch.finding().status, FindingStatus::Unavailable);
    }

    #[test]
    fn a_partial_read_that_found_a_blue_screen_still_reports_it() {
        let mut fake = Fake::new(vec![blue_screen(NOW_MS - DAY_MS)]);
        fake.error = Some("The log was cleared while reading".into());
        let (mut watch, _) = watch(Arc::new(fake));
        settle(&mut watch);
        assert_eq!(watch.finding().status, FindingStatus::Attention);
    }

    #[test]
    fn the_read_asks_only_for_system_crash_markers_in_the_window() {
        let fake = Arc::new(Fake::new(vec![]));
        let (mut watch, _) = watch(fake.clone());
        settle(&mut watch);
        let queries = fake.queries.lock().unwrap();
        assert_eq!(queries.len(), 1);
        let q = &queries[0];
        assert_eq!(q.channel, "System");
        assert_eq!(q.levels, vec![source::CRASH_MARKERS]);
        assert!(q.since_ms < NOW_MS - 7 * DAY_MS);
        assert!(q.since_ms > NOW_MS - 8 * DAY_MS);
    }

    #[test]
    fn the_finding_is_the_same_value_between_ticks() {
        let fake = Arc::new(Fake::new(vec![blue_screen(NOW_MS - DAY_MS)]));
        let (mut watch, time) = watch(fake);
        let first = watch.finding();
        assert_eq!(first, watch.finding(), "stable before the first read");
        settle(&mut watch);
        let done = watch.finding();
        time.fetch_add(60, Ordering::SeqCst);
        for _ in 0..5 {
            watch.step(None);
            assert_eq!(done, watch.finding());
        }
    }

    #[test]
    fn it_checks_again_after_thirty_minutes_and_not_before() {
        let fake = Arc::new(Fake::new(vec![]));
        let (mut watch, time) = watch(fake.clone());
        settle(&mut watch);
        time.fetch_add(29 * 60 + 59, Ordering::SeqCst);
        assert!(!watch.wants_tick());
        watch.step(None);
        assert_eq!(fake.reads.load(Ordering::SeqCst), 1);
        time.fetch_add(1, Ordering::SeqCst);
        assert!(watch.wants_tick());
        settle(&mut watch);
        assert_eq!(fake.reads.load(Ordering::SeqCst), 2);
        assert_eq!(watch.finding().measured_at, NOW + 30 * 60);
    }

    #[test]
    fn a_failed_read_is_tried_again_at_the_next_time_and_recovers() {
        // The same cadence as a good read, so an unreadable log is not hammered.
        let mut fake = Fake::new(vec![]);
        fake.fail = true;
        let (mut watch, time) = watch(Arc::new(fake));
        settle(&mut watch);
        assert_eq!(watch.finding().status, FindingStatus::Unavailable);
        assert!(!watch.wants_tick());
        time.fetch_add(30 * 60, Ordering::SeqCst);
        assert!(watch.wants_tick());
    }

    #[test]
    fn check_now_reads_again_at_once_and_only_one_read_runs_at_a_time() {
        let fake = Arc::new(Fake::new(vec![]));
        let (mut watch, _) = watch(fake.clone());
        settle(&mut watch);
        assert!(!watch.wants_tick());
        watch.request();
        watch.request();
        assert!(watch.wants_tick());
        settle(&mut watch);
        assert_eq!(fake.reads.load(Ordering::SeqCst), 2);
        assert!(!watch.wants_tick());
    }

    #[test]
    fn a_clock_that_went_back_does_not_freeze_the_check() {
        let fake = Arc::new(Fake::new(vec![]));
        let (mut watch, time) = watch(fake);
        settle(&mut watch);
        time.store(NOW - 3600, Ordering::SeqCst);
        assert!(watch.wants_tick());
    }
}
