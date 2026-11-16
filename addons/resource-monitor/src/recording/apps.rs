//! Per-app records and app events from one reading of the process table.

use std::collections::{BTreeSet, HashMap};

use super::record::{AppEvent, AppId, AppUsage, EventKind, Folded, NameTable, Usage};

/// An app is kept on its own when it used at least this much in the step (#39).
pub const KEEP_CPU_PERCENT: f32 = 0.5;
pub const KEEP_MEM_BYTES: u64 = 50 * 1024 * 1024;
pub const KEEP_GPU_PERCENT: f32 = 1.0;
/// Disk read plus write, bytes per second ("more than").
pub const KEEP_DISK_BPS: f32 = 100.0 * 1024.0;
/// At most this many apps per step; the rest go to "Everything else", so a PC with hundreds of
/// processes costs the same as one with a few.
pub const MAX_APPS_PER_STEP: usize = 24;

/// One process as the source read it. Holds the exe name only: no path, no command line, no
/// window title, no process id.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcReading {
    pub name: String,
    /// Percent of the whole machine.
    pub cpu: f32,
    /// Working set in bytes.
    pub mem: u64,
    /// Percent of the main GPU.
    pub gpu: f32,
    /// Bytes per second; `None` when Windows did not give the counters for this process.
    pub disk_read_bps: Option<f32>,
    pub disk_write_bps: Option<f32>,
}

/// All processes of one exe name, summed.
#[derive(Clone, Copy, Debug)]
struct Merged {
    procs: u32,
    disk_known: bool,
    usage: Usage,
}

/// The apps of one step plus the set of every app present (for events).
pub struct Split {
    pub apps: Vec<AppUsage>,
    pub other: Folded,
    pub present: BTreeSet<AppId>,
}

fn clean(v: f32) -> f32 {
    if v.is_finite() && v > 0.0 {
        v
    } else {
        0.0
    }
}

fn keep(m: &Merged) -> bool {
    let u = &m.usage;
    u.cpu >= KEEP_CPU_PERCENT
        || u.mem >= KEEP_MEM_BYTES
        || u.gpu >= KEEP_GPU_PERCENT
        || u.disk_read + u.disk_write > KEEP_DISK_BPS
}

/// The largest share of the process total this app has in any resource; ranks kept apps.
fn score(u: &Usage, total: &Usage) -> f64 {
    let share = |v: f64, t: f64| if t > 0.0 { v / t } else { 0.0 };
    let v = u.values();
    let t = total.values();
    let disk = share(v[3] + v[4], t[3] + t[4]);
    share(v[0], t[0])
        .max(share(v[1], t[1]))
        .max(share(v[2], t[2]))
        .max(disk)
}

/// Merges processes by exe name, keeps the apps above the thresholds (at most
/// [`MAX_APPS_PER_STEP`], biggest share first) and folds the rest into "Everything else".
/// Kept apps plus `other` always add up to the sum over all processes.
pub fn split(procs: &[ProcReading], names: &mut NameTable) -> Split {
    let mut by_app: HashMap<AppId, Merged> = HashMap::new();
    let mut other = Folded::default();
    let mut total = Usage::default();
    let mut no_slot = 0u32;
    for p in procs {
        let u = Usage {
            cpu: clean(p.cpu),
            mem: p.mem,
            gpu: clean(p.gpu),
            disk_read: clean(p.disk_read_bps.unwrap_or(0.0)),
            disk_write: clean(p.disk_write_bps.unwrap_or(0.0)),
        };
        total.add(&u);
        let known = p.disk_read_bps.is_some() || p.disk_write_bps.is_some();
        match names.intern(&p.name) {
            Some(id) => {
                let m = by_app.entry(id).or_insert(Merged {
                    procs: 0,
                    disk_known: false,
                    usage: Usage::default(),
                });
                m.procs += 1;
                m.disk_known |= known;
                m.usage.add(&u);
            }
            None => {
                // No name-table slot: counted, but never on its own.
                no_slot += 1;
                other.procs += 1;
                other.usage.add(&u);
            }
        }
    }
    if no_slot > 0 {
        other.apps = other.apps.saturating_add(1);
    }
    let present: BTreeSet<AppId> = by_app.keys().copied().collect();
    let mut kept: Vec<(AppId, Merged)> = Vec::new();
    for (id, m) in by_app {
        if keep(&m) {
            kept.push((id, m));
        } else {
            fold(&mut other, &m);
        }
    }
    kept.sort_by(|a, b| {
        score(&b.1.usage, &total)
            .total_cmp(&score(&a.1.usage, &total))
            .then(a.0.cmp(&b.0))
    });
    for (_, m) in kept.drain(MAX_APPS_PER_STEP.min(kept.len())..) {
        fold(&mut other, &m);
    }
    // A fresh, exact-size list: collecting in place would keep the capacity of every
    // candidate in each stored record.
    let mut apps = Vec::with_capacity(kept.len());
    apps.extend(kept.into_iter().map(|(app, m)| AppUsage {
        app,
        procs: m.procs.min(u16::MAX as u32) as u16,
        disk_known: m.disk_known,
        usage: m.usage,
    }));
    Split {
        apps,
        other,
        present,
    }
}

