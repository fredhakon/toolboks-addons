//! One process-table refresh for the live views and the recorder (#90).
//!
//! While Resource Monitor's page is open, its live process worker (`sampler.rs`) refreshes the
//! process table about every 2 s. With recording on, the recorder would refresh the same table
//! again every 5 s. Instead, the live worker adds every refresh it makes to this feed, and the
//! recorder takes the sum at its step: the time-weighted processor use, the bytes read and
//! written, and the latest memory of each process over that step. The recorder refreshes the
//! table itself only when the feed does not cover its step (the page is closed or hidden).
//!
//! The feed collects only while a recorder runs ([`Active`]), and holds one entry per process,
//! so it stays small.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::apps::ProcReading;

/// The feed covers a step when it holds at least this share of the step's time ...
const MIN_COVER: f64 = 0.6;
/// ... and its newest refresh is at most this old.
const MAX_AGE: Duration = Duration::from_secs(4);

#[derive(Clone, Debug, Default)]
struct ProcSum {
    name: String,
    /// Percent of the machine times seconds.
    cpu: f64,
    mem: u64,
    read: u64,
    written: u64,
    known: bool,
}

#[derive(Debug, Default)]
struct State {
    secs: f64,
    newest: Option<Instant>,
    procs: HashMap<u32, ProcSum>,
}

static FEED: Mutex<Option<State>> = Mutex::new(None);
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// Held by a running recorder: while one exists, the live worker feeds its refreshes.
pub struct Active(());

impl Active {
    pub fn new() -> Active {
        ACTIVE.fetch_add(1, Ordering::SeqCst);
        Active(())
    }
}

impl Drop for Active {
    fn drop(&mut self) {
        if ACTIVE.fetch_sub(1, Ordering::SeqCst) == 1 {
            if let Ok(mut f) = FEED.lock() {
                *f = None;
            }
        }
    }
}

/// True while a recorder wants the live refreshes.
pub fn wanted() -> bool {
    ACTIVE.load(Ordering::SeqCst) > 0
}

/// One process from a live refresh: exe name, percent of the machine, working set, bytes read
/// and written since the previous refresh, and whether Windows gave I/O counters at all.
pub struct Seen<'a> {
    pub pid: u32,
    pub name: Cow<'a, str>,
    pub cpu: f64,
    pub mem: u64,
    pub read: u64,
    pub written: u64,
    pub known: bool,
}

/// Adds one live refresh that covered `secs` seconds. Does nothing unless a recorder runs.
pub fn add<'a>(secs: f64, procs: impl IntoIterator<Item = Seen<'a>>) {
    if !wanted() || !(secs.is_finite() && secs > 0.0) {
        return;
    }
    let Ok(mut f) = FEED.lock() else { return };
    let s = f.get_or_insert_with(State::default);
    s.secs += secs;
    s.newest = Some(Instant::now());
    for p in procs {
        let e = s.procs.entry(p.pid).or_default();
        if e.name != p.name {
            e.name = p.name.into_owned();
        }
        e.cpu += p.cpu * secs;
        e.mem = p.mem;
        e.read += p.read;
        e.written += p.written;
        e.known |= p.known;
    }
}

/// Forgets what was collected (a recorder starts its own baseline).
pub fn clear() {
    if let Ok(mut f) = FEED.lock() {
        *f = None;
    }
}

/// The recorder's step of `step_secs`: the processes (with their ids, for the GPU figures) as
/// averages over the collected time when the feed covers the step, `None` otherwise. Either way
/// the feed starts over.
pub fn take(step_secs: f64) -> Option<Vec<(u32, ProcReading)>> {
    let s = FEED.lock().ok()?.take()?;
    let fresh = s.newest.is_some_and(|t| t.elapsed() <= MAX_AGE);
    if !fresh || s.secs < MIN_COVER * step_secs || s.secs <= 0.0 {
        return None;
    }
    let secs = s.secs;
    Some(
        s.procs
            .into_iter()
            .map(|(pid, p)| {
                let rate = |b: u64| p.known.then_some((b as f64 / secs) as f32);
                let reading = ProcReading {
                    name: p.name,
                    cpu: (p.cpu / secs) as f32,
                    mem: p.mem,
                    gpu: 0.0,
                    disk_read_bps: rate(p.read),
                    disk_write_bps: rate(p.written),
                };
                (pid, reading)
            })
            .collect(),
    )
}

#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(pid: u32, name: &'static str, cpu: f64, written: u64) -> Seen<'static> {
        Seen {
            pid,
            name: Cow::Borrowed(name),
            cpu,
            mem: 100,
            read: 0,
            written,
            known: true,
        }
    }

    #[test]
    fn collects_only_while_a_recorder_runs_and_averages_over_the_step() {
        let _l = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        add(2.0, [seen(1, "a.exe", 50.0, 0)]);
        assert!(take(5.0).is_none(), "no recorder: nothing collected");
        let active = Active::new();
        // Two live refreshes of 2 s and 3 s: 50 % then 0 %, 1 MB then 4 MB written.
        add(2.0, [seen(1, "a.exe", 50.0, 1 << 20)]);
        add(
            3.0,
            [seen(1, "a.exe", 0.0, 4 << 20), seen(2, "b.exe", 10.0, 0)],
        );
        let mut procs: Vec<ProcReading> = take(5.0)
            .expect("the feed covers the step")
            .into_iter()
            .map(|(_, p)| p)
            .collect();
        procs.sort_by(|x, y| x.name.cmp(&y.name));
        assert_eq!(procs[0].name, "a.exe");
        assert!((procs[0].cpu - 20.0).abs() < 1e-4, "{}", procs[0].cpu);
        assert!((procs[0].disk_write_bps.unwrap() - 1048576.0).abs() < 1.0);
        assert!(
            (procs[1].cpu - 6.0).abs() < 1e-4,
            "b.exe was seen for 3 of 5 s"
        );
        assert!(take(5.0).is_none(), "taking starts over");
        // Too little of the step covered: the recorder reads the table itself.
        add(2.0, [seen(1, "a.exe", 50.0, 0)]);
        assert!(take(5.0).is_none());
        drop(active);
        assert!(!wanted());
    }
}
