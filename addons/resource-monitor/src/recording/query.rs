//! Reading a period back (#41): the curve of the system totals, which apps used what, and the
//! curve of one app, from the run recording now (memory) and earlier runs (disk).
//!
//! # Levels of detail
//!
//! Each moment is read from one level only, so nothing is counted twice:
//! - every step for the last hour,
//! - per-minute buckets from 24 hours ago up to the last hour,
//! - 5-minute buckets before that.
//!
//! A period longer than 24 hours reads the 5-minute level only (plus the steps of the last
//! few minutes, which no closed 5-minute bucket holds yet).
//!
//! Records are weighted by the seconds they actually cover (a stalled reading covers more than
//! 5 s; a bucket can be partly recorded), and a record that straddles the edge of the period or
//! of a level is counted for the part inside. Nothing recorded is never read as zero: every
//! result says how many seconds of the period were recorded.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use super::disk::{self, Level};
use super::record::{
    AppEvent, AppId, EventKind, Marker, NameTable, StepRecord, TOTAL_FIGURES, USAGE_FIGURES,
};
use super::store::{Bucket, Stat, Store, Tier};

const MINUTE_MS: i64 = 60_000;
const FIVE_MS: i64 = 300_000;
const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 24 * HOUR_MS;
/// At most this many points in a curve.
pub const MAX_BINS: usize = 4000;

/// Indexes into the per-app figures ([`super::record::Usage::values`]).
pub const CPU: usize = 0;
pub const MEM: usize = 1;
pub const GPU: usize = 2;
pub const DISK_READ: usize = 3;
pub const DISK_WRITE: usize = 4;

/// The resources apps are ranked by. Network is recorded for the system only (#37).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resource {
    Cpu,
    Memory,
    Gpu,
    Disk,
}

/// What to read.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub from_ms: i64,
    pub to_ms: i64,
    /// Points in the curves (the width of the graph), 1 to [`MAX_BINS`].
    pub bins: usize,
    /// Also draw this app (exe name, any case).
    pub app: Option<String>,
}

/// Where to read from.
#[derive(Clone)]
pub struct Sources {
    pub dir: PathBuf,
    /// The run recording now and its in-memory history; its own files on disk are skipped.
    pub live: Option<(u64, Arc<Mutex<Store>>)>,
}

/// One point of a curve: the time-weighted average and the peak over `[start_ms, end_ms)`.
#[derive(Clone, Debug, PartialEq)]
pub struct Bin {
    pub start_ms: i64,
    pub end_ms: i64,
    /// Seconds of this bin that were recorded. 0 = a gap (not running, asleep, switched off).
    pub covered_secs: f64,
    /// The coarsest level that went into this bin (`None` in a gap).
    pub detail: Option<Level>,
    /// System totals, as [`super::record::SystemTotals::values`]; `None` when not recorded.
    pub totals: [Option<Stat>; TOTAL_FIGURES],
    /// The requested app, as [`super::record::Usage::values`]; `None` in a gap or without app.
    pub app: Option<[Stat; USAGE_FIGURES]>,
}

impl Bin {
    pub fn is_gap(&self) -> bool {
        self.covered_secs <= 0.0
    }
}

/// One figure of one app (or of "Everything else") over the period.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Figure {
    /// Average over the recorded seconds (seconds in which the app was not seen count as 0).
    pub avg: f64,
    /// Largest value in any one record. For buckets this is the bucket's peak.
    pub peak: f64,
    /// Use over the period: the figure times the seconds (percent-seconds, byte-seconds...).
    pub total: f64,
    /// Share of all processes together, 0 to 1.
    pub share: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppRow {
    pub name: String,
    /// Seconds the app was recorded on its own (above the recording thresholds).
    pub active_secs: f64,
    pub peak_procs: u16,
    /// False when Windows gave no disk counters for it at any time: disk is unknown, not 0.
    pub disk_known: bool,
    pub figures: [Figure; USAGE_FIGURES],
}

/// Which apps used what over a period.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Breakdown {
    pub from_ms: i64,
    pub to_ms: i64,
    /// Seconds of the period that were recorded ("6 h 20 min of 8 h recorded").
    pub recorded_secs: f64,
    /// Every app recorded on its own in the period, in no particular order (see [`Self::ranked`]).
    pub apps: Vec<AppRow>,
    /// Everything else: apps below the thresholds or over the per-record caps.
    pub other: [Figure; USAGE_FIGURES],
    /// System totals over the recorded seconds; `None` when never recorded.
    pub system: [Option<Stat>; TOTAL_FIGURES],
}

