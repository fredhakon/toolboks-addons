//! Resource history recorder (History part 1: #38 recorder, #39 per-app records and events).
//!
//! # Off unless switched on
//!
//! Recording runs only when the preference [`PREF_KEY`] is `true` in the add-on's own
//! preferences. The default is OFF until the owner decides (#45, Gusseti/toolboks#52). When it
//! is off there is no recorder thread, no reading and no stored history: nothing is sampled,
//! held or written. Switching it off drops everything recorded in this run.
//!
//! Part 1 records only while the add-on process runs. Resource Monitor is not yet marked as a
//! background add-on (`host-services.json`), so the host still ends it when the page is left.
//! The `background` flag comes with the switch (#45), once the memory a suspended add-on keeps
//! is fixed (#91). Until then recording in the tray does not happen.
//!
//! # What is recorded (and what is not)
//!
//! Every step (about [`STEP`]), in memory only. Nothing is written to disk yet (#40):
//! - System totals: processor use, memory in use and installed, main-GPU load, video memory,
//!   disk read and write, network down and up (all counted adapters together). GPU temperature
//!   and power have fields but are not read yet (see `source.rs`: the driver library costs
//!   about 19 MB).
//! - Per app, where an app is every process with the same exe name merged (`chrome.exe`):
//!   the exe name (file name only, at most 128 bytes), the number of processes, processor
//!   percent, working set, GPU percent, disk read and write per second (or "unknown" when
//!   Windows gives no counters for it). Only apps above small thresholds are kept on their own,
//!   at most 24 per step; the rest are summed into "Everything else".
//! - App events: an exe name appearing (started), disappearing (exited) or already running
//!   when recording began, with the time. Processes that start and end between two steps are
//!   not seen.
//! - Markers: recording began, a gap (sleep, hibernate, a stalled reading), the clock changed.
//!
//! Never recorded: process ids, paths, command lines, window titles, user names, file names,
//! network addresses or per-app network use (not measurable without admin rights, #37).
//!
//! # How much is kept
//!
//! Bounded rings: the steps of the last hour, per-minute summaries for 24 hours, 5-minute
//! summaries for 7 days (average and peak, so short spikes stay visible), 20 000 events,
//! 4096 exe names. A full store is a few MB whatever the number of processes.
//!
//! # Cost and safety
//!
//! One thread of its own, one reading at a time (never overlapping), the next one a step after
//! the previous one started or, when a reading ran late, a step after it ended. It never blocks
//! the UI or the live sampler. Reading is read-only (see `source.rs`).
//!
//! # Part 2
//!
//! A file writer plugs in as a [`store::Sink`]; a reader reads [`store::Store`] through
//! [`Control::store`].

// Readers and the file writer arrive in part 2 (#40, #41); until then some accessors are only
// used by the tests.
#![cfg_attr(not(test), allow(dead_code))]

mod apps;
mod core;
mod record;
mod source;
pub mod store;

use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use self::core::Core;
pub use self::core::Reading;
use self::store::Store;

/// Recording step while recording (#37: about 0.1 % of the whole CPU at 5 s on a 650-process
/// PC). The thread pauses at least this long between the start of two readings.
pub const STEP: Duration = Duration::from_secs(5);

/// The preference that switches recording on. Default off.
pub const PREF_KEY: &str = "history_recording";

/// True when the preferences switch recording on. A missing or non-boolean value is off.
pub fn wanted(prefs: &std::collections::BTreeMap<String, serde_json::Value>) -> bool {
    prefs
        .get(PREF_KEY)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// Something that reads the system once per step. Fakes implement it in the tests.
pub trait Source {
    fn read(&mut self) -> Reading;
}

fn utc_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => -(e.duration().as_millis() as i64),
    }
}

/// When the next reading may start: a step after this one started, or a step after now if
/// this one ran past that (no burst of readings to catch up, no overlap).
fn next_start(started: Instant, now: Instant, step: Duration) -> Instant {
    let next = started + step;
    if next <= now {
        now + step
    } else {
        next
    }
}

