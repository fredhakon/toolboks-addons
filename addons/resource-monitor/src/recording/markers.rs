//! Notable moments (#44, part 1): what to point at on the History timeline.
//!
//! Everything here is computed from the stored history ([`super::query`] and [`super::disk`]);
//! nothing is stored except the manual marks ([`manual`]). There is no page code in this module.
//!
//! # What is found
//!
//! | Kind | Rule (constants below) |
//! |---|---|
//! | [`MomentKind::Held`] | CPU or GPU near 100 % ([`NEAR_FULL_PERCENT`]), or the disk busy ([`DISK_BUSY_BYTES_PER_SEC`]), for at least [`HELD_MIN_MS`] |
//! | [`MomentKind::MemoryHigh`] | memory in use above [`MEMORY_HIGH_PERCENT`] of installed memory for at least [`HELD_MIN_MS`] |
//! | [`MomentKind::Leak`] | one app's memory grew steadily for at least [`LEAK_MIN_DURATION_MS`] (see [`find_leaks`]) |
//! | [`MomentKind::AppStarted`], [`MomentKind::AppExited`] | for the one app asked for ([`Options::app`]) |
//! | [`MomentKind::Gap`] | nothing was recorded for at least [`GAP_MIN_MS`] (Toolboks closed, sleep, a stall) |
//! | [`MomentKind::Manual`] | the user marked the moment ([`manual::add`]) |
//!
//! # Limits worth knowing
//!
//! - The disk is recorded as throughput, not as "percent busy", so there is no 100 % to hold:
//!   [`DISK_BUSY_BYTES_PER_SEC`] is a fixed rate (read plus write) and the text says the rate.
//! - Memory in use needs the installed memory, which the curve does not carry: pass it in
//!   [`Options::mem_total`]; without it no memory marker is made.
//! - Time resolution follows the data: one minute for the last 24 hours, five minutes before
//!   that. A period over about 66 hours is read in bins wider than a minute, so edges can be
//!   off by a few minutes. A gap made by closing Toolboks is found from the empty stretch of
//!   the curve (to within one bin); a gap the recorder noted itself (sleep, a stall) is exact.
//! - A leak is judged on the five-minute summaries, over a period of recorded time in which the
//!   app was present in nearly every summary.
//!
//! Reading files: call [`find`] on a worker thread, never on the UI thread.

use std::collections::HashMap;

use super::disk::{self, Level};
use super::query::{self, Answer, Bin, Request, Resource, Sources, MAX_BINS};
use super::record::{EventKind, MarkerKind};
use super::store::Tier;

const MIN: i64 = 60_000;
const HOUR: i64 = 60 * MIN;
const FIVE_MS: i64 = 5 * MIN;
const MB: f64 = 1024.0 * 1024.0;

/// CPU or GPU counts as "near 100 %" when a bin averages at least this.
pub const NEAR_FULL_PERCENT: f64 = 95.0;
/// The disk counts as busy at this read plus write rate (bytes per second). Disk is recorded as
/// throughput, not as percent busy; this is a fixed rate, 150 MB/s.
pub const DISK_BUSY_BYTES_PER_SEC: f64 = 150.0 * MB;
/// Memory in use above this share of installed memory counts as high.
pub const MEMORY_HIGH_PERCENT: f64 = 90.0;
/// How long CPU, GPU, disk or memory must stay over its threshold to become a moment.
pub const HELD_MIN_MS: i64 = 5 * MIN;
/// Two stretches over the threshold with a recorded dip of at most this between them are one.
pub const MERGE_DIP_MS: i64 = 2 * MIN;
/// Nothing recorded for at least this long is a gap.
pub const GAP_MIN_MS: i64 = 2 * MIN;
/// A leak needs at least this long of steadily growing memory.
pub const LEAK_MIN_DURATION_MS: i64 = 3 * HOUR;
/// ... and a fitted growth of at least this many bytes over that time,
pub const LEAK_MIN_GROWTH_BYTES: f64 = 200.0 * MB;
/// ... at a rate of at least this many bytes per hour,
pub const LEAK_MIN_RATE_BYTES_PER_HOUR: f64 = 50.0 * MB;
/// ... of at least this share of the memory it started with,
pub const LEAK_MIN_RELATIVE_GROWTH: f64 = 0.10;
/// ... along a straight line: the fit must explain at least this much (R squared).
pub const LEAK_MIN_R_SQUARED: f64 = 0.95;
/// Summaries more than this apart (the app was away or nothing was recorded) end a stretch.
pub const LEAK_MAX_HOLE_MS: i64 = 15 * MIN;
/// A summary counts for an app only if it was kept on its own for at least this share of it.
pub const LEAK_MIN_ACTIVE_SHARE: f64 = 0.8;
/// Choosing a single point in time (an app started, a mark) selects this much on each side.
pub const POINT_HALF_WINDOW_MS: i64 = 5 * MIN;

/// What kind of moment, with the app name where there is one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MomentKind {
    /// CPU, GPU (near 100 %) or disk (busy) held. Never `Resource::Memory`: see `MemoryHigh`.
    Held(Resource),
    MemoryHigh,
    Leak(String),
    AppStarted(String),
    AppExited(String),
    Gap,
    Manual,
}

impl MomentKind {
    /// A short heading for a list.
    pub fn title(&self) -> String {
        match self {
            MomentKind::Held(Resource::Cpu) => "CPU near 100 %".into(),
            MomentKind::Held(Resource::Gpu) => "GPU near 100 %".into(),
            MomentKind::Held(_) => "Disk busy".into(),
            MomentKind::MemoryHigh => "Memory above 90 %".into(),
            MomentKind::Leak(app) => format!("{app} memory growing"),
            MomentKind::AppStarted(app) => format!("{app} started"),
            MomentKind::AppExited(app) => format!("{app} exited"),
            MomentKind::Gap => "Nothing recorded".into(),
            MomentKind::Manual => "Marked by you".into(),
        }
    }

    fn order(&self) -> u8 {
        match self {
            MomentKind::Held(_) => 0,
            MomentKind::MemoryHigh => 1,
            MomentKind::Leak(_) => 2,
            MomentKind::AppStarted(_) => 3,
            MomentKind::AppExited(_) => 4,
            MomentKind::Gap => 5,
            MomentKind::Manual => 6,
        }
    }
}

/// One notable moment: a kind, when it began and ended (UTC ms), and one plain sentence.
#[derive(Clone, Debug, PartialEq)]
pub struct Moment {
    pub kind: MomentKind,
    pub start_ms: i64,
    /// Equal to `start_ms` for a single point in time (an app started, a mark).
    pub end_ms: i64,
    /// One sentence in plain words, ending with a full stop.
    pub text: String,
}

impl Moment {
    pub fn is_point(&self) -> bool {
        self.end_ms <= self.start_ms
    }

    /// The period to select on the timeline when this moment is chosen: the moment itself, or
    /// [`POINT_HALF_WINDOW_MS`] on each side of a point.
    pub fn period(&self) -> (i64, i64) {
        if self.is_point() {
            (
                self.start_ms - POINT_HALF_WINDOW_MS,
                self.start_ms + POINT_HALF_WINDOW_MS,
            )
        } else {
            (self.start_ms, self.end_ms)
        }
    }
}

/// What [`find`] needs besides the history.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Installed memory in bytes. `None`: no memory marker.
    pub mem_total: Option<u64>,
    /// The selected app (exe name, any case): its started and exited moments are included.
    pub app: Option<String>,
}

/// The moments in a period, in time order, and the files that could not be read in full.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Found {
    pub moments: Vec<Moment>,
    pub problems: Vec<String>,
}