impl Breakdown {
    pub fn period_secs(&self) -> f64 {
        (self.to_ms - self.from_ms).max(0) as f64 / 1000.0
    }

    /// The apps ranked for a resource, biggest first. Memory by average, then peak; the others
    /// by total use over the period. Apps that did not use the resource are left out.
    pub fn ranked(&self, r: Resource) -> Vec<&AppRow> {
        let key = |a: &AppRow| -> (f64, f64) {
            let f = &a.figures;
            match r {
                Resource::Cpu => (f[CPU].total, f[CPU].peak),
                Resource::Memory => (f[MEM].avg, f[MEM].peak),
                Resource::Gpu => (f[GPU].total, f[GPU].peak),
                Resource::Disk => (
                    f[DISK_READ].total + f[DISK_WRITE].total,
                    f[DISK_READ].peak.max(f[DISK_WRITE].peak),
                ),
            }
        };
        let mut rows: Vec<&AppRow> = self
            .apps
            .iter()
            .filter(|a| {
                let (k, p) = key(a);
                k > 0.0 || p > 0.0
            })
            .collect();
        rows.sort_by(|a, b| {
            let (ka, kb) = (key(a), key(b));
            kb.0.total_cmp(&ka.0)
                .then(kb.1.total_cmp(&ka.1))
                .then(a.name.cmp(&b.name))
        });
        rows
    }

    pub fn app(&self, name: &str) -> Option<&AppRow> {
        self.apps.iter().find(|a| a.name.eq_ignore_ascii_case(name))
    }
}

/// An app event with its name.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub utc_ms: i64,
    pub name: String,
    pub kind: EventKind,
    pub during_gap: bool,
}

/// Everything the page needs for one period.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Answer {
    pub request: Option<Request>,
    pub curve: Vec<Bin>,
    pub breakdown: Breakdown,
    /// App events in the period, in time order.
    pub events: Vec<Event>,
    /// Markers (recording began, gaps, clock changes) in the period, in time order.
    pub markers: Vec<Marker>,
    /// Files that could not be read in full (one line each). The rest of the answer stands.
    pub problems: Vec<String>,
}

// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct PApp {
    app: AppId,
    active: f64,
    procs: u16,
    disk_known: bool,
    figures: [Stat; USAGE_FIGURES],
}

/// A record over an interval, in one shared name table.
#[derive(Clone, Debug)]
struct Piece {
    /// The interval the record covers, clipped to its level's segment.
    a: i64,
    b: i64,
    /// The record's own length before clipping (ms), to weigh the part kept.
    len: f64,
    covered: f64,
    level: Level,
    totals: [Option<Stat>; TOTAL_FIGURES],
    apps: Vec<PApp>,
    other: [Stat; USAGE_FIGURES],
}

impl Piece {
    /// The share of this record inside `[x, y)`.
    fn part(&self, x: i64, y: i64) -> f64 {
        let o = (self.b.min(y) - self.a.max(x)).max(0) as f64;
        if self.len > 0.0 {
            o / self.len
        } else {
            0.0
        }
    }
}

fn step_piece(r: &StepRecord, map: impl Fn(AppId) -> Option<AppId>) -> Piece {
    let secs = r.secs.max(0.0) as f64;
    let b = r.utc_ms;
    let a = b - (secs * 1000.0).round() as i64;
    let mut other = r.other.usage.values().map(|v| Stat {
        avg: v as f32,
        peak: v as f32,
    });
    let mut apps = Vec::with_capacity(r.apps.len());
    for x in &r.apps {
        let figures = x.usage.values().map(|v| Stat {
            avg: v as f32,
            peak: v as f32,
        });
        match map(x.app) {
            Some(app) => apps.push(PApp {
                app,
                active: secs,
                procs: x.procs,
                disk_known: x.disk_known,
                figures,
            }),
            None => add_stats(&mut other, &figures),
        }
    }
    Piece {
        a,
        b,
        len: (b - a) as f64,
        covered: secs,
        level: Level::Fine,
        totals: r.totals.values().map(|v| {
            v.map(|v| Stat {
                avg: v as f32,
                peak: v as f32,
            })
        }),
        apps,
        other,
    }
}