/// A running recorder. Dropping it stops the thread at its next wake-up (at once while it
/// waits; after the current reading if one is running).
pub struct Recorder {
    _stop: Sender<()>,
    store: Arc<Mutex<Store>>,
}

impl Recorder {
    /// Starts recording the real system at [`STEP`].
    pub fn start_system() -> Option<Recorder> {
        Self::spawn(STEP, || {
            source::SystemSource::new().map(|s| Box::new(s) as Box<dyn Source>)
        })
    }

    /// Starts the thread. The source is made on the thread, so a slow start never blocks the
    /// caller. `None` if the thread cannot start.
    pub(crate) fn spawn<F>(step: Duration, make: F) -> Option<Recorder>
    where
        F: FnOnce() -> Option<Box<dyn Source>> + Send + 'static,
    {
        let (stop, stopped) = mpsc::channel::<()>();
        let store = Arc::new(Mutex::new(Store::default()));
        let shared = Arc::clone(&store);
        std::thread::Builder::new()
            .name("resource-history".into())
            .spawn(move || {
                let Some(mut source) = make() else {
                    return;
                };
                let epoch = Instant::now();
                let mut core = match shared.lock() {
                    Ok(mut s) => Core::new(&mut s, Duration::ZERO, utc_ms(), Vec::new()),
                    Err(_) => return,
                };
                let mut start = epoch + step;
                loop {
                    match stopped.recv_timeout(start.saturating_duration_since(Instant::now())) {
                        Err(RecvTimeoutError::Timeout) => {}
                        // Stopped, or the recorder was dropped.
                        _ => return,
                    }
                    let started = Instant::now();
                    let reading = source.read();
                    if !matches!(stopped.try_recv(), Err(mpsc::TryRecvError::Empty)) {
                        return;
                    }
                    let (mono, utc) = (epoch.elapsed(), utc_ms());
                    match shared.lock() {
                        Ok(mut s) => {
                            core.push(&mut s, mono, utc, reading);
                        }
                        Err(_) => return,
                    }
                    start = next_start(started, Instant::now(), step);
                }
            })
            .ok()?;
        Some(Recorder { _stop: stop, store })
    }
}

/// The on/off switch the add-on holds. It outlives page switches and `suspend`.
#[derive(Default)]
pub struct Control {
    recorder: Option<Recorder>,
}

impl Control {
    /// Starts the recorder when `on` and none runs; stops it (dropping its history) when off.
    pub fn apply(&mut self, on: bool, start: impl FnOnce() -> Option<Recorder>) {
        if !on {
            self.recorder = None;
        } else if self.recorder.is_none() {
            self.recorder = start();
        }
    }

    pub fn is_on(&self) -> bool {
        self.recorder.is_some()
    }

    /// The history recorded in this run, for the readers of part 2.
    pub fn store(&self) -> Option<Arc<Mutex<Store>>> {
        self.recorder.as_ref().map(|r| Arc::clone(&r.store))
    }
}

#[cfg(test)]
mod tests {
    use super::apps::proc;
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fake {
        reads: Arc<AtomicUsize>,
    }

    impl Source for Fake {
        fn read(&mut self) -> Reading {
            self.reads.fetch_add(1, Ordering::SeqCst);
            Reading {
                totals: store::totals(10.0),
                procs: vec![proc("a.exe", 1.0, 100)],
            }
        }
    }

