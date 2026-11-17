//! Bounded in-memory history: the step records of the last hour, per-minute buckets for 24
//! hours and 5-minute buckets for 7 days, plus app events and markers. Every list is a ring
//! with a fixed capacity, so memory stays bounded whatever the clock or the PC does.
//!
//! Part 2 plugs in through [`Sink`] (a file writer gets every record as it is made) and reads
//! through the accessors on [`Store`].

use std::collections::{HashMap, VecDeque};

use super::record::{
    AppEvent, AppId, Marker, NameTable, StepRecord, Usage, TOTAL_FIGURES, USAGE_FIGURES,
};
use super::STEP;

/// Step records kept: one hour at the recording step.
pub const FINE_CAP: usize = (3600 / STEP.as_secs()) as usize;
/// Per-minute buckets kept: 24 hours.
pub const MINUTE_CAP: usize = 24 * 60;
/// 5-minute buckets kept: 7 days.
pub const FIVE_MINUTE_CAP: usize = 7 * 24 * 12;
/// App events kept (about 7 days of a busy PC; the oldest go first).
pub const EVENT_CAP: usize = 20_000;
pub const MARKER_CAP: usize = 2_000;
/// At most this many apps per bucket; the rest are folded into "Everything else".
pub const MAX_APPS_PER_BUCKET: usize = 32;

/// A list that drops its oldest entry when full.
#[derive(Debug)]
pub struct Ring<T> {
    items: VecDeque<T>,
    cap: usize,
}

impl<T> Ring<T> {
    pub fn new(cap: usize) -> Self {
        Self {
            items: VecDeque::new(),
            cap,
        }
    }

    pub fn push(&mut self, item: T) {
        if self.items.len() == self.cap {
            self.items.pop_front();
        }
        self.items.push_back(item);
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Oldest first.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &T> {
        self.items.iter()
    }

    pub fn last(&self) -> Option<&T> {
        self.items.back()
    }
}

/// Time-weighted average and the largest single-step value over a bucket.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Stat {
    pub avg: f32,
    pub peak: f32,
}

/// One app over a bucket. Steps in which the app was folded into "Everything else" count as
/// zero here (they are in `other`), so apps plus `other` still add up to the process total.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RolledApp {
    pub app: AppId,
    /// Seconds in which it was above the thresholds.
    pub active_secs: f32,
    pub peak_procs: u16,
    /// False when its disk figures were unknown in every step it was kept.
    pub disk_known: bool,
    /// cpu, mem, gpu, disk read, disk write (as [`Usage::values`]).
    pub figures: [Stat; USAGE_FIGURES],
}

/// A per-minute or 5-minute summary.
#[derive(Clone, Debug, PartialEq)]
pub struct Bucket {
    /// Start of the period (UTC ms, a multiple of the period).
    pub start_utc_ms: i64,
    pub first_seq: u64,
    pub last_seq: u64,
    /// Seconds of the period that were actually recorded.
    pub covered_secs: f32,
    /// As [`SystemTotals::values`]; `None` when the figure was never available.
    pub totals: [Option<Stat>; TOTAL_FIGURES],
    pub apps: Vec<RolledApp>,
    /// Everything else. Its peak is an upper bound once apps were folded at bucket level.
    pub other: [Stat; USAGE_FIGURES],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    Minute,
    FiveMinutes,
}

impl Tier {
    pub fn period_ms(self) -> i64 {
        match self {
            Tier::Minute => 60_000,
            Tier::FiveMinutes => 300_000,
        }
    }
}

/// Receives every record as it is made. Part 2's file writer implements this; the recorder
/// calls it on its own thread, never on the UI thread.
pub trait Sink: Send {
    fn step(&mut self, _names: &NameTable, _record: &StepRecord) {}
    fn bucket(&mut self, _names: &NameTable, _tier: Tier, _bucket: &Bucket) {}
    fn event(&mut self, _names: &NameTable, _event: &AppEvent) {}
    fn marker(&mut self, _marker: &Marker) {}
    /// Recording stops: write out anything buffered.
    fn finish(&mut self) {}
}