fn bucket_piece(bk: &Bucket, tier: Tier, map: impl Fn(AppId) -> Option<AppId>) -> Piece {
    let a = bk.start_utc_ms;
    let b = a + tier.period_ms();
    let mut other = bk.other;
    let mut apps = Vec::with_capacity(bk.apps.len());
    for x in &bk.apps {
        match map(x.app) {
            Some(app) => apps.push(PApp {
                app,
                active: x.active_secs as f64,
                procs: x.peak_procs,
                disk_known: x.disk_known,
                figures: x.figures,
            }),
            None => add_stats(&mut other, &x.figures),
        }
    }
    Piece {
        a,
        b,
        len: (b - a) as f64,
        covered: bk.covered_secs.max(0.0) as f64,
        level: match tier {
            Tier::Minute => Level::Minute,
            Tier::FiveMinutes => Level::Day,
        },
        totals: bk.totals,
        apps,
        other,
    }
}

fn add_stats(to: &mut [Stat; USAGE_FIGURES], from: &[Stat; USAGE_FIGURES]) {
    for (o, s) in to.iter_mut().zip(from) {
        o.avg += s.avg;
        o.peak += s.peak;
    }
}

/// Where each level is read: `[lo, hi)`.
#[derive(Clone, Copy, Debug)]
struct Segments {
    fine_from: i64,
    minute_from: i64,
}

impl Segments {
    fn for_period(from: i64, to: i64, now: i64) -> Segments {
        if to - from > DAY_MS {
            // Coarse only; the steps fill the open 5-minute bucket.
            let cut = now.div_euclid(FIVE_MS) * FIVE_MS;
            Segments {
                fine_from: cut,
                minute_from: cut,
            }
        } else {
            Segments {
                fine_from: now - HOUR_MS,
                minute_from: now - DAY_MS,
            }
        }
    }

    fn range(&self, level: Level) -> (i64, i64) {
        match level {
            Level::Fine => (self.fine_from, i64::MAX),
            Level::Minute => (self.minute_from, self.fine_from),
            Level::Day => (i64::MIN, self.minute_from),
        }
    }

    fn wants(&self, level: Level, from: i64, to: i64) -> bool {
        let (lo, hi) = self.range(level);
        lo.max(from) < hi.min(to)
    }
}

/// Clips a piece to its level's segment and the period; `None` if nothing is left.
fn clip(mut p: Piece, seg: &Segments, from: i64, to: i64) -> Option<Piece> {
    let (lo, hi) = seg.range(p.level);
    p.a = p.a.max(lo).max(from);
    p.b = p.b.min(hi).min(to);
    (p.a < p.b).then_some(p)
}

struct Collected {
    names: NameTable,
    pieces: Vec<Piece>,
    events: Vec<Event>,
    markers: Vec<Marker>,
    problems: Vec<String>,
}