fn fold(other: &mut Folded, m: &Merged) {
    other.apps = other.apps.saturating_add(1);
    other.procs += m.procs;
    other.usage.add(&m.usage);
}

/// Which apps are running, to turn consecutive readings into start and exit events.
#[derive(Debug, Default)]
pub struct Presence {
    running: BTreeSet<AppId>,
    primed: bool,
}

impl Presence {
    /// Compares `present` with the previous reading. The first reading gives `AlreadyRunning`
    /// for every app. `during_gap` marks events whose real moment is unknown (inside a gap).
    pub fn update(
        &mut self,
        present: BTreeSet<AppId>,
        seq: u64,
        utc_ms: i64,
        during_gap: bool,
    ) -> Vec<AppEvent> {
        let event = |app, kind| AppEvent {
            seq,
            utc_ms,
            app,
            kind,
            during_gap,
        };
        let events = if self.primed {
            let started = present
                .difference(&self.running)
                .map(|a| event(*a, EventKind::Started));
            let exited = self
                .running
                .difference(&present)
                .map(|a| event(*a, EventKind::Exited));
            exited.chain(started).collect()
        } else {
            self.primed = true;
            present
                .iter()
                .map(|a| event(*a, EventKind::AlreadyRunning))
                .collect()
        };
        self.running = present;
        events
    }
}

