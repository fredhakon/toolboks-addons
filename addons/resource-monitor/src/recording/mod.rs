//! Resource history recorder (#38 recorder, #39 per-app records and events, #40 saved to disk).
//!
//! # On by default, one switch to turn it off
//!
//! Recording runs unless the preference [`PREF_KEY`] is `false` in the add-on's own
//! preferences (owner decision on #45 and Gusseti/toolboks#52: on by default, with a one-time
//! note on the History page; see `history_settings.rs`). When it is off there is no recorder
//! thread, no reading and no stored history: nothing is sampled, held or written. Switching it
//! off stops recording and drops the in-memory copy of this run; what was already saved stays
//! on disk until it is deleted ("Delete history") or, once recording is on again, ages out.
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
pub mod markers;
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

/// The preference that switches recording off when `false`. Default on.
pub const PREF_KEY: &str = "history_recording";

/// False only when the preferences switch recording off; missing (a first start) means on.
/// A value that is not a boolean is treated as off, the safe reading of a damaged setting.
pub fn wanted(prefs: &std::collections::BTreeMap<String, serde_json::Value>) -> bool {
    match prefs.get(PREF_KEY) {
        None => true,
        Some(v) => v.as_bool().unwrap_or(false),
    }
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
    /// Asks the thread to stop without waiting for it. The receiver reports
    /// `Disconnected` once the thread has written out its buffers and ended.
    fn stop(mut self) -> mpsc::Receiver<()> {
        self.stop.take();
        let (_, empty) = mpsc::channel();
        std::mem::replace(&mut self.finished, empty)
    }

    /// Starts recording the real system at [`STEP`], saving to the history folder. Unit tests
    /// use a fake instead, so they never write a real history folder.
    #[cfg_attr(test, allow(dead_code))]
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
///
/// Switching off never blocks the UI: the recorder is told to stop and finishes its current
/// reading on its own thread. A recorder is only started once the previous thread has ended, so
/// switching off and straight back on never runs two readings or two writers at once; the
/// start waits for [`Self::poll`] (called every frame, and as background work while the add-on
/// is hidden, see [`Self::pending`]).
#[derive(Default)]
pub struct Control {
    recorder: Option<Recorder>,
    /// The thread of a recorder that was switched off and may still be finishing.
    stopping: Option<mpsc::Receiver<()>>,
    /// Recording is wanted (the switch).
    want: bool,
    /// "Delete history" was asked for: delete the files in this folder once no recorder runs.
    delete: Option<std::path::PathBuf>,
    /// The result of the last delete: files deleted, or what went wrong.
    deleted: Option<Result<usize, String>>,
}

impl Drop for Control {
    fn drop(&mut self) {
        // Quitting while a switched-off recorder finishes: let it write out, as a drop does.
        if let Some(rx) = self.stopping.take() {
            let _ = rx.recv_timeout(FINISH_WAIT);
        }
    }
}

impl Control {
    /// Sets the switch and starts or stops the recorder as far as possible now: off stops at
    /// once, on starts once no earlier recorder is still finishing.
    pub fn apply(&mut self, on: bool, start: impl FnOnce() -> Option<Recorder>) {
        self.want = on;
        if !on {
            self.stop_recorder();
        }
        self.poll(start);
    }

    /// Finishes what is waiting: a delete, then a start, once the old thread has ended.
    pub fn poll(&mut self, start: impl FnOnce() -> Option<Recorder>) {
        if !self.stopped() {
            return;
        }
        if let Some(dir) = self.delete.take() {
            self.deleted = Some(disk::delete_all(&dir));
        }
        if self.want && self.recorder.is_none() {
            self.recorder = start();
        }
    }

    /// Stops recording (if on), deletes every saved history file in `dir` once the thread has
    /// written out, and starts a fresh recorder if the switch is on. Finished by [`Self::poll`].
    pub fn delete_history(
        &mut self,
        dir: std::path::PathBuf,
        start: impl FnOnce() -> Option<Recorder>,
    ) {
        self.stop_recorder();
        self.delete = Some(dir);
        self.deleted = None;
        self.poll(start);
    }

    /// The result of the last finished delete.
    pub fn deleted(&self) -> Option<&Result<usize, String>> {
        self.deleted.as_ref()
    }

    /// True while a delete or a start waits for an old recorder thread to end.
    pub fn pending(&self) -> bool {
        self.stopping.is_some() && (self.delete.is_some() || self.want)
    }

    /// True while a delete has not run yet.
    pub fn deleting(&self) -> bool {
        self.delete.is_some()
    }

    /// No switched-off recorder thread is still running.
    pub fn stopped(&mut self) -> bool {
        let done = self
            .stopping
            .as_ref()
            .is_none_or(|rx| !matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        if done {
            self.stopping = None;
        }
        done
    }

    fn stop_recorder(&mut self) {
        if let Some(r) = self.recorder.take() {
            // At most one thread is ever stopping: a new one only starts after `stopped`.
            self.stopping = Some(r.stop());
        }
    }

    pub fn is_on(&self) -> bool {
        self.recorder.is_some()
    }

    /// The switch: recording is wanted (it may still be starting).
    pub fn wanted(&self) -> bool {
        self.want
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
    fn on_by_default_off_only_when_switched_off() {
        let mut prefs = std::collections::BTreeMap::new();
        assert!(wanted(&prefs), "a first start records");
        prefs.insert(PREF_KEY.into(), serde_json::json!(false));
        assert!(!wanted(&prefs));
        // A damaged value reads as off, never as on.
        prefs.insert(PREF_KEY.into(), serde_json::json!("true"));
        assert!(!wanted(&prefs));
        prefs.insert(PREF_KEY.into(), serde_json::json!(1));
        assert!(!wanted(&prefs));
        prefs.insert(PREF_KEY.into(), serde_json::json!(true));
        assert!(wanted(&prefs));
    }

    /// A source whose reading blocks while the test holds `gate`.
    struct Slow {
        gate: Arc<Mutex<()>>,
        reading: Arc<AtomicUsize>,
        most: Arc<AtomicUsize>,
    }

    impl Source for Slow {
        fn read(&mut self) -> Reading {
            let now = self.reading.fetch_add(1, Ordering::SeqCst) + 1;
            self.most.fetch_max(now, Ordering::SeqCst);
            drop(self.gate.lock().unwrap());
            self.reading.fetch_sub(1, Ordering::SeqCst);
            Reading {
                totals: store::totals(10.0),
                procs: vec![proc("a.exe", 1.0, 100)],
            }
        }
    }

    type Counters = (Arc<Mutex<()>>, Arc<AtomicUsize>, Arc<AtomicUsize>);

    fn slow(c: &Counters) -> impl FnOnce() -> Option<Recorder> {
        let (gate, reading, most) = (Arc::clone(&c.0), Arc::clone(&c.1), Arc::clone(&c.2));
        move || {
            Recorder::spawn(Duration::from_millis(5), move || {
                Some(Box::new(Slow {
                    gate,
                    reading,
                    most,
                }) as Box<dyn Source>)
            })
        }
    }

    #[test]
    fn off_and_straight_back_on_waits_for_the_old_reading() {
        let c: Counters = Default::default();
        let mut control = Control::default();
        let held = c.0.lock().unwrap();
        control.apply(true, slow(&c));
        // The first reading is now blocked inside the source.
        assert!(wait_for(|| c.1.load(Ordering::SeqCst) == 1));
        let started = Instant::now();
        control.apply(false, || None);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "off does not block"
        );
        let mut second = false;
        control.apply(true, || {
            second = true;
            None
        });
        assert!(!second, "no new recorder while the old one is reading");
        assert!(control.pending() && !control.is_on() && control.wanted());
        drop(held);
        assert!(wait_for(|| {
            control.poll(slow(&c));
            control.is_on()
        }));
        assert!(!control.pending());
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(c.2.load(Ordering::SeqCst), 1, "never two readings at once");
    }

    #[test]
    fn delete_waits_for_the_recorder_then_deletes_and_restarts() {
        let dir = disk::tests::test_dir("delete-all");
        let reads = Arc::new(AtomicUsize::new(0));
        let start = |to: std::path::PathBuf, reads: &Arc<AtomicUsize>| {
            let source = fake(reads);
            move || {
                Recorder::spawn_with(
                    Duration::from_millis(10),
                    source,
                    Box::new(move |run, now| {
                        vec![Box::new(disk::Writer::new(to, run, now)) as Box<dyn store::Sink>]
                    }),
                )
            }
        };
        let mut control = Control::default();
        control.apply(true, start(dir.clone(), &reads));
        let first = control.run().unwrap();
        let store = control.store().unwrap();
        assert!(wait_for(|| store.lock().unwrap().fine().len() >= 3));
        drop(store);
        control.delete_history(dir.clone(), start(dir.clone(), &reads));
        assert!(wait_for(|| {
            control.poll(start(dir.clone(), &reads));
            control.deleted().is_some() && control.is_on()
        }));
        assert!(matches!(control.deleted(), Some(Ok(n)) if *n > 0));
        assert_ne!(control.run(), Some(first), "a fresh run after the delete");
        assert!(
            disk::list(&dir).iter().all(|f| f.3 != first),
            "the old run is gone"
        );
        control.apply(false, || None);
        assert!(wait_for(|| control.stopped()));
        let _ = std::fs::remove_dir_all(&dir);
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
        // Switching off lets the thread close its buckets and write its buffers.
        control.apply(false, || None);
        assert!(wait_for(|| control.stopped()));
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