    fn fake(reads: &Arc<AtomicUsize>) -> impl FnOnce() -> Option<Box<dyn Source>> + Send + 'static {
        let reads = Arc::clone(reads);
        move || Some(Box::new(Fake { reads }) as Box<dyn Source>)
    }

    fn wait_for(mut done: impl FnMut() -> bool) -> bool {
        let until = Instant::now() + Duration::from_secs(10);
        while Instant::now() < until {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn off_by_default_and_for_anything_but_true() {
        let mut prefs = std::collections::BTreeMap::new();
        assert!(!wanted(&prefs));
        prefs.insert(PREF_KEY.into(), serde_json::json!("true"));
        assert!(!wanted(&prefs));
        prefs.insert(PREF_KEY.into(), serde_json::json!(1));
        assert!(!wanted(&prefs));
        prefs.insert(PREF_KEY.into(), serde_json::json!(true));
        assert!(wanted(&prefs));
    }

    #[test]
    fn off_means_no_thread_no_reading_no_store() {
        let mut control = Control::default();
        let mut started = false;
        control.apply(false, || {
            started = true;
            None
        });
        assert!(!started, "off never starts a recorder");
        assert!(!control.is_on());
        assert!(control.store().is_none());
    }

    #[test]
    fn the_thread_records_and_stops_when_switched_off() {
        let reads = Arc::new(AtomicUsize::new(0));
        let mut control = Control::default();
        control.apply(true, || {
            Recorder::spawn(Duration::from_millis(10), fake(&reads))
        });
        let store = control.store().unwrap();
        assert!(wait_for(|| store.lock().unwrap().fine().len() >= 3));
        // Switching on again does not start a second recorder.
        control.apply(true, || panic!("a second recorder"));
        control.apply(false, || None);
        assert!(control.store().is_none());
        // The thread ends: the store is released and no more readings are taken.
        assert!(wait_for(|| Arc::strong_count(&store) == 1));
        let after = reads.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(reads.load(Ordering::SeqCst), after);
    }

    #[test]
    fn a_source_that_cannot_start_records_nothing() {
        let mut control = Control::default();
        control.apply(true, || Recorder::spawn(Duration::from_millis(1), || None));
        let store = control.store().unwrap();
        assert!(wait_for(|| Arc::strong_count(&store) == 2));
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(store.lock().unwrap().fine().len(), 0);
    }

    #[test]
    fn readings_never_overlap_or_burst_after_a_slow_one() {
        let step = Duration::from_millis(100);
        let t = Instant::now();
        assert_eq!(next_start(t, t + Duration::from_millis(30), step), t + step);
        let late = t + Duration::from_millis(450);
        assert_eq!(next_start(t, late, step), late + step);
    }

    /// Measures the recorder's own cost per step on this PC. Not part of the gate:
    /// `cargo test --release -p resource-monitor recording::tests::cost -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn cost() {
        // Bookkeeping only, with a synthetic 650-process table over 180 exe names.
        let reading = Reading {
            totals: store::totals(30.0),
            procs: (0..650)
                .map(|i| {
                    proc(
                        &format!("app{}.exe", i % 180),
                        (i % 9) as f32 * 0.2,
                        (i % 23) * 5,
                    )
                })
                .collect(),
        };
        let mut s = Store::default();
        let mut core = Core::new(&mut s, Duration::ZERO, 0, Vec::new());
        let n = 2000u32;
        let t = Instant::now();
        for i in 1..=n {
            core.push(&mut s, STEP * i, i as i64 * 5000, reading.clone());
        }
        let per = t.elapsed() / n;
        println!("bookkeeping per step (650 procs, incl. clone): {per:?}");
        // The real reading, measured as thread CPU time.
        let mut src = source::SystemSource::new().expect("source");
        let cpu0 = thread_cpu();
        let wall0 = Instant::now();
        let reads = 10;
        let mut procs = 0;
        for _ in 0..reads {
            std::thread::sleep(STEP);
            procs = src.read().procs.len();
        }
        let cpu = (thread_cpu() - cpu0) / reads;
        println!(
            "real reading: {cpu:?} CPU per step, {procs} processes, {:?} wall for {reads} steps",
            wall0.elapsed()
        );
        println!("store after {n} steps: {} KB", s.approx_bytes() / 1024);
    }

    fn thread_cpu() -> Duration {
        use windows::Win32::Foundation::FILETIME;
        use windows::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
        let (mut a, mut b, mut k, mut u) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        // SAFETY: the pseudo handle of the current thread is always valid; all four out
        // pointers point at local FILETIMEs.
        unsafe { GetThreadTimes(GetCurrentThread(), &mut a, &mut b, &mut k, &mut u) }.unwrap();
        let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
        Duration::from_nanos((t(k) + t(u)) * 100)
    }
}