#[cfg(test)]
pub(crate) fn proc(name: &str, cpu: f32, mem_mb: u64) -> ProcReading {
    ProcReading {
        name: name.into(),
        cpu,
        mem: mem_mb * 1024 * 1024,
        gpu: 0.0,
        disk_read_bps: Some(0.0),
        disk_write_bps: Some(0.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sum(split: &Split) -> Usage {
        let mut u = split.other.usage;
        for a in &split.apps {
            u.add(&a.usage);
        }
        u
    }

    fn sum_procs(procs: &[ProcReading]) -> Usage {
        let mut u = Usage::default();
        for p in procs {
            u.add(&Usage {
                cpu: p.cpu,
                mem: p.mem,
                gpu: p.gpu,
                disk_read: p.disk_read_bps.unwrap_or(0.0),
                disk_write: p.disk_write_bps.unwrap_or(0.0),
            });
        }
        u
    }

    fn close(a: &Usage, b: &Usage) -> bool {
        let near = |x: f32, y: f32| (x - y).abs() <= 1e-3 * x.abs().max(y.abs()).max(1.0);
        near(a.cpu, b.cpu)
            && a.mem == b.mem
            && near(a.gpu, b.gpu)
            && near(a.disk_read, b.disk_read)
            && near(a.disk_write, b.disk_write)
    }

    #[test]
    fn processes_with_one_exe_name_are_merged() {
        let mut names = NameTable::default();
        let procs = vec![
            proc("chrome.exe", 3.0, 200),
            proc("Chrome.exe", 2.0, 100),
            proc("chrome.exe", 0.0, 10),
            proc("tiny.exe", 0.0, 1),
        ];
        let s = split(&procs, &mut names);
        assert_eq!(s.apps.len(), 1);
        let chrome = s.apps[0];
        assert_eq!(names.name(chrome.app), Some("chrome.exe"));
        assert_eq!(chrome.procs, 3);
        assert_eq!(chrome.usage.cpu, 5.0);
        assert_eq!(chrome.usage.mem, 310 * 1024 * 1024);
        assert_eq!(s.other.apps, 1);
        assert_eq!(s.other.procs, 1);
        assert_eq!(s.present.len(), 2);
    }

    #[test]
    fn thresholds_each_keep_an_app_on_their_own() {
        let mut names = NameTable::default();
        let mut gpu = proc("game.exe", 0.0, 1);
        gpu.gpu = KEEP_GPU_PERCENT;
        let mut disk = proc("copy.exe", 0.0, 1);
        disk.disk_write_bps = Some(KEEP_DISK_BPS + 1.0);
        let mut at_disk_limit = proc("idle-disk.exe", 0.0, 1);
        at_disk_limit.disk_read_bps = Some(KEEP_DISK_BPS);
        let procs = vec![
            proc("cpu.exe", KEEP_CPU_PERCENT, 1),
            proc("mem.exe", 0.0, KEEP_MEM_BYTES / 1024 / 1024),
            proc("below.exe", KEEP_CPU_PERCENT - 0.01, 49),
            gpu,
            disk,
            at_disk_limit,
        ];
        let s = split(&procs, &mut names);
        let kept: BTreeSet<&str> = s.apps.iter().map(|a| names.name(a.app).unwrap()).collect();
        assert_eq!(
            kept,
            BTreeSet::from(["cpu.exe", "mem.exe", "game.exe", "copy.exe"])
        );
        assert_eq!(s.other.apps, 2);
    }

    #[test]
    fn apps_plus_everything_else_equal_the_process_total() {
        let mut names = NameTable::default();
        // 400 processes over 120 exe names, a mix above and below the thresholds.
        let procs: Vec<ProcReading> = (0..400)
            .map(|i| {
                let mut p = proc(
                    &format!("app{}.exe", i % 120),
                    (i % 7) as f32 * 0.3,
                    (i % 13) * 9,
                );
                p.gpu = (i % 11) as f32 * 0.2;
                p.disk_read_bps = if i % 17 == 0 {
                    None
                } else {
                    Some((i % 5) as f32 * 40_000.0)
                };
                p
            })
            .collect();
        let s = split(&procs, &mut names);
        assert!(close(&sum(&s), &sum_procs(&procs)));
        assert_eq!(s.apps.len(), MAX_APPS_PER_STEP, "capped");
        assert_eq!(
            s.apps.capacity(),
            MAX_APPS_PER_STEP,
            "no spare room kept per record"
        );
        let procs_counted: u32 = s.apps.iter().map(|a| a.procs as u32).sum::<u32>() + s.other.procs;
        assert_eq!(procs_counted, 400);
        assert_eq!(s.apps.len() + s.other.apps as usize, 120);
    }

    #[test]
    fn the_cap_keeps_the_biggest_users() {
        let mut names = NameTable::default();
        let mut procs: Vec<ProcReading> = (0..40)
            .map(|i| proc(&format!("small{i}.exe"), 1.0, 1))
            .collect();
        procs.push(proc("big.exe", 50.0, 1));
        procs.push(proc("hog.exe", 0.0, 8000));
        let s = split(&procs, &mut names);
        let first: Vec<&str> = s.apps[..2]
            .iter()
            .map(|a| names.name(a.app).unwrap())
            .collect();
        assert!(first.contains(&"big.exe") && first.contains(&"hog.exe"));
        assert_eq!(s.apps.len(), MAX_APPS_PER_STEP);
    }

    #[test]
    fn unknown_disk_is_not_zero() {
        let mut names = NameTable::default();
        let mut protected = proc("protected.exe", 1.0, 1);
        protected.disk_read_bps = None;
        protected.disk_write_bps = None;
        let s = split(&[protected, proc("open.exe", 1.0, 1)], &mut names);
        let find = |n: &str| {
            s.apps
                .iter()
                .find(|a| names.name(a.app) == Some(n))
                .unwrap()
        };
        assert!(!find("protected.exe").disk_known);
        assert!(find("open.exe").disk_known);
    }

    #[test]
    fn processes_without_a_name_slot_go_to_everything_else() {
        let mut names = NameTable::default();
        for i in 0..super::super::record::MAX_NAMES {
            names.intern(&format!("n{i}.exe"));
        }
        let s = split(&[proc("new.exe", 90.0, 900)], &mut names);
        assert!(s.apps.is_empty());
        assert!(s.present.is_empty(), "no events for an app without an id");
        assert_eq!(s.other.procs, 1);
        assert_eq!(s.other.usage.cpu, 90.0);
    }

    #[test]
    fn bad_numbers_count_as_zero() {
        let mut names = NameTable::default();
        let mut p = proc("odd.exe", f32::NAN, 1);
        p.gpu = -4.0;
        p.disk_read_bps = Some(f32::INFINITY);
        let s = split(&[p], &mut names);
        assert_eq!(
            s.other.usage,
            Usage {
                mem: 1024 * 1024,
                ..Usage::default()
            }
        );
    }

    #[test]
    fn start_and_exit_events_follow_the_exe_name() {
        let mut names = NameTable::default();
        let mut presence = Presence::default();
        fn run(
            names: &mut NameTable,
            presence: &mut Presence,
            procs: &[ProcReading],
            seq: u64,
        ) -> Vec<AppEvent> {
            let s = split(procs, names);
            presence.update(s.present, seq, seq as i64 * 5000, false)
        }
        macro_rules! step {
            ($procs:expr, $seq:expr) => {
                run(&mut names, &mut presence, $procs, $seq)
            };
        }
        let first = step!(&[proc("a.exe", 0.0, 1), proc("b.exe", 0.0, 1)], 1);
        assert_eq!(first.len(), 2);
        assert!(first.iter().all(|e| e.kind == EventKind::AlreadyRunning));
        // A second process of a.exe starts: not an app start.
        assert!(step!(
            &[
                proc("a.exe", 0.0, 1),
                proc("a.exe", 0.0, 1),
                proc("b.exe", 0.0, 1)
            ],
            2
        )
        .is_empty());
        // c.exe starts, b.exe exits, one a.exe process ends (a.exe still runs).
        let e = step!(&[proc("a.exe", 0.0, 1), proc("C.EXE", 0.0, 1)], 3);
        let kinds: Vec<(EventKind, i64)> = e.iter().map(|e| (e.kind, e.utc_ms)).collect();
        assert_eq!(
            kinds,
            vec![(EventKind::Exited, 15000), (EventKind::Started, 15000)]
        );
        assert_eq!(e[0].app, names.id("b.exe").unwrap());
        assert_eq!(e[1].app, names.id("c.exe").unwrap());
        let e = step!(&[], 4);
        assert_eq!(e.len(), 2);
        assert!(e.iter().all(|e| e.kind == EventKind::Exited));
    }
}