fn collect(src: &Sources, from: i64, to: i64, now: i64) -> Collected {
    let seg = Segments::for_period(from, to, now);
    let mut names = NameTable::default();
    let mut pieces = Vec::new();
    let mut events = Vec::new();
    let mut markers = Vec::new();
    let in_range = |t: i64| (from..to).contains(&t);
    // Earlier runs, from disk. Day files hold the events and markers, so they are always read.
    let mut levels = vec![Level::Day];
    for l in [Level::Fine, Level::Minute] {
        if seg.wants(l, from, to) {
            levels.push(l);
        }
    }
    let q = disk::Query {
        // A bucket that started before `from` can still reach into the period.
        from_ms: from - FIVE_MS,
        // A step is stamped at its end: one that ends after `to` can still cover part of it.
        to_ms: to + super::core::GAP_AFTER.as_millis() as i64 + 1,
        levels,
        skip_run: src.live.as_ref().map(|(run, _)| *run),
        now_ms: now,
    };
    let loaded = disk::load(&src.dir, &q);
    let mut map_disk: HashMap<AppId, AppId> = HashMap::new();
    for id in 0..loaded.names.len() as AppId {
        if let Some(g) = loaded.names.name(id).and_then(|n| names.intern(n)) {
            map_disk.insert(id, g);
        }
    }
    let md = |id: AppId| map_disk.get(&id).copied();
    let mut add = |p: Piece| {
        if let Some(p) = clip(p, &seg, from, to) {
            pieces.push(p);
        }
    };
    if seg.wants(Level::Fine, from, to) {
        for s in &loaded.steps {
            add(step_piece(&s.rec, md));
        }
    }
    if seg.wants(Level::Minute, from, to) {
        for s in &loaded.minutes {
            add(bucket_piece(&s.rec, Tier::Minute, md));
        }
    }
    if seg.wants(Level::Day, from, to) {
        for s in &loaded.five_minutes {
            add(bucket_piece(&s.rec, Tier::FiveMinutes, md));
        }
    }
    let event = |e: &AppEvent, n: &NameTable| Event {
        utc_ms: e.utc_ms,
        name: n.name(e.app).unwrap_or("?").to_string(),
        kind: e.kind,
        during_gap: e.during_gap,
    };
    for e in loaded.events.iter().filter(|e| in_range(e.rec.utc_ms)) {
        events.push(event(&e.rec, &loaded.names));
    }
    markers.extend(
        loaded
            .markers
            .iter()
            .filter(|m| in_range(m.rec.utc_ms))
            .map(|m| m.rec),
    );
    // The run recording now, from memory (it also holds what is not on disk yet).
    if let Some((_, store)) = &src.live {
        if let Ok(store) = store.lock() {
            let mut map_live: HashMap<AppId, AppId> = HashMap::new();
            for id in 0..store.names.len() as AppId {
                if let Some(g) = store.names.name(id).and_then(|n| names.intern(n)) {
                    map_live.insert(id, g);
                }
            }
            let ml = |id: AppId| map_live.get(&id).copied();
            let mut add = |p: Piece| {
                if let Some(p) = clip(p, &seg, from, to) {
                    pieces.push(p);
                }
            };
            if seg.wants(Level::Fine, from, to) {
                for r in store.fine().iter().filter(|r| r.utc_ms > from) {
                    add(step_piece(r, ml));
                }
            }
            for (tier, level) in [
                (Tier::Minute, Level::Minute),
                (Tier::FiveMinutes, Level::Day),
            ] {
                if seg.wants(level, from, to) {
                    for b in store
                        .tier(tier)
                        .iter()
                        .filter(|b| b.start_utc_ms + tier.period_ms() > from && b.start_utc_ms < to)
                    {
                        add(bucket_piece(b, tier, ml));
                    }
                }
            }
            for e in store.events().iter().filter(|e| in_range(e.utc_ms)) {
                events.push(event(e, &store.names));
            }
            markers.extend(
                store
                    .markers()
                    .iter()
                    .filter(|m| in_range(m.utc_ms))
                    .copied(),
            );
        }
    }
    events.sort_by_key(|e| e.utc_ms);
    markers.sort_by_key(|m| m.utc_ms);
    Collected {
        names,
        pieces,
        events,
        markers,
        problems: loaded.problems,
    }
}

#[derive(Clone, Copy, Default)]
struct Acc {
    sum: f64,
    secs: f64,
    peak: f64,
    seen: bool,
}

impl Acc {
    fn add(&mut self, s: Stat, secs: f64) {
        self.sum += s.avg as f64 * secs;
        self.secs += secs;
        self.peak = if self.seen {
            self.peak.max(s.peak as f64)
        } else {
            s.peak as f64
        };
        self.seen = true;
    }

    fn stat(&self) -> Option<Stat> {
        (self.seen && self.secs > 0.0).then(|| Stat {
            avg: (self.sum / self.secs) as f32,
            peak: self.peak as f32,
        })
    }
}

#[derive(Clone, Default)]
struct AppAcc {
    active: f64,
    procs: u16,
    disk_known: bool,
    figures: [Acc; USAGE_FIGURES],
}

fn breakdown(c: &Collected, from: i64, to: i64) -> Breakdown {
    let mut recorded = 0.0;
    let mut system = [Acc::default(); TOTAL_FIGURES];
    let mut apps: HashMap<AppId, AppAcc> = HashMap::new();
    let mut other = [Acc::default(); USAGE_FIGURES];
    for p in &c.pieces {
        let f = p.part(from, to);
        if f <= 0.0 {
            continue;
        }
        let secs = p.covered * f;
        recorded += secs;
        for (acc, t) in system.iter_mut().zip(&p.totals) {
            if let Some(t) = t {
                acc.add(*t, secs);
            }
        }
        for x in &p.apps {
            let a = apps.entry(x.app).or_default();
            a.active += x.active * f;
            a.procs = a.procs.max(x.procs);
            a.disk_known |= x.disk_known;
            for (acc, s) in a.figures.iter_mut().zip(x.figures) {
                acc.add(s, secs);
            }
        }
        for (acc, s) in other.iter_mut().zip(p.other) {
            acc.add(s, secs);
        }
    }
    // Use = average times seconds. Averages divide by all recorded seconds: an app that was
    // not seen in a record counts as 0 there.
    let mut process_total = [0.0f64; USAGE_FIGURES];
    for a in apps.values() {
        for (t, acc) in process_total.iter_mut().zip(&a.figures) {
            *t += acc.sum;
        }
    }
    for (t, acc) in process_total.iter_mut().zip(&other) {
        *t += acc.sum;
    }
    let figure = |acc: &Acc, i: usize| Figure {
        avg: if recorded > 0.0 {
            acc.sum / recorded
        } else {
            0.0
        },
        peak: if acc.seen { acc.peak } else { 0.0 },
        total: acc.sum,
        share: if process_total[i] > 0.0 {
            acc.sum / process_total[i]
        } else {
            0.0
        },
    };
    let rows = apps
        .into_iter()
        .map(|(id, a)| AppRow {
            name: c.names.name(id).unwrap_or("?").to_string(),
            active_secs: a.active,
            peak_procs: a.procs,
            disk_known: a.disk_known,
            figures: std::array::from_fn(|i| figure(&a.figures[i], i)),
        })
        .collect();
    Breakdown {
        from_ms: from,
        to_ms: to,
        recorded_secs: recorded,
        apps: rows,
        other: std::array::from_fn(|i| figure(&other[i], i)),
        system: system.map(|a| a.stat()),
    }
}

