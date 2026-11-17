//! Resource history recorder (#38 recorder, #39 per-app records and events, #40 saved to disk).
//!
//! # Off unless switched on
//!
//! Recording runs only when the preference [`PREF_KEY`] is `true` in the add-on's own
//! preferences. The default is OFF until the owner decides (#45, Gusseti/toolboks#52). When it
//! is off there is no recorder thread, no reading and no stored history: nothing is sampled,
//! held or written. Switching it off stops recording and drops the in-memory copy of this run;
//! what was already saved stays on disk until it ages out or is deleted (#45).
//!
//! # What is recorded (and what is not)
//!
//! Every step (about [`STEP`]), in memory and saved on this PC only (see [`disk`]; nothing is
//! ever sent anywhere):
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
//! In memory, this run only: bounded rings with the steps of the last hour, per-minute
//! summaries for 24 hours, 5-minute summaries for 7 days (average and peak, so short spikes
//! stay visible), 20 000 events, 4096 exe names. A full store is a few MB whatever the number
//! of processes.
//!
//! On disk, every run: the same three levels with the same windows (1 hour, 24 hours, 7 days),
//! in the add-on's data folder. A full week is at most about 7 MB (budget 20 MB, see [`disk`]).
//!
//! # Cost and safety
//!
//! One thread of its own, one reading at a time (never overlapping), the next one a step after
//! the previous one started or, when a reading ran late, a step after it ended. It never blocks
//! the UI or the live sampler. Reading is read-only (see `source.rs`).
//!
//! # Reading it back
//!
//! The run recording now is read from memory ([`Control::store`]); earlier runs from disk
//! ([`disk::load`], leaving out [`Control::run`]).

// The readers and the page arrive in #41 and #42; until then some accessors are only used by
// the tests.
#![cfg_attr(not(test), allow(dead_code))]

mod apps;
mod core;
pub mod disk;
pub mod query;
pub mod record;
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

/// How long dropping a recorder waits for its thread to write out its buffers (the add-on is
/// quitting or recording was switched off). A reading in progress is normally about 10 ms.
const FINISH_WAIT: Duration = Duration::from_secs(1);

/// Makes the sinks of a run on the recorder thread: (run id, wall clock now).
pub type MakeSinks = Box<dyn FnOnce(u64, i64) -> Vec<Box<dyn store::Sink>> + Send>;

/// A running recorder. Dropping it stops the thread at its next wake-up (at once while it
/// waits; after the current reading if one is running) and waits up to [`FINISH_WAIT`] for it
/// to close its buckets and write out what is buffered.
pub struct Recorder {
    stop: Option<Sender<()>>,
    finished: mpsc::Receiver<()>,
    store: Arc<Mutex<Store>>,
    run: u64,
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop.take();
        let _ = self.finished.recv_timeout(FINISH_WAIT);
    }
}

impl Recorder {
    /// Starts recording the real system at [`STEP`], saving to the history folder.
    pub fn start_system() -> Option<Recorder> {
        Self::spawn_with(
            STEP,
            || source::SystemSource::new().map(|s| Box::new(s) as Box<dyn Source>),
            Box::new(|run, now| {
                let writer = disk::Writer::new(disk::directory(), run, now);
                vec![Box::new(writer) as Box<dyn store::Sink>]
            }),
        )
    }

    /// Starts the thread without saving anything (tests).
    pub(crate) fn spawn<F>(step: Duration, make: F) -> Option<Recorder>
    where
        F: FnOnce() -> Option<Box<dyn Source>> + Send + 'static,
    {
        Self::spawn_with(step, make, Box::new(|_, _| Vec::new()))
    }