/// Finds the notable moments in `[from_ms, to_ms)`. Reads files: run it on a worker thread.
pub fn find(src: &Sources, from_ms: i64, to_ms: i64, now_ms: i64, opts: &Options) -> Found {
    let to_ms = to_ms.max(from_ms);
    let bins = (((to_ms - from_ms) + MIN - 1) / MIN).clamp(1, MAX_BINS as i64) as usize;
    let answer = query::answer(
        src,
        &Request {
            from_ms,
            to_ms,
            bins,
            app: None,
        },
        now_ms,
    );
    let mut moments = detect(&answer, opts);
    let (series, mut problems) = app_series(src, from_ms, to_ms, now_ms);
    moments.extend(find_leaks(&series));
    let marks = manual::load(&manual::file(&src.dir), now_ms);
    moments.extend(
        marks
            .marks
            .iter()
            .filter(|m| (from_ms..to_ms).contains(&m.utc_ms))
            .map(manual_moment),
    );
    problems.extend(answer.problems.iter().cloned());
    problems.extend(marks.problems);
    sort(&mut moments);
    Found { moments, problems }
}

/// The moments that a query [`Answer`] holds: held resources, high memory, gaps, and the
/// selected app's starts and exits. Leaks and manual marks need other data: see [`find`].
pub fn detect(answer: &Answer, opts: &Options) -> Vec<Moment> {
    let mut out = Vec::new();
    let bins = &answer.curve;
    out.extend(held(bins, Resource::Cpu, |b| stat(b, 0)));
    out.extend(held(bins, Resource::Gpu, |b| stat(b, 2)));
    out.extend(held(bins, Resource::Disk, |b| {
        match (stat(b, 6), stat(b, 7)) {
            (None, None) => None,
            (r, w) => Some(r.unwrap_or(0.0) + w.unwrap_or(0.0)),
        }
    }));
    if let Some(total) = opts.mem_total.filter(|t| *t > 0) {
        out.extend(memory_high(bins, total));
    }
    out.extend(gaps(answer));
    if let Some(app) = opts.app.as_deref() {
        out.extend(app_moments(answer, app));
    }
    sort(&mut out);
    out
}

fn sort(moments: &mut [Moment]) {
    moments.sort_by(|a, b| {
        (a.start_ms, a.kind.order(), a.end_ms).cmp(&(b.start_ms, b.kind.order(), b.end_ms))
    });
}

fn stat(b: &Bin, i: usize) -> Option<f64> {
    b.totals[i].map(|s| s.avg as f64)
}

// ---------------------------------------------------------------------------------------------
// Held resources and high memory

/// A stretch of bins over a threshold.
struct Run {
    start: i64,
    end: i64,
    /// Average of the figure over the recorded seconds.
    avg: f64,
}

/// The stretches of `bins` where `value` is at least `threshold`, at least [`HELD_MIN_MS`] long.
/// A bin that is mostly empty never counts; recorded dips of at most [`MERGE_DIP_MS`] between two
/// stretches do not split them; an empty stretch (a gap) always does.
fn runs(bins: &[Bin], threshold: f64, value: impl Fn(&Bin) -> Option<f64>) -> Vec<Run> {
    let over = |b: &Bin| -> Option<f64> {
        let span = (b.end_ms - b.start_ms).max(1) as f64 / 1000.0;
        if b.covered_secs < span * 0.5 {
            return None;
        }
        value(b).filter(|v| *v >= threshold)
    };
    // (first bin, last bin, sum of value * secs, secs)
    let mut found: Vec<(usize, usize, f64, f64)> = Vec::new();
    for (i, b) in bins.iter().enumerate() {
        let Some(v) = over(b) else { continue };
        let secs = b.covered_secs;
        match found.last_mut() {
            Some(last) if last.1 + 1 == i => {
                last.1 = i;
                last.2 += v * secs;
                last.3 += secs;
            }
            Some(last)
                if bins[last.1 + 1..i].iter().all(|x| !x.is_gap())
                    && bins[i].start_ms - bins[last.1].end_ms <= MERGE_DIP_MS =>
            {
                last.1 = i;
                last.2 += v * secs;
                last.3 += secs;
            }
            _ => found.push((i, i, v * secs, secs)),
        }
    }
    found
        .into_iter()
        .filter_map(|(a, z, sum, secs)| {
            let (start, end) = (bins[a].start_ms, bins[z].end_ms);
            (end - start >= HELD_MIN_MS && secs > 0.0).then(|| Run {
                start,
                end,
                avg: sum / secs,
            })
        })
        .collect()
}

fn held(bins: &[Bin], resource: Resource, value: impl Fn(&Bin) -> Option<f64>) -> Vec<Moment> {
    let (threshold, what) = match resource {
        Resource::Disk => (DISK_BUSY_BYTES_PER_SEC, "The disk"),
        Resource::Gpu => (NEAR_FULL_PERCENT, "The graphics card"),
        _ => (NEAR_FULL_PERCENT, "The processor"),
    };
    runs(bins, threshold, value)
        .into_iter()
        .map(|r| {
            let level = match resource {
                Resource::Disk => format!("busy at about {}/s", bytes_text(r.avg)),
                _ => format!("at about {:.0} %", r.avg),
            };
            Moment {
                kind: MomentKind::Held(resource),
                start_ms: r.start,
                end_ms: r.end,
                text: format!("{what} was {level} for {}.", duration_text(r.end - r.start)),
            }
        })
        .collect()
}