#[derive(Clone, Copy, Debug, Default)]
struct StatAcc {
    weighted: f64,
    secs: f64,
    peak: f64,
}

impl StatAcc {
    fn add(&mut self, v: f64, secs: f64) {
        self.weighted += v * secs;
        self.secs += secs;
        self.peak = self.peak.max(v);
    }

    /// Average over `covered` seconds (absent steps count as zero).
    fn stat(&self, covered: f64) -> Stat {
        Stat {
            avg: if covered > 0.0 {
                (self.weighted / covered) as f32
            } else {
                0.0
            },
            peak: self.peak as f32,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct AppAcc {
    figures: [StatAcc; USAGE_FIGURES],
    active: f64,
    peak_procs: u16,
    disk_known: bool,
}

impl AppAcc {
    fn add(&mut self, u: &Usage, secs: f64) {
        for (acc, v) in self.figures.iter_mut().zip(u.values()) {
            acc.add(v, secs);
        }
    }
}

/// The open bucket of one tier.
#[derive(Debug)]
struct Acc {
    tier: Tier,
    key: i64,
    first_seq: u64,
    last_seq: u64,
    covered: f64,
    totals: [StatAcc; TOTAL_FIGURES],
    apps: HashMap<AppId, AppAcc>,
    other: AppAcc,
}

impl Acc {
    fn new(tier: Tier, key: i64, seq: u64) -> Self {
        Self {
            tier,
            key,
            first_seq: seq,
            last_seq: seq,
            covered: 0.0,
            totals: Default::default(),
            apps: HashMap::new(),
            other: AppAcc::default(),
        }
    }

    fn add(&mut self, r: &StepRecord) {
        let secs = r.secs as f64;
        self.last_seq = r.seq;
        self.covered += secs;
        for (acc, v) in self.totals.iter_mut().zip(r.totals.values()) {
            if let Some(v) = v {
                acc.add(v, secs);
            }
        }
        for a in &r.apps {
            let acc = self.apps.entry(a.app).or_default();
            acc.add(&a.usage, secs);
            acc.active += secs;
            acc.peak_procs = acc.peak_procs.max(a.procs);
            acc.disk_known |= a.disk_known;
        }
        self.other.add(&r.other.usage, secs);
    }

    fn close(self) -> Bucket {
        let covered = self.covered;
        let totals = self.totals.map(|a| (a.secs > 0.0).then(|| a.stat(a.secs)));
        let mut apps: Vec<RolledApp> = self
            .apps
            .into_iter()
            .map(|(app, a)| RolledApp {
                app,
                active_secs: a.active as f32,
                peak_procs: a.peak_procs,
                disk_known: a.disk_known,
                figures: a.figures.map(|f| f.stat(covered)),
            })
            .collect();
        let mut other = self.other.figures.map(|f| f.stat(covered));
        // Rank by the largest share of the bucket's process total, as a step does.
        let mut total = [0.0f64; USAGE_FIGURES];
        for a in &apps {
            for (t, s) in total.iter_mut().zip(&a.figures) {
                *t += s.avg as f64;
            }
        }
        for (t, s) in total.iter_mut().zip(&other) {
            *t += s.avg as f64;
        }
        let share = |a: &RolledApp| {
            let f = |i: usize| {
                if total[i] > 0.0 {
                    a.figures[i].avg as f64 / total[i]
                } else {
                    0.0
                }
            };
            let disk_total = total[3] + total[4];
            let disk = if disk_total > 0.0 {
                (a.figures[3].avg + a.figures[4].avg) as f64 / disk_total
            } else {
                0.0
            };
            f(0).max(f(1)).max(f(2)).max(disk)
        };
        apps.sort_by(|a, b| share(b).total_cmp(&share(a)).then(a.app.cmp(&b.app)));
        for a in apps.drain(MAX_APPS_PER_BUCKET.min(apps.len())..) {
            for (o, s) in other.iter_mut().zip(a.figures) {
                o.avg += s.avg;
                o.peak += s.peak;
            }
        }
        apps.shrink_to_fit();
        Bucket {
            start_utc_ms: self.key * self.tier.period_ms(),
            first_seq: self.first_seq,
            last_seq: self.last_seq,
            covered_secs: covered as f32,
            totals,
            apps,
            other,
        }
    }
}

/// Everything recorded in this run, bounded. Shared with readers behind a mutex; the recorder
/// holds the lock only while it adds one step.
#[derive(Debug)]
pub struct Store {
    pub names: NameTable,
    fine: Ring<StepRecord>,
    minutes: Ring<Bucket>,
    five_minutes: Ring<Bucket>,
    events: Ring<AppEvent>,
    markers: Ring<Marker>,
    open: [Option<Acc>; 2],
}

impl Default for Store {
    fn default() -> Self {
        Self {
            names: NameTable::default(),
            fine: Ring::new(FINE_CAP),
            minutes: Ring::new(MINUTE_CAP),
            five_minutes: Ring::new(FIVE_MINUTE_CAP),
            events: Ring::new(EVENT_CAP),
            markers: Ring::new(MARKER_CAP),
            open: [None, None],
        }
    }
}

const TIERS: [Tier; 2] = [Tier::Minute, Tier::FiveMinutes];

impl Store {
    pub fn fine(&self) -> &Ring<StepRecord> {
        &self.fine
    }

    pub fn tier(&self, tier: Tier) -> &Ring<Bucket> {
        match tier {
            Tier::Minute => &self.minutes,
            Tier::FiveMinutes => &self.five_minutes,
        }
    }

    pub fn events(&self) -> &Ring<AppEvent> {
        &self.events
    }

    pub fn markers(&self) -> &Ring<Marker> {
        &self.markers
    }

    fn ring_mut(&mut self, tier: Tier) -> &mut Ring<Bucket> {
        match tier {
            Tier::Minute => &mut self.minutes,
            Tier::FiveMinutes => &mut self.five_minutes,
        }
    }

    /// Adds a step: to the fine ring and to the open bucket of each tier, closing a bucket
    /// when the record falls in another period (forward or, after a clock change, backward).
    pub fn push_step(&mut self, record: StepRecord, sinks: &mut [Box<dyn Sink>]) {
        // A step covers the time up to its reading: a reading at 12:01:00 belongs to 12:00.
        let at = record.utc_ms - 1;
        for (i, tier) in TIERS.into_iter().enumerate() {
            let key = at.div_euclid(tier.period_ms());
            if self.open[i].as_ref().is_some_and(|a| a.key != key) {
                self.close(i, sinks);
            }
            self.open[i]
                .get_or_insert_with(|| Acc::new(tier, key, record.seq))
                .add(&record);
        }
        for sink in sinks.iter_mut() {
            sink.step(&self.names, &record);
        }
        self.fine.push(record);
    }

    /// Closes the open buckets, so none spans a gap or a clock change.
    pub fn close_buckets(&mut self, sinks: &mut [Box<dyn Sink>]) {
        for i in 0..TIERS.len() {
            self.close(i, sinks);
        }
    }

    fn close(&mut self, i: usize, sinks: &mut [Box<dyn Sink>]) {
        if let Some(acc) = self.open[i].take() {
            let tier = acc.tier;
            let bucket = acc.close();
            for sink in sinks.iter_mut() {
                sink.bucket(&self.names, tier, &bucket);
            }
            self.ring_mut(tier).push(bucket);
        }
    }

    pub fn push_event(&mut self, event: AppEvent, sinks: &mut [Box<dyn Sink>]) {
        for sink in sinks.iter_mut() {
            sink.event(&self.names, &event);
        }
        self.events.push(event);
    }

    pub fn push_marker(&mut self, marker: Marker, sinks: &mut [Box<dyn Sink>]) {
        for sink in sinks.iter_mut() {
            sink.marker(&marker);
        }
        self.markers.push(marker);
    }

    /// Rough bytes held, for the bound check and the PR's numbers. Counts the heap parts.
    pub fn approx_bytes(&self) -> usize {
        use std::mem::size_of;
        let fine: usize = self
            .fine
            .iter()
            .map(|r| {
                size_of::<StepRecord>() + r.apps.capacity() * size_of::<super::record::AppUsage>()
            })
            .sum();
        let bucket = |b: &Bucket| size_of::<Bucket>() + b.apps.capacity() * size_of::<RolledApp>();
        let tiers: usize = self.minutes.iter().map(bucket).sum::<usize>()
            + self.five_minutes.iter().map(bucket).sum::<usize>();
        fine + tiers
            + self.events.len() * size_of::<AppEvent>()
            + self.markers.len() * size_of::<Marker>()
            + self.names.len() * (2 * super::record::MAX_NAME_LEN)
    }
}

#[cfg(test)]
pub(crate) fn totals(cpu: f32) -> super::record::SystemTotals {
    super::record::SystemTotals {
        cpu,
        mem_used: 8 << 30,
        mem_total: 16 << 30,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::super::record::{AppUsage, Folded};
    use super::*;

    fn step(seq: u64, utc_ms: i64, secs: f32, cpu: f32, apps: Vec<AppUsage>) -> StepRecord {
        StepRecord {
            seq,
            utc_ms,
            secs,
            totals: totals(cpu),
            apps,
            other: Folded {
                apps: 1,
                procs: 1,
                usage: Usage {
                    cpu: 1.0,
                    ..Usage::default()
                },
            },
        }
    }

    fn app(app: AppId, cpu: f32) -> AppUsage {
        AppUsage {
            app,
            procs: 1,
            disk_known: true,
            usage: Usage {
                cpu,
                ..Usage::default()
            },
        }
    }

    #[test]
    fn rings_drop_the_oldest() {
        let mut r = Ring::new(3);
        for i in 0..10 {
            r.push(i);
        }
        assert_eq!(r.iter().copied().collect::<Vec<_>>(), vec![7, 8, 9]);
        assert_eq!(r.capacity(), 3);
    }

    #[test]
    fn steps_roll_up_into_minutes_with_average_and_peak() {
        let mut store = Store::default();
        let mut sinks: Vec<Box<dyn Sink>> = Vec::new();
        // 12 steps of 5 s in minute 0: cpu 10 except one 90 % spike; app 7 in half the steps.
        for i in 0..12u64 {
            let cpu = if i == 5 { 90.0 } else { 10.0 };
            let apps = if i % 2 == 0 {
                vec![app(7, 4.0)]
            } else {
                vec![]
            };
            store.push_step(
                step(i + 1, (i as i64 + 1) * 5000, 5.0, cpu, apps),
                &mut sinks,
            );
        }
        assert!(store.tier(Tier::Minute).last().is_none(), "still open");
        // The first step of minute 1 closes minute 0.
        store.push_step(step(13, 65_000, 5.0, 10.0, vec![]), &mut sinks);
        let m = store.tier(Tier::Minute).last().unwrap();
        assert_eq!(m.start_utc_ms, 0);
        assert_eq!((m.first_seq, m.last_seq), (1, 12));
        assert_eq!(m.covered_secs, 60.0);
        let cpu = m.totals[0].unwrap();
        assert!((cpu.avg - (11.0 * 10.0 + 90.0) / 12.0).abs() < 1e-4);
        assert_eq!(cpu.peak, 90.0, "a short spike stays visible");
        let a = m.apps[0];
        assert_eq!(a.app, 7);
        assert_eq!(a.active_secs, 30.0);
        assert!(
            (a.figures[0].avg - 2.0).abs() < 1e-5,
            "absent steps count as 0"
        );
        assert_eq!(a.figures[0].peak, 4.0);
        assert!(m.totals[2].is_none(), "no GPU figure was ever available");
        // The 5-minute bucket is still open.
        assert!(store.tier(Tier::FiveMinutes).last().is_none());
        assert_eq!(store.fine().len(), 13);
    }

    #[test]
    fn rolled_apps_plus_other_equal_the_rolled_process_total() {
        let mut store = Store::default();
        let mut sinks: Vec<Box<dyn Sink>> = Vec::new();
        for i in 0..12u64 {
            // 40 apps per step, so the bucket cap folds some.
            let apps = (0..40)
                .map(|a| app(a + (i as u32 % 3), 1.0 + a as f32))
                .collect();
            store.push_step(
                step(i + 1, (i as i64 + 1) * 5000, 5.0, 50.0, apps),
                &mut sinks,
            );
        }
        store.close_buckets(&mut sinks);
        let m = store.tier(Tier::Minute).last().unwrap();
        assert_eq!(m.apps.len(), MAX_APPS_PER_BUCKET);
        let rolled: f32 = m.apps.iter().map(|a| a.figures[0].avg).sum::<f32>() + m.other[0].avg;
        // Per step: 40 apps summing (1..=40) plus other's 1.0.
        let per_step: f32 = (1..=40).sum::<i32>() as f32 + 1.0;
        assert!((rolled - per_step).abs() < 1e-2, "{rolled} vs {per_step}");
    }

    #[test]
    fn memory_is_bounded_when_every_ring_is_full() {
        let mut store = Store::default();
        let mut sinks: Vec<Box<dyn Sink>> = Vec::new();
        let apps: Vec<AppUsage> = (0..24).map(|a| app(a, 1.0)).collect();
        // Far more than 7 days of 5 s steps would hold (8 days at one step per 5 s, but faster:
        // every step lands in a new 5-minute period, so every ring fills).
        let steps = (FIVE_MINUTE_CAP + 100) as u64;
        for i in 0..steps {
            let at = i as i64 * 300_000;
            store.push_step(step(i + 1, at, 5.0, 1.0, apps.clone()), &mut sinks);
            store.push_event(
                AppEvent {
                    seq: i,
                    utc_ms: at,
                    app: 1,
                    kind: super::super::record::EventKind::Started,
                    during_gap: false,
                },
                &mut sinks,
            );
        }
        assert_eq!(store.fine().len(), FINE_CAP);
        assert_eq!(store.tier(Tier::Minute).len(), MINUTE_CAP);
        assert_eq!(store.tier(Tier::FiveMinutes).len(), FIVE_MINUTE_CAP);
        assert!(store.events().len() <= EVENT_CAP);
        // Worst case with 24 apps per step and 24 per bucket: a few MB, never more.
        let bytes = store.approx_bytes();
        assert!(bytes < 12 * 1024 * 1024, "{bytes} bytes");
    }

    #[derive(Default)]
    struct Count(std::sync::Arc<std::sync::Mutex<(usize, usize)>>);
    impl Sink for Count {
        fn step(&mut self, _: &NameTable, _: &StepRecord) {
            self.0.lock().unwrap().0 += 1;
        }
        fn bucket(&mut self, _: &NameTable, _: Tier, _: &Bucket) {
            self.0.lock().unwrap().1 += 1;
        }
    }

    #[test]
    fn sinks_see_every_step_and_bucket() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new((0, 0)));
        let mut sinks: Vec<Box<dyn Sink>> = vec![Box::new(Count(seen.clone()))];
        let mut store = Store::default();
        for i in 0..3u64 {
            store.push_step(
                step(i + 1, (i as i64 + 1) * 60_000, 5.0, 1.0, vec![]),
                &mut sinks,
            );
        }
        store.close_buckets(&mut sinks);
        // 3 steps; 3 minute buckets and 1 five-minute bucket.
        assert_eq!(*seen.lock().unwrap(), (3, 4));
    }
}