    /// Starts the thread. The source and the sinks are made on the thread, so a slow start
    /// never blocks the caller. `None` if the thread cannot start.
    pub(crate) fn spawn_with<F>(step: Duration, make: F, sinks: MakeSinks) -> Option<Recorder>
    where
        F: FnOnce() -> Option<Box<dyn Source>> + Send + 'static,
    {
        let (stop, stopped) = mpsc::channel::<()>();
        let (done, finished) = mpsc::channel::<()>();
        let store = Arc::new(Mutex::new(Store::default()));
        let shared = Arc::clone(&store);
        let run = disk::new_run_id(utc_ms());
        std::thread::Builder::new()
            .name("resource-history".into())
            .spawn(move || {
                // Dropped when the thread ends, which wakes a waiting `Recorder::drop`.
                let _done = done;
                let Some(mut source) = make() else {
                    return;
                };
                let epoch = Instant::now();
                let sinks = sinks(run, utc_ms());
                let mut core = match shared.lock() {
                    Ok(mut s) => Core::new(&mut s, Duration::ZERO, utc_ms(), sinks),
                    Err(_) => return,
                };
                let mut start = epoch + step;
                loop {
                    let wait = start.saturating_duration_since(Instant::now());
                    let go_on =
                        matches!(stopped.recv_timeout(wait), Err(RecvTimeoutError::Timeout));
                    let reading = go_on.then(|| (Instant::now(), source.read()));
                    // Stopped (or the recorder dropped) while waiting or reading.
                    let go_on =
                        go_on && matches!(stopped.try_recv(), Err(mpsc::TryRecvError::Empty));
                    let Ok(mut s) = shared.lock() else {
                        return;
                    };
                    match reading {
                        Some((started, reading)) if go_on => {
                            core.push(&mut s, epoch.elapsed(), utc_ms(), reading);
                            drop(s);
                            start = next_start(started, Instant::now(), step);
                        }
                        _ => {
                            core.finish(&mut s);
                            return;
                        }
                    }
                }
            })
            .ok()?;
        Some(Recorder {
            stop: Some(stop),
            finished,
            store,
            run,
        })
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

    /// The id of the run recording now; its own files on disk are read from [`Self::store`].
    pub fn run(&self) -> Option<u64> {
        self.recorder.as_ref().map(|r| r.run)
    }
}

/// A recorder over a fake system with one app, for tests outside this module.
#[cfg(test)]
pub(crate) fn fake_recorder(step: Duration) -> Option<Recorder> {
    struct One;
    impl Source for One {
        fn read(&mut self) -> Reading {
            Reading {
                totals: store::totals(10.0),
                procs: vec![apps::proc("a.exe", 1.0, 100)],
            }
        }
    }
    Recorder::spawn(step, || Some(Box::new(One) as Box<dyn Source>))
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
    fn switching_off_writes_out_the_run_and_the_next_run_reads_it_back() {
        let dir = disk::tests::test_dir("thread");
        let reads = Arc::new(AtomicUsize::new(0));
        let to = dir.clone();
        let mut control = Control::default();
        control.apply(true, || {
            Recorder::spawn_with(
                Duration::from_millis(10),
                fake(&reads),
                Box::new(move |run, now| {
                    vec![Box::new(disk::Writer::new(to, run, now)) as Box<dyn store::Sink>]
                }),
            )
        });
        let run = control.run().unwrap();
        let store = control.store().unwrap();
        assert!(wait_for(|| store.lock().unwrap().fine().len() >= 5));
        // Switching off waits for the thread to close its buckets and write its buffers.
        control.apply(false, || None);
        let recorded = store.lock().unwrap().fine().len();
        let q = disk::Query {
            from_ms: 0,
            to_ms: i64::MAX / 2,
            levels: disk::LEVELS.to_vec(),
            skip_run: None,
            now_ms: utc_ms(),
        };
        let loaded = disk::load(&dir, &q);
        assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
        assert_eq!(loaded.steps.len(), recorded);
        assert!(loaded.steps.iter().all(|s| s.run == run));
        assert_eq!(
            loaded.names.name(loaded.steps[0].rec.apps[0].app),
            Some("a.exe")
        );
        assert!(
            !loaded.minutes.is_empty(),
            "the open minute was closed and saved"
        );
        assert!(matches!(
            loaded.markers[0].rec.kind,
            record::MarkerKind::Began
        ));
        let _ = std::fs::remove_dir_all(&dir);
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