fn memory_high(bins: &[Bin], total: u64) -> Vec<Moment> {
    let percent = |b: &Bin| stat(b, 1).map(|used| used / total as f64 * 100.0);
    runs(bins, MEMORY_HIGH_PERCENT, percent)
        .into_iter()
        .map(|r| Moment {
            kind: MomentKind::MemoryHigh,
            start_ms: r.start,
            end_ms: r.end,
            text: format!(
                "Memory in use was above {:.0} % (about {:.0} %) for {}.",
                MEMORY_HIGH_PERCENT,
                r.avg,
                duration_text(r.end - r.start)
            ),
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Gaps

/// Gaps: stretches of the curve with nothing recorded that have recorded data on both sides
/// (before the first record or after the last one is "no history", not a gap), joined with the
/// gaps the recorder noted itself, each at least [`GAP_MIN_MS`] long. Overlapping ones are one.
fn gaps(answer: &Answer) -> Vec<Moment> {
    let mut spans: Vec<(i64, i64)> = Vec::new();
    let bins = &answer.curve;
    let first = bins.iter().position(|b| !b.is_gap());
    let last = bins.iter().rposition(|b| !b.is_gap());
    if let (Some(first), Some(last)) = (first, last) {
        let mut i = first;
        while i <= last {
            if bins[i].is_gap() {
                let mut j = i;
                while j < last && bins[j + 1].is_gap() {
                    j += 1;
                }
                spans.push((bins[i].start_ms, bins[j].end_ms));
                i = j + 1;
            } else {
                i += 1;
            }
        }
    }
    for m in &answer.markers {
        if let MarkerKind::Gap { since_utc_ms } = m.kind {
            spans.push((since_utc_ms, m.utc_ms));
        }
    }
    spans.sort_unstable();
    let mut merged: Vec<(i64, i64)> = Vec::new();
    for (a, b) in spans {
        match merged.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => merged.push((a, b)),
        }
    }
    merged
        .into_iter()
        .filter(|(a, b)| b - a >= GAP_MIN_MS)
        .map(|(a, b)| Moment {
            kind: MomentKind::Gap,
            start_ms: a,
            end_ms: b,
            text: format!(
                "Nothing was recorded for {} (Toolboks was closed or the PC was asleep).",
                duration_text(b - a)
            ),
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// One app's starts and exits

fn app_moments(answer: &Answer, app: &str) -> Vec<Moment> {
    answer
        .events
        .iter()
        .filter(|e| e.name.eq_ignore_ascii_case(app))
        .filter_map(|e| {
            let (kind, said) = match (e.kind, e.during_gap) {
                (EventKind::AlreadyRunning, _) => return None,
                (EventKind::Started, false) => (MomentKind::AppStarted(e.name.clone()), "started"),
                (EventKind::Exited, false) => (MomentKind::AppExited(e.name.clone()), "exited"),
                (EventKind::Started, true) => (
                    MomentKind::AppStarted(e.name.clone()),
                    "started at some time while nothing was recorded, and was seen",
                ),
                (EventKind::Exited, true) => (
                    MomentKind::AppExited(e.name.clone()),
                    "exited at some time while nothing was recorded, and was missed",
                ),
            };
            Some(Moment {
                kind,
                start_ms: e.utc_ms,
                end_ms: e.utc_ms,
                text: format!("{} {said}.", e.name),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Leaks

/// One point of an app's memory: the start of a five-minute summary and the average working set
/// (bytes) over the time the app was kept on its own in it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MemoryPoint {
    pub start_ms: i64,
    pub bytes: f64,
}

/// The memory of every app in the five-minute summaries of `[from, to)`, from earlier runs
/// (disk) and the run recording now (memory). Apps are by exe name, matched without case.
pub fn app_series(
    src: &Sources,
    from: i64,
    to: i64,
    now: i64,
) -> (Vec<(String, Vec<MemoryPoint>)>, Vec<String>) {
    let mut by_name: HashMap<String, (String, Vec<MemoryPoint>)> = HashMap::new();
    let mut add = |name: &str, b: &super::store::Bucket, r: &super::store::RolledApp| {
        if b.start_utc_ms < from || b.start_utc_ms >= to || b.covered_secs <= 0.0 {
            return;
        }
        let covered = b.covered_secs as f64;
        if (r.active_secs as f64) < covered * LEAK_MIN_ACTIVE_SHARE || r.active_secs <= 0.0 {
            return;
        }
        // The average counts the seconds it was folded into "everything else" as zero.
        let bytes = r.figures[query::MEM].avg as f64 * covered / r.active_secs as f64;
        by_name
            .entry(name.to_ascii_lowercase())
            .or_insert_with(|| (name.to_string(), Vec::new()))
            .1
            .push(MemoryPoint {
                start_ms: b.start_utc_ms,
                bytes,
            });
    };
    let loaded = disk::load(
        &src.dir,
        &disk::Query {
            from_ms: from,
            to_ms: to,
            levels: vec![Level::Day],
            skip_run: src.live.as_ref().map(|(run, _)| *run),
            now_ms: now,
        },
    );
    for s in &loaded.five_minutes {
        for r in &s.rec.apps {
            if let Some(name) = loaded.names.name(r.app) {
                add(name, &s.rec, r);
            }
        }
    }
    if let Some((_, store)) = &src.live {
        if let Ok(store) = store.lock() {
            for b in store.tier(Tier::FiveMinutes).iter() {
                for r in &b.apps {
                    if let Some(name) = store.names.name(r.app) {
                        add(name, b, r);
                    }
                }
            }
        }
    }
    let mut series: Vec<(String, Vec<MemoryPoint>)> = by_name.into_values().collect();
    for (_, points) in &mut series {
        points.sort_by_key(|p| p.start_ms);
        points.dedup_by_key(|p| p.start_ms);
    }
    series.sort_by(|a, b| a.0.cmp(&b.0));
    (series, loaded.problems)
}

/// Least-squares line through points `i..j` of prefix sums: (slope per hour, fitted start, R²).
struct Fit {
    sx: Vec<f64>,
    sy: Vec<f64>,
    sxy: Vec<f64>,
    sxx: Vec<f64>,
    syy: Vec<f64>,
}

impl Fit {
    fn new(points: &[MemoryPoint]) -> Fit {
        let t0 = points.first().map_or(0, |p| p.start_ms);
        let mut f = Fit {
            sx: vec![0.0],
            sy: vec![0.0],
            sxy: vec![0.0],
            sxx: vec![0.0],
            syy: vec![0.0],
        };
        for p in points {
            let x = (p.start_ms - t0) as f64 / HOUR as f64;
            let y = p.bytes;
            let push = |v: &mut Vec<f64>, add: f64| v.push(v[v.len() - 1] + add);
            push(&mut f.sx, x);
            push(&mut f.sy, y);
            push(&mut f.sxy, x * y);
            push(&mut f.sxx, x * x);
            push(&mut f.syy, y * y);
        }
        f
    }

    /// Slope (bytes per hour), the fitted value at the first point's x, and R².
    fn window(&self, i: usize, j: usize, x0: f64) -> Option<(f64, f64, f64)> {
        let n = (j - i) as f64;
        if n < 3.0 {
            return None;
        }
        let d = |v: &[f64]| v[j] - v[i];
        let (sx, sy, sxy, sxx, syy) = (
            d(&self.sx),
            d(&self.sy),
            d(&self.sxy),
            d(&self.sxx),
            d(&self.syy),
        );
        let cov = sxy - sx * sy / n;
        let varx = sxx - sx * sx / n;
        let vary = syy - sy * sy / n;
        if varx <= 0.0 || vary <= 0.0 {
            return None;
        }
        let slope = cov / varx;
        let intercept = (sy - slope * sx) / n;
        Some((slope, intercept + slope * x0, cov * cov / (varx * vary)))
    }
}

/// The leaks in each app's memory series: the longest period (starting and ending on whole hours
/// of the app's recorded stretch) of at least [`LEAK_MIN_DURATION_MS`] where memory rose along a
/// straight line (R squared at least [`LEAK_MIN_R_SQUARED`]) by at least
/// [`LEAK_MIN_GROWTH_BYTES`], [`LEAK_MIN_RATE_BYTES_PER_HOUR`] and [`LEAK_MIN_RELATIVE_GROWTH`].
/// A stretch ends where the points are more than [`LEAK_MAX_HOLE_MS`] apart. At most one leak
/// per stretch.
pub fn find_leaks(series: &[(String, Vec<MemoryPoint>)]) -> Vec<Moment> {
    let mut out = Vec::new();
    for (name, points) in series {
        let mut start = 0;
        for i in 1..=points.len() {
            let end_of_stretch = i == points.len()
                || points[i].start_ms - points[i - 1].start_ms - FIVE_MS > LEAK_MAX_HOLE_MS;
            if end_of_stretch {
                if let Some(m) = leak_in(name, &points[start..i]) {
                    out.push(m);
                }
                start = i;
            }
        }
    }
    out
}

fn leak_in(name: &str, points: &[MemoryPoint]) -> Option<Moment> {
    let first = points.first()?;
    let n = points.len();
    // Windows start and end on whole hours counted from the stretch's start and its end.
    let per_hour = (HOUR / FIVE_MS) as usize;
    let starts: Vec<usize> = (0..n).step_by(per_hour).collect();
    let mut ends: Vec<usize> = vec![n];
    ends.extend(
        (1..=n / per_hour)
            .map(|k| n - k * per_hour)
            .filter(|j| *j > 0),
    );
    let fit = Fit::new(points);
    let t0 = first.start_ms;
    // (first point, one past the last point, fitted bytes at the start, fitted bytes at the end)
    let mut best: Option<(usize, usize, f64, f64)> = None;
    let length = |i: usize, j: usize| points[j - 1].start_ms + FIVE_MS - points[i].start_ms;
    for &i in &starts {
        for &j in &ends {
            if j <= i + 1 {
                continue;
            }
            let duration = length(i, j);
            if duration < LEAK_MIN_DURATION_MS {
                continue;
            }
            let x0 = (points[i].start_ms - t0) as f64 / HOUR as f64;
            let Some((slope, at_start, r2)) = fit.window(i, j, x0) else {
                continue;
            };
            let growth = slope * duration as f64 / HOUR as f64;
            let steady = slope >= LEAK_MIN_RATE_BYTES_PER_HOUR
                && r2 >= LEAK_MIN_R_SQUARED
                && growth >= LEAK_MIN_GROWTH_BYTES
                && at_start > 0.0
                && growth >= LEAK_MIN_RELATIVE_GROWTH * at_start;
            if steady && best.is_none_or(|(bi, bj, _, _)| duration > length(bi, bj)) {
                best = Some((i, j, at_start, at_start + growth));
            }
        }
    }
    let (i, j, from, to) = best?;
    let (a, b) = (points[i].start_ms, points[j - 1].start_ms + FIVE_MS);
    Some(Moment {
        kind: MomentKind::Leak(name.to_string()),
        start_ms: a,
        end_ms: b,
        text: format!(
            "{name} used steadily more memory, from about {} to about {} over {}.",
            bytes_text(from),
            bytes_text(to),
            duration_text(b - a)
        ),
    })
}

// ---------------------------------------------------------------------------------------------
// Manual marks

fn manual_moment(m: &manual::ManualMark) -> Moment {
    Moment {
        kind: MomentKind::Manual,
        start_ms: m.utc_ms,
        end_ms: m.utc_ms,
        text: if m.note.is_empty() {
            "You marked this moment.".to_string()
        } else {
            format!("You marked this moment: {}", with_stop(&m.note))
        },
    }
}

fn with_stop(s: &str) -> String {
    if s.ends_with(['.', '!', '?']) {
        s.to_string()
    } else {
        format!("{s}.")
    }
}

pub mod manual {
    //! The marks the user makes ("Mark this moment"): the only notable moments that are stored.
    //!
    //! One small file next to the history, `marks.rmm` in [`super::super::disk::directory`]. The
    //! history loader ignores it (it only reads `<level>-...rmh` names), and it is not counted in
    //! the history size.
    //!
    //! # Format (version [`VERSION`])
    //!
    //! Little-endian. A 16-byte header: magic `TBRMMARK`, version (u16), six zero bytes. Then
    //! frames, as in the history files: tag (u8, 1 = mark), payload length (u32), payload, FNV-1a
    //! checksum (u32) of tag, length and payload. A mark's payload is the time (UTC ms, i64), the
    //! note length (u16) and the note (UTF-8). Append-only: a mark is one appended frame. A reader
    //! stops at the first frame that is cut short or fails its checksum and keeps the marks before
    //! it; a file with another magic or a newer version is skipped whole and never overwritten
    //! (a file with another magic is moved aside first when a new mark is added).
    //!
    //! Kept for [`KEEP_MS`] (like the history) and at most [`MAX_MARKS`]; older or surplus marks are
    //! dropped when a mark is added, by writing the file again (temporary file, then rename).

    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::{Path, PathBuf};

    pub const MAGIC: &[u8; 8] = b"TBRMMARK";
    pub const VERSION: u16 = 1;
    const HEADER_LEN: usize = 16;
    const TAG_MARK: u8 = 1;
    const FILE: &str = "marks.rmm";
    /// Marks older than this are dropped (the history keeps 7 days).
    pub const KEEP_MS: i64 = 7 * 24 * 3_600_000;
    /// At most this many marks are kept (the newest).
    pub const MAX_MARKS: usize = 1000;
    /// A note is cut to this many bytes (at a character boundary).
    pub const MAX_NOTE_LEN: usize = 120;
    /// A frame longer than this is damage, not data.
    const MAX_FRAME: u32 = 1024;

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct ManualMark {
        pub utc_ms: i64,
        /// A short note the user typed; empty for none. Control characters are spaces.
        pub note: String,
    }

    /// The marks read back, in time order, and what was wrong with the file.
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    pub struct Loaded {
        pub marks: Vec<ManualMark>,
        pub problems: Vec<String>,
    }

    #[derive(Debug)]
    pub enum AddError {
        /// The file was written by a newer Toolboks; it is left alone and the mark is not saved.
        NewerFormat,
        Io(std::io::Error),
    }

    impl std::fmt::Display for AddError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                AddError::NewerFormat => {
                    write!(f, "the saved marks were written by a newer Toolboks")
                }
                AddError::Io(e) => write!(f, "{e}"),
            }
        }
    }

    /// The marks file in a history folder.
    pub fn file(dir: &Path) -> PathBuf {
        dir.join(FILE)
    }

    fn fnv(bytes: &[u8]) -> u32 {
        let mut h: u32 = 0x811c_9dc5;
        for b in bytes {
            h ^= *b as u32;
            h = h.wrapping_mul(0x0100_0193);
        }
        h
    }

    fn header() -> Vec<u8> {
        let mut h = Vec::with_capacity(HEADER_LEN);
        h.extend_from_slice(MAGIC);
        h.extend_from_slice(&VERSION.to_le_bytes());
        h.extend_from_slice(&[0; 6]);
        h
    }

    /// Control characters become spaces, the note is trimmed and cut at [`MAX_NOTE_LEN`].
    pub fn clean_note(note: &str) -> String {
        let s: String = note
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let s = s.trim();
        let mut end = s.len().min(MAX_NOTE_LEN);
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s[..end].trim_end().to_string()
    }

    fn frame(m: &ManualMark) -> Vec<u8> {
        let note = m.note.as_bytes();
        let mut payload = Vec::with_capacity(10 + note.len());
        payload.extend_from_slice(&m.utc_ms.to_le_bytes());
        payload.extend_from_slice(&(note.len() as u16).to_le_bytes());
        payload.extend_from_slice(note);
        let mut out = Vec::with_capacity(payload.len() + 9);
        out.push(TAG_MARK);
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&payload);
        let sum = fnv(&out);
        out.extend_from_slice(&sum.to_le_bytes());
        out
    }

    /// What reading a file's bytes gave.
    struct Parsed {
        marks: Vec<ManualMark>,
        problem: Option<String>,
        /// The header is wrong (not our file, or too new).
        header: Option<Header>,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Header {
        Foreign,
        Newer,
    }

    fn parse(bytes: &[u8]) -> Parsed {
        let bad = |header: Header, problem: &str| Parsed {
            marks: Vec::new(),
            problem: Some(problem.to_string()),
            header: Some(header),
        };
        if bytes.len() < HEADER_LEN || &bytes[..8] != MAGIC {
            return bad(Header::Foreign, "not a marks file; skipped");
        }
        let version = u16::from_le_bytes([bytes[8], bytes[9]]);
        if version != VERSION {
            return bad(
                Header::Newer,
                &format!("format version {version} is not supported; skipped"),
            );
        }
        let mut at = HEADER_LEN;
        let mut marks = Vec::new();
        let mut problem = None;
        while at < bytes.len() {
            let rest = &bytes[at..];
            let cut = || Some(format!("cut short or damaged after {} marks", marks.len()));
            if rest.len() < 5 {
                problem = cut();
                break;
            }
            let (tag, len) = (
                rest[0],
                u32::from_le_bytes([rest[1], rest[2], rest[3], rest[4]]),
            );
            if len > MAX_FRAME || rest.len() < 5 + len as usize + 4 {
                problem = cut();
                break;
            }
            let end = 5 + len as usize;
            let sum = u32::from_le_bytes([rest[end], rest[end + 1], rest[end + 2], rest[end + 3]]);
            if sum != fnv(&rest[..end]) {
                problem = cut();
                break;
            }
            let payload = &rest[5..end];
            if tag == TAG_MARK {
                let mark = (payload.len() >= 10).then(|| {
                    let utc_ms = i64::from_le_bytes(payload[..8].try_into().unwrap());
                    let n = u16::from_le_bytes([payload[8], payload[9]]) as usize;
                    payload
                        .get(10..10 + n)
                        .and_then(|b| std::str::from_utf8(b).ok())
                        .map(|note| ManualMark {
                            utc_ms,
                            note: note.to_string(),
                        })
                });
                match mark.flatten() {
                    Some(m) => marks.push(m),
                    None => {
                        problem = cut();
                        break;
                    }
                }
            }
            // An unknown tag with a good checksum is skipped (a later version may add some).
            at += end + 4;
        }
        Parsed {
            marks,
            problem,
            header: None,
        }
    }

    /// The marks in `path`, newest `MAX_MARKS` of the last [`KEEP_MS`], in time order. A missing
    /// file is no marks and no problem; a damaged one gives the marks before the damage.
    pub fn load(path: &Path, now_ms: i64) -> Loaded {
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Loaded::default(),
            Err(e) => {
                return Loaded {
                    marks: Vec::new(),
                    problems: vec![format!("marks: {e}")],
                }
            }
        };
        let p = parse(&bytes);
        let mut marks: Vec<ManualMark> = p
            .marks
            .into_iter()
            .filter(|m| m.utc_ms > now_ms - KEEP_MS)
            .collect();
        marks.sort_by_key(|m| m.utc_ms);
        if marks.len() > MAX_MARKS {
            marks.drain(..marks.len() - MAX_MARKS);
        }
        Loaded {
            marks,
            problems: p
                .problem
                .map(|s| format!("marks: {s}"))
                .into_iter()
                .collect(),
        }
    }

    /// Saves a mark at `utc_ms` with a short note (may be empty). Appends one frame. When the
    /// file has a cut or damaged end, expired or surplus marks, it is written again without them.
    /// A file that is not a marks file is moved aside (`marks.rmm.bad`); one from a newer
    /// version is left alone and [`AddError::NewerFormat`] is returned.
    pub fn add(dir: &Path, utc_ms: i64, note: &str, now_ms: i64) -> Result<(), AddError> {
        let path = file(dir);
        fs::create_dir_all(dir).map_err(AddError::Io)?;
        let mark = ManualMark {
            utc_ms,
            note: clean_note(note),
        };
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(AddError::Io(e)),
        };
        let mut keep = Vec::new();
        if bytes.is_empty() {
            // A new file.
        } else {
            let p = parse(&bytes);
            match p.header {
                Some(Header::Newer) => return Err(AddError::NewerFormat),
                Some(Header::Foreign) => {
                    let _ = fs::rename(&path, dir.join(format!("{FILE}.bad")));
                }
                None => {
                    let before = p.marks.len();
                    keep = p
                        .marks
                        .into_iter()
                        .filter(|m| m.utc_ms > now_ms - KEEP_MS)
                        .collect();
                    // One more for the mark being added.
                    let surplus = (keep.len() + 1).saturating_sub(MAX_MARKS);
                    if surplus > 0 {
                        keep.sort_by_key(|m| m.utc_ms);
                        keep.drain(..surplus);
                    }
                    let unchanged = p.problem.is_none() && keep.len() == before;
                    if unchanged {
                        // Plain append.
                        let mut f = OpenOptions::new()
                            .append(true)
                            .open(&path)
                            .map_err(AddError::Io)?;
                        f.write_all(&frame(&mark)).map_err(AddError::Io)?;
                        return f.flush().map_err(AddError::Io);
                    }
                }
            }
        }
        // Written again: a new file, or one with damage, expired or surplus marks.
        let mut out = header();
        for m in keep.iter().chain(std::iter::once(&mark)) {
            out.extend_from_slice(&frame(m));
        }
        let tmp = dir.join(format!("{FILE}.tmp"));
        fs::write(&tmp, &out).map_err(AddError::Io)?;
        fs::rename(&tmp, &path).map_err(AddError::Io)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Words

/// "5 minutes", "1 hour 5 minutes", "3 hours".
pub fn duration_text(ms: i64) -> String {
    let minutes = ((ms.max(0) + MIN / 2) / MIN).max(1);
    let (h, m) = (minutes / 60, minutes % 60);
    let unit = |n: i64, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    match (h, m) {
        (0, m) => unit(m, "minute", "minutes"),
        (h, 0) => unit(h, "hour", "hours"),
        (h, m) => format!(
            "{} {}",
            unit(h, "hour", "hours"),
            unit(m, "minute", "minutes")
        ),
    }
}

/// "850 MB", "1.2 GB".
pub fn bytes_text(bytes: f64) -> String {
    if bytes >= 1024.0 * MB {
        format!("{:.1} GB", bytes / (1024.0 * MB))
    } else {
        format!("{:.0} MB", bytes / MB)
    }
}

#[cfg(test)]
mod tests {
    use super::super::apps::proc;
    use super::super::core::{Core, Reading};
    use super::super::disk::tests::test_dir;
    use super::super::disk::Writer;
    use super::super::query::{Event, Sources};
    use super::super::record::{Marker, SystemTotals};
    use super::super::store::{Sink, Stat, Store};
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    // ----- pure detection on made-up curves --------------------------------------------------

    fn stat(v: f64) -> Option<Stat> {
        Some(Stat {
            avg: v as f32,
            peak: v as f32,
        })
    }

    /// Bin `i` of a curve of one-minute bins starting at 0.
    fn bin(i: i64, covered: f64, totals: [Option<f64>; 10]) -> Bin {
        Bin {
            start_ms: i * MIN,
            end_ms: (i + 1) * MIN,
            covered_secs: covered,
            detail: Some(Level::Minute),
            totals: totals.map(|v| v.and_then(stat)),
            app: None,
        }
    }

    fn cpu_curve(values: &[f64]) -> Vec<Bin> {
        values
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let mut t = [None; 10];
                t[0] = Some(*v);
                bin(i as i64, 60.0, t)
            })
            .collect()
    }

    fn answer(curve: Vec<Bin>) -> Answer {
        Answer {
            curve,
            ..Answer::default()
        }
    }

    fn kinds(moments: &[Moment]) -> Vec<MomentKind> {
        moments.iter().map(|m| m.kind.clone()).collect()
    }

    #[test]
    fn cpu_near_full_for_long_enough_is_one_moment() {
        let mut v = vec![20.0; 5];
        v.extend([99.0; 6]);
        v.extend([20.0; 5]);
        let found = detect(&answer(cpu_curve(&v)), &Options::default());
        assert_eq!(kinds(&found), vec![MomentKind::Held(Resource::Cpu)]);
        assert_eq!((found[0].start_ms, found[0].end_ms), (5 * MIN, 11 * MIN));
        assert_eq!(
            found[0].text,
            "The processor was at about 99 % for 6 minutes."
        );
    }

    #[test]
    fn the_thresholds_are_exact() {
        // Exactly HELD_MIN_MS counts, a minute less does not.
        let at = |n: usize, pct: f64| {
            detect(&answer(cpu_curve(&vec![pct; n])), &Options::default()).len()
        };
        assert_eq!(at((HELD_MIN_MS / MIN) as usize, NEAR_FULL_PERCENT), 1);
        assert_eq!(at((HELD_MIN_MS / MIN) as usize - 1, NEAR_FULL_PERCENT), 0);
        assert_eq!(at(30, NEAR_FULL_PERCENT - 0.1), 0);
        assert_eq!(at(30, 100.0), 1);
    }

    #[test]
    fn a_short_dip_does_not_split_a_moment_but_a_long_one_does() {
        let run = |dip: usize| {
            let mut v = vec![99.0; 6];
            v.extend(vec![50.0; dip]);
            v.extend([99.0; 6]);
            detect(&answer(cpu_curve(&v)), &Options::default())
        };
        let joined = run((MERGE_DIP_MS / MIN) as usize);
        assert_eq!(joined.len(), 1);
        assert_eq!(joined[0].end_ms - joined[0].start_ms, 14 * MIN);
        assert_eq!(run((MERGE_DIP_MS / MIN) as usize + 1).len(), 2);
    }

    #[test]
    fn an_empty_stretch_always_splits_and_mostly_empty_bins_never_count() {
        let mut curve = cpu_curve(&[99.0; 6]);
        curve.extend((6..9).map(|i| bin(i, 0.0, [None; 10])));
        curve.extend(
            cpu_curve(&[99.0; 6])
                .into_iter()
                .enumerate()
                .map(|(k, mut b)| {
                    b.start_ms = (9 + k as i64) * MIN;
                    b.end_ms = b.start_ms + MIN;
                    b
                }),
        );
        let held: Vec<_> = detect(&answer(curve), &Options::default())
            .into_iter()
            .filter(|m| m.kind == MomentKind::Held(Resource::Cpu))
            .collect();
        assert_eq!(held.len(), 2);
        // 20 s of 60 s recorded: not counted, so 4 full bins are all that is left.
        let mut v = cpu_curve(&[99.0; 5]);
        let mut t = [None; 10];
        t[0] = Some(99.0);
        v[4] = bin(4, 20.0, t);
        assert!(detect(&answer(v), &Options::default()).is_empty());
    }

    #[test]
    fn gpu_and_disk_are_held_too_and_a_missing_gpu_is_not_a_zero() {
        let mut gpu_disk = Vec::new();
        for i in 0..8 {
            let mut t = [None; 10];
            t[2] = Some(98.0);
            t[6] = Some(100.0 * MB);
            t[7] = Some(60.0 * MB);
            gpu_disk.push(bin(i, 60.0, t));
        }
        let found = detect(&answer(gpu_disk), &Options::default());
        assert_eq!(
            kinds(&found),
            vec![
                MomentKind::Held(Resource::Gpu),
                MomentKind::Held(Resource::Disk)
            ]
        );
        assert!(found[0]
            .text
            .starts_with("The graphics card was at about 98 %"));
        assert!(
            found[1].text.contains("busy at about 160 MB/s"),
            "{}",
            found[1].text
        );
        // 100 MB/s alone is under the busy rate; no GPU figure is no marker.
        let calm: Vec<Bin> = (0..8)
            .map(|i| {
                let mut t = [None; 10];
                t[6] = Some(100.0 * MB);
                bin(i, 60.0, t)
            })
            .collect();
        assert!(detect(&answer(calm), &Options::default()).is_empty());
    }

    #[test]
    fn memory_above_ninety_percent_needs_the_installed_memory() {
        let total: u64 = 16 << 30;
        let curve = |used: f64| -> Vec<Bin> {
            (0..8)
                .map(|i| {
                    let mut t = [None; 10];
                    t[1] = Some(used);
                    bin(i, 60.0, t)
                })
                .collect()
        };
        let opts = Options {
            mem_total: Some(total),
            app: None,
        };
        let high = detect(&answer(curve(total as f64 * 0.94)), &opts);
        assert_eq!(kinds(&high), vec![MomentKind::MemoryHigh]);
        assert_eq!(
            high[0].text,
            "Memory in use was above 90 % (about 94 %) for 8 minutes."
        );
        assert!(detect(&answer(curve(total as f64 * 0.89)), &opts).is_empty());
        assert!(detect(&answer(curve(total as f64 * 0.99)), &Options::default()).is_empty());
    }

    #[test]
    fn gaps_are_found_between_recorded_data_and_joined_with_noted_ones() {
        // Empty at both ends (not gaps), recorded, 3 empty minutes, recorded, 1 empty minute.
        let rec = |i| bin(i, 60.0, [Some(10.0); 10]);
        let empty = |i| bin(i, 0.0, [None; 10]);
        let curve = vec![
            empty(0),
            empty(1),
            rec(2),
            rec(3),
            empty(4),
            empty(5),
            empty(6),
            rec(7),
            empty(8),
            rec(9),
            empty(10),
        ];
        let mut a = answer(curve);
        let found = detect(&a, &Options::default());
        assert_eq!(kinds(&found), vec![MomentKind::Gap]);
        assert_eq!((found[0].start_ms, found[0].end_ms), (4 * MIN, 7 * MIN));
        assert_eq!(
            found[0].text,
            "Nothing was recorded for 3 minutes (Toolboks was closed or the PC was asleep)."
        );
        // The recorder's own note for the same stretch (sleep) is not a second moment, and
        // gives the exact times.
        a.markers.push(Marker {
            seq: 1,
            utc_ms: 7 * MIN - 5_000,
            kind: MarkerKind::Gap {
                since_utc_ms: 4 * MIN - 20_000,
            },
        });
        let found = detect(&a, &Options::default());
        assert_eq!(found.len(), 1);
        assert_eq!(
            (found[0].start_ms, found[0].end_ms),
            (4 * MIN - 20_000, 7 * MIN)
        );
        // A noted gap under the minimum is not shown.
        a.markers.clear();
        a.markers.push(Marker {
            seq: 2,
            utc_ms: 9 * MIN + 30_000,
            kind: MarkerKind::Gap {
                since_utc_ms: 9 * MIN,
            },
        });
        assert_eq!(detect(&a, &Options::default()).len(), 1);
    }

    #[test]
    fn app_starts_and_exits_are_for_the_selected_app_only() {
        let event = |utc_ms, name: &str, kind, during_gap| Event {
            utc_ms,
            name: name.to_string(),
            kind,
            during_gap,
        };
        let a = Answer {
            events: vec![
                event(1000, "Game.exe", EventKind::AlreadyRunning, false),
                event(2000, "game.exe", EventKind::Started, false),
                event(3000, "other.exe", EventKind::Started, false),
                event(4000, "game.exe", EventKind::Exited, false),
                event(5000, "game.exe", EventKind::Started, true),
            ],
            ..Answer::default()
        };
        let opts = |app: &str| Options {
            mem_total: None,
            app: Some(app.to_string()),
        };
        let found = detect(&a, &opts("GAME.EXE"));
        assert_eq!(
            kinds(&found),
            vec![
                MomentKind::AppStarted("game.exe".into()),
                MomentKind::AppExited("game.exe".into()),
                MomentKind::AppStarted("game.exe".into()),
            ]
        );
        assert_eq!(found[0].text, "game.exe started.");
        assert_eq!(found[1].text, "game.exe exited.");
        assert!(found[2].text.contains("while nothing was recorded"));
        assert!(found.iter().all(Moment::is_point));
        assert!(detect(&a, &Options::default()).is_empty());
        assert!(detect(&a, &opts("nothing.exe")).is_empty());
    }

    // ----- leaks -----------------------------------------------------------------------------

    /// Points every 5 minutes from `t0` for `hours`, with `bytes(hours since t0)`.
    fn points(t0: i64, hours: f64, bytes: impl Fn(f64) -> f64) -> Vec<MemoryPoint> {
        (0..(hours * 12.0) as i64)
            .map(|k| MemoryPoint {
                start_ms: t0 + k * FIVE_MS,
                bytes: bytes(k as f64 / 12.0),
            })
            .collect()
    }

    fn leaks(points: Vec<MemoryPoint>) -> Vec<Moment> {
        find_leaks(&[("leaky.exe".to_string(), points)])
    }

    #[test]
    fn steady_growth_over_hours_is_a_leak_with_its_period() {
        let t0 = 10 * HOUR;
        let found = leaks(points(t0, 7.0, |h| 500.0 * MB + 100.0 * MB * h));
        assert_eq!(kinds(&found), vec![MomentKind::Leak("leaky.exe".into())]);
        assert_eq!((found[0].start_ms, found[0].end_ms), (t0, t0 + 7 * HOUR));
        assert_eq!(
            found[0].text,
            "leaky.exe used steadily more memory, from about 500 MB to about 1.2 GB over 7 hours."
        );
    }

    #[test]
    fn short_slow_flat_or_noisy_growth_is_not_a_leak() {
        let t0 = 0;
        // Too short.
        assert!(leaks(points(t0, 2.5, |h| 500.0 * MB + 100.0 * MB * h)).is_empty());
        // Too slow: 20 MB an hour.
        assert!(leaks(points(t0, 8.0, |h| 500.0 * MB + 20.0 * MB * h)).is_empty());
        // Flat.
        assert!(leaks(points(t0, 8.0, |_| 500.0 * MB)).is_empty());
        // Up and down (not a straight line).
        assert!(leaks(points(t0, 8.0, |h| 800.0 * MB + 150.0 * MB * (h * 2.0).sin())).is_empty());
        // Falling.
        assert!(leaks(points(t0, 8.0, |h| 2000.0 * MB - 100.0 * MB * h)).is_empty());
        // Big in itself: 100 MB an hour on 20 GB is under 10 % over 3 hours.
        assert!(leaks(points(t0, 3.5, |h| 20_000.0 * MB + 100.0 * MB * h)).is_empty());
    }

    #[test]
    fn growth_that_starts_after_a_flat_spell_is_found_from_near_where_it_starts() {
        let found = leaks(points(0, 9.0, |h| {
            500.0 * MB + 100.0 * MB * (h - 4.0).max(0.0)
        }));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].end_ms, 9 * HOUR);
        assert!(found[0].start_ms >= 2 * HOUR, "{}", found[0].start_ms);
        assert!(found[0].start_ms <= 4 * HOUR);
    }

    #[test]
    fn a_hole_ends_a_stretch_so_two_short_stretches_are_no_leak() {
        let mut p = points(0, 2.0, |h| 500.0 * MB + 100.0 * MB * h);
        p.extend(points(2 * HOUR + 30 * MIN, 2.0, |h| {
            700.0 * MB + 100.0 * MB * h
        }));
        assert!(leaks(p).is_empty());
        // A hole at the limit still counts as one stretch.
        let mut p = points(0, 2.0, |h| 500.0 * MB + 100.0 * MB * h);
        p.extend(points(2 * HOUR + LEAK_MAX_HOLE_MS, 2.0, |h| {
            700.0 * MB + 100.0 * MB * (h + 0.25)
        }));
        assert_eq!(leaks(p).len(), 1);
    }

    // ----- words -----------------------------------------------------------------------------

    #[test]
    fn every_kind_has_a_short_title() {
        assert_eq!(MomentKind::Held(Resource::Cpu).title(), "CPU near 100 %");
        assert_eq!(MomentKind::Held(Resource::Gpu).title(), "GPU near 100 %");
        assert_eq!(MomentKind::Held(Resource::Disk).title(), "Disk busy");
        assert_eq!(MomentKind::MemoryHigh.title(), "Memory above 90 %");
        assert_eq!(
            MomentKind::Leak("a.exe".into()).title(),
            "a.exe memory growing"
        );
        assert_eq!(
            MomentKind::AppStarted("a.exe".into()).title(),
            "a.exe started"
        );
        assert_eq!(
            MomentKind::AppExited("a.exe".into()).title(),
            "a.exe exited"
        );
        assert_eq!(MomentKind::Gap.title(), "Nothing recorded");
        assert_eq!(MomentKind::Manual.title(), "Marked by you");
    }

    #[test]
    fn durations_and_sizes_read_plainly() {
        assert_eq!(duration_text(0), "1 minute");
        assert_eq!(duration_text(5 * MIN), "5 minutes");
        assert_eq!(duration_text(HOUR), "1 hour");
        assert_eq!(duration_text(HOUR + 5 * MIN), "1 hour 5 minutes");
        assert_eq!(duration_text(3 * HOUR), "3 hours");
        assert_eq!(bytes_text(850.0 * MB), "850 MB");
        assert_eq!(bytes_text(1.2 * 1024.0 * MB), "1.2 GB");
    }

    #[test]
    fn choosing_a_moment_selects_its_period_or_a_window_around_a_point() {
        let range = Moment {
            kind: MomentKind::Gap,
            start_ms: 1000,
            end_ms: 9000,
            text: String::new(),
        };
        assert_eq!(range.period(), (1000, 9000));
        let point = Moment {
            kind: MomentKind::Manual,
            start_ms: 100_000_000,
            end_ms: 100_000_000,
            text: String::new(),
        };
        assert!(point.is_point());
        assert_eq!(
            point.period(),
            (
                100_000_000 - POINT_HALF_WINDOW_MS,
                100_000_000 + POINT_HALF_WINDOW_MS
            )
        );
    }

    // ----- manual marks ----------------------------------------------------------------------

    /// 2026-11-17 12:00 UTC: "now" in the fixtures.
    const NOW: i64 = 20_774 * 24 * HOUR + 12 * HOUR;

    fn marks_in(dir: &std::path::Path) -> manual::Loaded {
        manual::load(&manual::file(dir), NOW)
    }

    #[test]
    fn manual_marks_are_appended_read_back_in_time_order_with_clean_notes() {
        let dir = test_dir("markers-manual");
        assert_eq!(marks_in(&dir), manual::Loaded::default());
        manual::add(&dir, NOW - 5 * MIN, "stutter\nin the game", NOW).unwrap();
        manual::add(&dir, NOW - 10 * MIN, "", NOW).unwrap();
        manual::add(&dir, NOW - MIN, &"é".repeat(200), NOW).unwrap();
        let got = marks_in(&dir);
        assert!(got.problems.is_empty(), "{:?}", got.problems);
        let times: Vec<i64> = got.marks.iter().map(|m| m.utc_ms).collect();
        assert_eq!(times, vec![NOW - 10 * MIN, NOW - 5 * MIN, NOW - MIN]);
        assert_eq!(got.marks[0].note, "");
        assert_eq!(got.marks[1].note, "stutter in the game");
        assert!(got.marks[2].note.len() <= manual::MAX_NOTE_LEN);
        // The file has the header and then only appended frames.
        let bytes = std::fs::read(manual::file(&dir)).unwrap();
        assert_eq!(&bytes[..8], manual::MAGIC);
        assert_eq!(u16::from_le_bytes([bytes[8], bytes[9]]), manual::VERSION);
        assert_eq!(
            manual_moment(&got.marks[1]).text,
            "You marked this moment: stutter in the game."
        );
        assert_eq!(manual_moment(&got.marks[0]).text, "You marked this moment.");
        // The history loader never mistakes it for a history file.
        assert!(super::super::disk::list(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_marks_file_keeps_the_marks_before_the_damage_and_reports_it() {
        let dir = test_dir("markers-damaged");
        for k in 0..4 {
            manual::add(&dir, NOW - (10 - k) * MIN, &format!("mark {k}"), NOW).unwrap();
        }
        let path = manual::file(&dir);
        let good = std::fs::read(&path).unwrap();
        // Every frame here has the same size: 5 + (10 + 6) + 4 = 25 bytes after the 16-byte
        // header. A flipped byte inside the third frame: the first two marks survive.
        assert_eq!(good.len(), 16 + 4 * 25);
        let mut bad = good.clone();
        bad[16 + 2 * 25 + 8] ^= 0xff;
        std::fs::write(&path, &bad).unwrap();
        let got = marks_in(&dir);
        assert_eq!(got.marks.len(), 2, "{:?}", got.marks);
        assert_eq!(got.problems.len(), 1);
        assert!(
            got.problems[0].contains("after 2 marks"),
            "{:?}",
            got.problems
        );
        // A cut end: three whole marks and a piece of the fourth.
        std::fs::write(&path, &good[..good.len() - 7]).unwrap();
        let got = marks_in(&dir);
        assert_eq!(got.marks.len(), 3);
        assert_eq!(got.problems.len(), 1);
        // Adding after a cut end writes the file again, so the new mark is not lost behind it.
        manual::add(&dir, NOW - MIN, "after", NOW).unwrap();
        let got = marks_in(&dir);
        assert!(got.problems.is_empty(), "{:?}", got.problems);
        assert_eq!(got.marks.len(), 4);
        assert_eq!(got.marks[3].note, "after");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_foreign_file_is_moved_aside_and_a_newer_one_is_left_alone() {
        let dir = test_dir("markers-header");
        let path = manual::file(&dir);
        std::fs::write(&path, b"this is not a marks file at all").unwrap();
        let got = marks_in(&dir);
        assert!(got.marks.is_empty());
        assert_eq!(got.problems.len(), 1);
        manual::add(&dir, NOW - MIN, "new", NOW).unwrap();
        assert_eq!(marks_in(&dir).marks.len(), 1);
        assert_eq!(
            std::fs::read(dir.join("marks.rmm.bad")).unwrap(),
            b"this is not a marks file at all"
        );
        // A newer version: skipped on read, refused on add, never changed.
        let mut newer = std::fs::read(&path).unwrap();
        newer[8] = 9;
        std::fs::write(&path, &newer).unwrap();
        let got = marks_in(&dir);
        assert!(got.marks.is_empty());
        assert!(got.problems[0].contains("version 9"), "{:?}", got.problems);
        assert!(matches!(
            manual::add(&dir, NOW, "x", NOW),
            Err(manual::AddError::NewerFormat)
        ));
        assert_eq!(std::fs::read(&path).unwrap(), newer);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_and_surplus_marks_are_dropped() {
        let dir = test_dir("markers-keep");
        manual::add(&dir, NOW - manual::KEEP_MS - HOUR, "old", NOW - HOUR).unwrap();
        manual::add(&dir, NOW - 2 * HOUR, "recent", NOW - HOUR).unwrap();
        // The old one is gone when read, and the file is written again when adding.
        assert_eq!(marks_in(&dir).marks.len(), 1);
        manual::add(&dir, NOW - HOUR, "newer", NOW).unwrap();
        let bytes = std::fs::read(manual::file(&dir)).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("old"));
        // The cap keeps the newest.
        for k in 0..manual::MAX_MARKS as i64 + 5 {
            manual::add(&dir, NOW - 30 * MIN + k * 1000, "x", NOW).unwrap();
        }
        let got = marks_in(&dir);
        assert_eq!(got.marks.len(), manual::MAX_MARKS);
        assert!(got.problems.is_empty());
        assert_eq!(
            got.marks.last().unwrap().utc_ms,
            NOW - 30 * MIN + (manual::MAX_MARKS as i64 + 4) * 1000
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ----- the recorded fixture --------------------------------------------------------------

    fn totals(cpu: f32) -> SystemTotals {
        SystemTotals {
            cpu,
            mem_used: 8 << 30,
            mem_total: 16 << 30,
            ..Default::default()
        }
    }

    /// Records `[from, to)` every 5 s through `Core` into `store` and the sinks, as the recorder
    /// thread does.
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
            core.push(store, Duration::from_millis((t - from) as u64), t, load(t));
            t += 5000;
        }
        core.finish(store);
    }

    /// Ten hours ending now. Run 1 records the first hour. Toolboks is closed for 40 minutes (the
    /// gap). Run 2 records the rest: busy at 100 % for 12 minutes, 4 hours ago (the spike), and
    /// `leaky.exe` growing 100 MB an hour for 7 hours (the leak). `chrome.exe` stays flat and
    /// `noisy.exe` goes up and down: neither is a moment.
    fn fixture(dir: &std::path::Path) -> Arc<Mutex<Store>> {
        let load = |t: i64| {
            let cpu = if (NOW - 4 * HOUR..NOW - 4 * HOUR + 12 * MIN).contains(&(t - 5000)) {
                100.0
            } else {
                20.0
            };
            let mut procs = vec![
                proc("chrome.exe", 3.0, 2000),
                proc(
                    "noisy.exe",
                    1.0,
                    (600.0 + 100.0 * ((t / MIN) as f64 / 8.0).sin()) as u64,
                ),
            ];
            if (NOW - 8 * HOUR..NOW - HOUR).contains(&t) {
                let hours = (t - (NOW - 8 * HOUR)) as f64 / HOUR as f64;
                procs.push(proc("leaky.exe", 1.0, (500.0 + 100.0 * hours) as u64));
            }
            Reading {
                totals: totals(cpu),
                procs,
            }
        };
        let mut past = Store::default();
        record(
            &mut past,
            vec![Box::new(Writer::new(dir.to_owned(), 1, NOW - 10 * HOUR)) as Box<dyn Sink>],
            NOW - 10 * HOUR,
            NOW - 9 * HOUR,
            load,
        );
        let mut live = Store::default();
        let run2 = NOW - 8 * HOUR - 20 * MIN;
        record(
            &mut live,
            vec![Box::new(Writer::new(dir.to_owned(), 2, run2)) as Box<dyn Sink>],
            run2,
            NOW,
            load,
        );
        Arc::new(Mutex::new(live))
    }

    fn near(actual: i64, expected: i64, slack: i64) -> bool {
        (actual - expected).abs() <= slack
    }

    #[test]
    fn a_recorded_spike_leak_and_gap_give_exactly_those_three_moments_and_choosing_one_selects_its_period(
    ) {
        let dir = test_dir("markers-fixture");
        let live = fixture(&dir);
        let from_memory = Sources {
            dir: dir.clone(),
            live: Some((2, live)),
        };
        // The same history read from disk only, as the next run will read it.
        let from_disk = Sources {
            dir: dir.clone(),
            live: None,
        };
        let opts = Options {
            mem_total: Some(16 << 30),
            app: None,
        };
        for src in [&from_memory, &from_disk] {
            let found = find(src, NOW - 10 * HOUR, NOW, NOW, &opts);
            assert!(found.problems.is_empty(), "{:?}", found.problems);
            let got = &found.moments;
            assert_eq!(
                kinds(got),
                vec![
                    MomentKind::Gap,
                    MomentKind::Leak("leaky.exe".into()),
                    MomentKind::Held(Resource::Cpu),
                ],
                "{got:#?}"
            );
            // Choosing each one selects its own period (to within a bin).
            let by_kind = |k: &MomentKind| got.iter().find(|m| &m.kind == k).unwrap().period();
            let (a, b) = by_kind(&MomentKind::Gap);
            assert!(near(a, NOW - 9 * HOUR, MIN), "gap starts {}", NOW - a);
            assert!(
                near(b, NOW - 8 * HOUR - 20 * MIN, MIN),
                "gap ends {}",
                NOW - b
            );
            let (a, b) = by_kind(&MomentKind::Leak("leaky.exe".into()));
            assert!(near(a, NOW - 8 * HOUR, FIVE_MS), "leak starts {}", NOW - a);
            assert!(near(b, NOW - HOUR, FIVE_MS), "leak ends {}", NOW - b);
            let (a, b) = by_kind(&MomentKind::Held(Resource::Cpu));
            assert!(near(a, NOW - 4 * HOUR, MIN), "spike starts {}", NOW - a);
            assert!(
                near(b, NOW - 4 * HOUR + 12 * MIN, MIN),
                "spike ends {}",
                NOW - b
            );
            // Every one is one sentence ending in a full stop.
            for m in got {
                assert!(
                    m.text.ends_with('.') && !m.text.contains('\n'),
                    "{}",
                    m.text
                );
            }
        }
        // With the leaky app selected, its start and exit are added; nothing else changes.
        let opts = Options {
            mem_total: Some(16 << 30),
            app: Some("LEAKY.exe".into()),
        };
        let found = find(&from_disk, NOW - 10 * HOUR, NOW, NOW, &opts);
        assert_eq!(found.moments.len(), 5, "{:#?}", found.moments);
        let started = found
            .moments
            .iter()
            .find(|m| matches!(m.kind, MomentKind::AppStarted(_)))
            .unwrap();
        assert!(near(started.start_ms, NOW - 8 * HOUR, 10_000));
        let exited = found
            .moments
            .iter()
            .find(|m| matches!(m.kind, MomentKind::AppExited(_)))
            .unwrap();
        assert!(near(exited.start_ms, NOW - HOUR, 10_000));
        // A manual mark inside the period is one more; one outside is not.
        manual::add(&dir, NOW - 2 * HOUR, "the game stuttered", NOW).unwrap();
        manual::add(&dir, NOW - 20 * HOUR, "too old for this period", NOW).unwrap();
        let found = find(&from_disk, NOW - 10 * HOUR, NOW, NOW, &Options::default());
        let marks: Vec<_> = found
            .moments
            .iter()
            .filter(|m| m.kind == MomentKind::Manual)
            .collect();
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].start_ms, NOW - 2 * HOUR);
        assert_eq!(
            marks[0].period(),
            (
                NOW - 2 * HOUR - POINT_HALF_WINDOW_MS,
                NOW - 2 * HOUR + POINT_HALF_WINDOW_MS
            )
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