fn level_rank(l: Level) -> u8 {
    match l {
        Level::Fine => 0,
        Level::Minute => 1,
        Level::Day => 2,
    }
}

fn curve(c: &Collected, from: i64, to: i64, bins: usize, app: Option<AppId>) -> Vec<Bin> {
    let bins = bins.clamp(1, MAX_BINS);
    let span = (to - from).max(1);
    let edge = |i: usize| from + (span as i128 * i as i128 / bins as i128) as i64;
    struct B {
        covered: f64,
        detail: Option<Level>,
        totals: [Acc; TOTAL_FIGURES],
        app: [Acc; USAGE_FIGURES],
    }
    let mut out: Vec<B> = (0..bins)
        .map(|_| B {
            covered: 0.0,
            detail: None,
            totals: [Acc::default(); TOTAL_FIGURES],
            app: [Acc::default(); USAGE_FIGURES],
        })
        .collect();
    let index = |t: i64| -> usize {
        (((t - from).clamp(0, span - 1) as i128 * bins as i128 / span as i128) as usize)
            .min(bins - 1)
    };
    for p in &c.pieces {
        let (first, last) = (index(p.a), index(p.b - 1));
        let figures = app.map(|id| {
            p.apps
                .iter()
                .find(|x| x.app == id)
                .map(|x| x.figures)
                .unwrap_or_default()
        });
        for (i, bin) in out.iter_mut().enumerate().take(last + 1).skip(first) {
            let f = p.part(edge(i), edge(i + 1));
            if f <= 0.0 {
                continue;
            }
            let secs = p.covered * f;
            bin.covered += secs;
            if bin
                .detail
                .is_none_or(|d| level_rank(p.level) > level_rank(d))
            {
                bin.detail = Some(p.level);
            }
            for (acc, t) in bin.totals.iter_mut().zip(&p.totals) {
                if let Some(t) = t {
                    acc.add(*t, secs);
                }
            }
            if let Some(figures) = figures {
                for (acc, s) in bin.app.iter_mut().zip(figures) {
                    acc.add(s, secs);
                }
            }
        }
    }
    out.into_iter()
        .enumerate()
        .map(|(i, b)| Bin {
            start_ms: edge(i),
            end_ms: edge(i + 1),
            covered_secs: b.covered,
            detail: b.detail,
            totals: b.totals.map(|a| a.stat()),
            app: (app.is_some() && b.covered > 0.0)
                .then(|| b.app.map(|a| a.stat().unwrap_or_default())),
        })
        .collect()
}

/// Answers a request. Reads files, so it runs on the query worker, never on the UI thread.
pub fn answer(src: &Sources, req: &Request, now_ms: i64) -> Answer {
    let (from, to) = (req.from_ms, req.to_ms.max(req.from_ms));
    let c = collect(src, from, to, now_ms);
    let app = req.app.as_deref().and_then(|n| {
        (0..c.names.len() as AppId)
            .find(|id| c.names.name(*id).is_some_and(|x| x.eq_ignore_ascii_case(n)))
    });
    let curve = curve(&c, from, to, req.bins, app);
    let breakdown = breakdown(&c, from, to);
    Answer {
        request: Some(req.clone()),
        curve,
        breakdown,
        events: c.events,
        markers: c.markers,
        problems: c.problems,
    }
}

/// Answers requests on a thread of its own. Only the newest waiting request is answered.
pub struct Worker {
    requests: Sender<(Request, Sources)>,
    answers: Receiver<Answer>,
}

impl Worker {
    pub fn start() -> Option<Worker> {
        let (requests, waiting) = mpsc::channel::<(Request, Sources)>();
        let (send, answers) = mpsc::channel();
        std::thread::Builder::new()
            .name("resource-history-query".into())
            .spawn(move || {
                while let Ok(mut job) = waiting.recv() {
                    while let Ok(newer) = waiting.try_recv() {
                        job = newer;
                    }
                    let (req, src) = job;
                    if send.send(answer(&src, &req, super::utc_ms())).is_err() {
                        return;
                    }
                }
            })
            .ok()?;
        Some(Worker { requests, answers })
    }

    pub fn ask(&self, req: Request, src: Sources) {
        let _ = self.requests.send((req, src));
    }

    /// The newest answer that arrived, if any.
    pub fn take(&self) -> Option<Answer> {
        self.answers.try_iter().last()
    }
}

#[cfg(test)]
mod tests {
    use super::super::apps::proc;
    use super::super::core::{Core, Reading};
    use super::super::disk::tests::test_dir;
    use super::super::disk::Writer;
    use super::super::record::MarkerKind;
    use super::super::store::{totals, Sink};
    use super::*;
    use std::time::Duration;

    /// 2026-11-17 12:00 UTC: "now" in the fixture.
    const NOW: i64 = 20_774 * DAY_MS + 12 * HOUR_MS;
    const MIN: i64 = MINUTE_MS;

    fn reading(cpu: f32, apps: &[(&str, f32, u64, f32)]) -> Reading {
        Reading {
            totals: totals(cpu),
            procs: apps
                .iter()
                .map(|&(n, c, mem, disk)| {
                    let mut p = proc(n, c, mem);
                    p.disk_write_bps = Some(disk);
                    p
                })
                .collect(),
        }
    }

    /// Records `[from, to)` every 5 s with `load(t)` as the reading, through `Core`, into
    /// `store` and (when given) a file writer, as the recorder thread does.
    fn record(
        store: &mut Store,
        sinks: Vec<Box<dyn Sink>>,
        from: i64,
        to: i64,
        load: impl Fn(i64) -> Reading,
    ) {
        let mut core = Core::new(store, Duration::ZERO, from, sinks);
        let mut t = from + 5000;
        while t <= to {
            let mono = Duration::from_millis((t - from) as u64);
            core.push(store, mono, t, load(t));
            t += 5000;
        }
        core.finish(store);
    }

    /// The fixture: run 1 (on disk) from 3 h to 2 h ago with a game and a browser, Toolboks
    /// closed from 2 h to 90 min ago (a gap), then run 2, recording now (in memory and on
    /// disk), with the browser and a build that runs from 30 to 10 min ago.
    fn fixture(dir: &std::path::Path) -> Arc<Mutex<Store>> {
        let mut past = Store::default();
        record(
            &mut past,
            vec![Box::new(Writer::new(dir.to_owned(), 1, NOW - 3 * HOUR_MS))],
            NOW - 3 * HOUR_MS,
            NOW - 2 * HOUR_MS,
            |_| {
                reading(
                    70.0,
                    &[
                        ("game.exe", 50.0, 4000, 0.0),
                        ("chrome.exe", 10.0, 2000, 0.0),
                    ],
                )
            },
        );
        let mut live = Store::default();
        record(
            &mut live,
            vec![Box::new(Writer::new(dir.to_owned(), 2, NOW - 90 * MIN))],
            NOW - 90 * MIN,
            NOW,
            |t| {
                let mut apps = vec![
                    ("chrome.exe", 5.0, 2500, 0.0),
                    ("backup.exe", 1.0, 100, 50e6),
                ];
                if (NOW - 30 * MIN..NOW - 10 * MIN).contains(&(t - 5000)) {
                    apps.push(("build.exe", 60.0, 1000, 0.0));
                }
                reading(90.0, &apps)
            },
        );
        Arc::new(Mutex::new(live))
    }

    fn ask(src: &Sources, from: i64, to: i64, app: Option<&str>) -> Answer {
        answer(
            src,
            &Request {
                from_ms: from,
                to_ms: to,
                bins: 60,
                app: app.map(str::to_string),
            },
            NOW,
        )
    }

    fn names(rows: Vec<&AppRow>) -> Vec<&str> {
        rows.into_iter().map(|r| r.name.as_str()).collect()
    }

    fn close(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() <= tol
    }

    #[test]
    fn a_period_across_a_gap_ranks_and_says_how_much_was_recorded() {
        let dir = test_dir("query-gap");
        let live = fixture(&dir);
        let src = Sources {
            dir: dir.clone(),
            live: Some((2, live)),
        };
        // 2 h 30 min ago to 1 h 30 min ago: 30 min of run 1, then the gap.
        let a = ask(&src, NOW - 150 * MIN, NOW - 90 * MIN, None);
        assert!(a.problems.is_empty(), "{:?}", a.problems);
        let b = &a.breakdown;
        assert!(close(b.recorded_secs, 1800.0, 1.0), "{}", b.recorded_secs);
        assert_eq!(b.period_secs(), 3600.0);
        assert_eq!(
            names(b.ranked(Resource::Cpu)),
            vec!["game.exe", "chrome.exe"]
        );
        let game = b.app("game.exe").unwrap();
        assert!(close(game.figures[CPU].avg, 50.0, 0.01));
        assert!(close(game.figures[CPU].share, 50.0 / 60.0, 0.001));
        assert!(close(game.active_secs, 1800.0, 1.0));
        assert_eq!(
            names(b.ranked(Resource::Memory)),
            vec!["game.exe", "chrome.exe"]
        );
        assert!(close(b.system[0].unwrap().avg as f64, 70.0, 0.01));
        // The curve: the first half recorded (per-minute detail: over an hour ago), the
        // second half a gap, never zero.
        assert!(a.curve[..30]
            .iter()
            .all(|p| !p.is_gap() && p.detail == Some(Level::Minute)));
        assert!(a.curve[30..]
            .iter()
            .all(|p| p.is_gap() && p.totals[0].is_none()));
        let cpu = a.curve[10].totals[0].unwrap();
        assert!(close(cpu.avg as f64, 70.0, 0.01));
        // The gap marker of run 2's start is not in this period; run 1's events are.
        assert!(a.events.iter().all(|e| e.utc_ms < NOW - 120 * MIN));
    }

    #[test]
    fn a_period_across_two_levels_counts_every_moment_once() {
        let dir = test_dir("query-levels");
        let live = fixture(&dir);
        // 80 to 20 min ago: per-minute buckets before the last hour, steps after it.
        let (from, to) = (NOW - 80 * MIN, NOW - 20 * MIN);
        let from_memory = Sources {
            dir: dir.clone(),
            live: Some((2, live)),
        };
        // The same run read from disk only (as the next run will read it).
        let from_disk = Sources {
            dir: dir.clone(),
            live: None,
        };
        for src in [&from_memory, &from_disk] {
            let a = ask(src, from, to, Some("BUILD.exe"));
            let b = &a.breakdown;
            assert!(close(b.recorded_secs, 3600.0, 1.0), "{}", b.recorded_secs);
            assert_eq!(
                names(b.ranked(Resource::Cpu)),
                vec!["build.exe", "chrome.exe", "backup.exe"]
            );
            assert_eq!(names(b.ranked(Resource::Disk))[0], "backup.exe");
            assert_eq!(names(b.ranked(Resource::Memory))[0], "chrome.exe");
            let chrome = b.app("chrome.exe").unwrap();
            assert!(close(chrome.figures[CPU].avg, 5.0, 0.01));
            assert!(close(chrome.active_secs, 3600.0, 1.0));
            let build = b.app("build.exe").unwrap();
            // 10 of the 60 minutes at 60 %.
            assert!(
                close(build.active_secs, 600.0, 5.0),
                "{}",
                build.active_secs
            );
            assert!(close(build.figures[CPU].avg, 10.0, 0.1));
            assert_eq!(build.figures[CPU].peak, 60.0);
            let backup = b.app("backup.exe").unwrap();
            assert!(close(backup.figures[DISK_WRITE].avg, 50e6, 1e3));
            // The table adds up: apps plus everything else is 100 %.
            let shares: f64 =
                b.apps.iter().map(|a| a.figures[CPU].share).sum::<f64>() + b.other[CPU].share;
            assert!(close(shares, 1.0, 1e-6));
            // The curve: minute detail before the last hour, steps after; build's curve.
            assert_eq!(a.curve[0].detail, Some(Level::Minute));
            assert_eq!(a.curve[59].detail, Some(Level::Fine));
            assert!(a.curve.iter().all(|p| !p.is_gap()));
            let build_at = |i: usize| a.curve[i].app.unwrap()[CPU].avg;
            assert_eq!(build_at(5), 0.0, "50 min ago: not running");
            assert!(
                close(build_at(55) as f64, 60.0, 0.01),
                "25 min ago: running"
            );
        }
        // Started and exited events for the build, with names.
        let a = ask(&from_memory, from, to, None);
        let kinds: Vec<_> = a
            .events
            .iter()
            .filter(|e| e.name == "build.exe")
            .map(|e| e.kind)
            .collect();
        assert_eq!(kinds, vec![EventKind::Started]);
    }

    #[test]
    fn a_week_reads_the_coarse_level_and_marks_the_gap() {
        let dir = test_dir("query-week");
        let live = fixture(&dir);
        let src = Sources {
            dir: dir.clone(),
            live: Some((2, live)),
        };
        let a = ask(&src, NOW - 7 * DAY_MS, NOW, None);
        let b = &a.breakdown;
        // 1 h of run 1 and 90 min of run 2 (the last open 5-minute bucket from the steps).
        assert!(close(b.recorded_secs, 9000.0, 5.0), "{}", b.recorded_secs);
        assert_eq!(names(b.ranked(Resource::Cpu))[0], "game.exe");
        assert!(b.ranked(Resource::Gpu).is_empty(), "no app used the GPU");
        assert!(a
            .curve
            .iter()
            .filter(|p| !p.is_gap())
            .all(|p| p.detail == Some(Level::Day) || p.end_ms > NOW - FIVE_MS));
        assert!(a.markers.iter().any(|m| m.kind == MarkerKind::Began));
    }

    #[test]
    fn nothing_recorded_is_not_zero() {
        let dir = test_dir("query-empty");
        let src = Sources {
            dir: dir.clone(),
            live: None,
        };
        let a = ask(&src, NOW - HOUR_MS, NOW, Some("x.exe"));
        assert_eq!(a.breakdown.recorded_secs, 0.0);
        assert!(a.breakdown.apps.is_empty());
        assert!(a.curve.iter().all(|p| p.is_gap() && p.app.is_none()));
        assert!(a.breakdown.system.iter().all(Option::is_none));
    }

    /// Prints the breakdown of a recorded period, to compare with Windows' own counters (#43,
    /// #46). `RM_REPORT_DIR` (a history folder), `RM_REPORT_FROM` and `RM_REPORT_TO` (UTC ms).
    /// `cargo test --release -p resource-monitor --lib report_from_env -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn report_from_env() {
        let dir = std::path::PathBuf::from(std::env::var("RM_REPORT_DIR").expect("RM_REPORT_DIR"));
        let num = |k: &str| -> i64 { std::env::var(k).ok().and_then(|v| v.parse().ok()).expect(k) };
        let (from, to) = (num("RM_REPORT_FROM"), num("RM_REPORT_TO"));
        let src = Sources { dir, live: None };
        let req = Request {
            from_ms: from,
            to_ms: to,
            bins: 10,
            app: None,
        };
        let a = answer(&src, &req, super::super::utc_ms());
        let b = &a.breakdown;
        println!(
            "period {} s, recorded {:.0} s, problems {:?}",
            (to - from) / 1000,
            b.recorded_secs,
            a.problems
        );
        for (r, name, i, scale, unit) in [
            (Resource::Cpu, "CPU", CPU, 1.0, "%"),
            (Resource::Memory, "Memory", MEM, 1.0 / 1048576.0, "MB"),
            (Resource::Gpu, "GPU", GPU, 1.0, "%"),
            (
                Resource::Disk,
                "Disk write",
                DISK_WRITE,
                1.0 / 1048576.0,
                "MB/s",
            ),
        ] {
            println!("{name}:");
            for row in b.ranked(r).into_iter().take(5) {
                let f = row.figures[i];
                println!(
                    "  {:<28} avg {:>9.2} {unit}  peak {:>9.2} {unit}  share {:>5.1} %  active {:>5.0} s",
                    row.name,
                    f.avg * scale,
                    f.peak * scale,
                    f.share * 100.0,
                    row.active_secs
                );
            }
        }
    }

    #[test]
    fn the_worker_answers_off_the_calling_thread() {
        let dir = test_dir("query-worker");
        let w = Worker::start().unwrap();
        w.ask(
            Request {
                from_ms: 0,
                to_ms: 1000,
                bins: 10,
                app: None,
            },
            Sources {
                dir: dir.clone(),
                live: None,
            },
        );
        let until = std::time::Instant::now() + Duration::from_secs(10);
        let mut got = None;
        while got.is_none() && std::time::Instant::now() < until {
            got = w.take();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(got.unwrap().curve.len(), 10);
    }
}
