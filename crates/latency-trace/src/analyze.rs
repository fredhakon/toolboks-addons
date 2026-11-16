//! Turns parsed events into the [`Report`]: timing, nesting (exclusive time), per-routine
//! aggregation, module resolution and ranking. Every collection here has a hard cap from
//! [`Limits`]; nothing grows with the length of the input.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Read};
use std::time::Duration;

use crate::event::{parse_block, Parsed};
use crate::scan::for_each_block;
use crate::time::{round_to_minute, MINUTE};
use crate::{
    Coverage, DriverLatency, ExecKind, InputStats, KindStats, Limits, ModuleName, Report, Stat,
    TraceLosses, WorstEvent,
};

/// Clock differences larger than this are not a time zone and mean the event is bad.
const MAX_ZONE_SKEW: i64 = 15 * 60 * MINUTE;
/// Longest module name kept, in characters.
const MAX_NAME_CHARS: usize = 64;

fn ticks(d: Duration) -> u64 {
    u64::try_from(d.as_nanos() / 100).unwrap_or(u64::MAX)
}

pub(crate) fn dur(t: u64) -> Duration {
    Duration::from_secs(t / 10_000_000) + Duration::from_nanos((t % 10_000_000) * 100)
}

#[derive(Default, Clone, Copy)]
struct KindAgg {
    count: u64,
    excl_total: u64,
    excl_max: u64,
    incl_total: u64,
    incl_max: u64,
}

impl KindAgg {
    fn add(&mut self, excl: u64, incl: u64) {
        self.count = self.count.saturating_add(1);
        self.excl_total = self.excl_total.saturating_add(excl);
        self.incl_total = self.incl_total.saturating_add(incl);
        self.excl_max = self.excl_max.max(excl);
        self.incl_max = self.incl_max.max(incl);
    }

    fn merge(&mut self, o: &KindAgg) {
        self.count = self.count.saturating_add(o.count);
        self.excl_total = self.excl_total.saturating_add(o.excl_total);
        self.incl_total = self.incl_total.saturating_add(o.incl_total);
        self.excl_max = self.excl_max.max(o.excl_max);
        self.incl_max = self.incl_max.max(o.incl_max);
    }

    fn to_stats(self) -> KindStats {
        let avg = |total: u64| total.checked_div(self.count).map_or(Duration::ZERO, dur);
        KindStats {
            count: self.count,
            exclusive: Stat {
                max: dur(self.excl_max),
                average: avg(self.excl_total),
                total: dur(self.excl_total),
            },
            inclusive: Stat {
                max: dur(self.incl_max),
                average: avg(self.incl_total),
                total: dur(self.incl_total),
            },
        }
    }
}

/// One retained worst event, before the trace start is known.
#[derive(Clone, Copy)]
struct Worst {
    kind: ExecKind,
    cpu: u32,
    start_utc: u64,
    excl: u64,
    incl: u64,
    routine: u64,
}

#[derive(Default)]
struct Agg {
    dpc: KindAgg,
    isr: KindAgg,
    worst: Vec<Worst>,
}

impl Agg {
    fn add(&mut self, w: Worst, keep: usize) {
        if w.kind == ExecKind::Isr {
            self.isr.add(w.excl, w.incl);
        } else {
            self.dpc.add(w.excl, w.incl);
        }
        push_worst(&mut self.worst, w, keep);
    }

    fn merge(&mut self, o: &Agg, keep: usize) {
        self.dpc.merge(&o.dpc);
        self.isr.merge(&o.isr);
        for w in &o.worst {
            push_worst(&mut self.worst, *w, keep);
        }
    }

    fn excl_total(&self) -> u64 {
        self.dpc.excl_total.saturating_add(self.isr.excl_total)
    }

    fn count(&self) -> u64 {
        self.dpc.count.saturating_add(self.isr.count)
    }
}

/// Keeps `list` sorted by exclusive time (longest first) and at most `keep` long.
fn push_worst(list: &mut Vec<Worst>, w: Worst, keep: usize) {
    if keep == 0 {
        return;
    }
    if list.len() >= keep && list.last().is_some_and(|l| l.excl >= w.excl) {
        return;
    }
    let at = list.partition_point(|l| l.excl >= w.excl);
    list.insert(at, w);
    list.truncate(keep);
}

/// A finished DPC/ISR on one CPU that a later, longer one may turn out to contain.
#[derive(Clone, Copy)]
struct Pending {
    start: u64,
    end: u64,
    incl: u64,
}

#[derive(Default)]
struct Cpu {
    pending: VecDeque<Pending>,
    last_end: u64,
}

struct Image {
    base: u64,
    end: u64,
    name: String,
}

struct State<'a> {
    limits: &'a Limits,
    max_plausible: u64,
    /// Wall-clock time minus UTC, in ticks, once known.
    skew: Option<i64>,
    header_window: Option<(u64, u64)>,
    losses: Option<TraceLosses>,
    seen_window: Option<(u64, u64)>,
    cpus: HashMap<u32, Cpu>,
    routines: HashMap<u64, Agg>,
    overflow: Agg,
    images: Vec<Image>,
    image_keys: HashSet<(u64, u64, String)>,
    stats: InputStats,
}

impl State<'_> {
    fn on_block(&mut self, raw: &[u8]) {
        self.stats.events_seen = self.stats.events_seen.saturating_add(1);
        let text = match std::str::from_utf8(raw) {
            Ok(s) => std::borrow::Cow::Borrowed(s),
            Err(_) => {
                self.stats.non_utf8 = self.stats.non_utf8.saturating_add(1);
                String::from_utf8_lossy(raw)
            }
        };
        match parse_block(&text) {
            Parsed::Header {
                wall,
                start_utc,
                end_utc,
                events_lost,
                buffers_lost,
            } => self.on_header(wall, start_utc, end_utc, events_lost, buffers_lost),
            Parsed::Exec {
                kind,
                cpu,
                end_wall,
                initial,
                routine,
            } => self.on_exec(kind, cpu, end_wall, initial, routine),
            Parsed::Image {
                base,
                size,
                pid,
                file_name,
            } => self.on_image(base, size, pid, &file_name),
            Parsed::Other => self.stats.other_events = self.stats.other_events.saturating_add(1),
            Parsed::Malformed => self.stats.malformed = self.stats.malformed.saturating_add(1),
        }
    }

    fn on_header(
        &mut self,
        wall: Option<u64>,
        start: Option<u64>,
        end: Option<u64>,
        events_lost: Option<u64>,
        buffers_lost: Option<u64>,
    ) {
        if events_lost.is_some() || buffers_lost.is_some() {
            self.losses = Some(TraceLosses {
                events_lost: events_lost.unwrap_or(0),
                buffers_lost: buffers_lost.unwrap_or(0),
            });
        }
        if let (Some(wall), Some(start)) = (wall, start) {
            let diff = i128::from(wall) - i128::from(start);
            if self.skew.is_none() && diff.abs() <= i128::from(MAX_ZONE_SKEW + MINUTE) {
                let skew = round_to_minute(diff as i64);
                if skew.abs() <= MAX_ZONE_SKEW {
                    self.skew = Some(skew);
                }
            }
            // Only trust the recorded window if it is sane: at most one day long.
            if let Some(end) = end {
                if end > start && end - start <= 864_000_000_000 {
                    self.header_window = Some((start, end));
                }
            }
        }
    }

    fn on_exec(&mut self, kind: ExecKind, cpu: u32, end_wall: u64, initial: u64, routine: u64) {
        // i128: `initial` comes straight from the file and may be any 64-bit value.
        let delta = i128::from(end_wall) - i128::from(initial);
        let bad = |state: &mut Self| {
            state.stats.bad_timing = state.stats.bad_timing.saturating_add(1);
        };
        if delta.abs() > i128::from(MAX_ZONE_SKEW + MINUTE) {
            return bad(self);
        }
        let delta = delta as i64;
        let skew = match self.skew {
            Some(s) => s,
            None => {
                // No header: learn the clock difference from the first sane event.
                let s = round_to_minute(delta);
                let rest = delta - s;
                if s.abs() > MAX_ZONE_SKEW || rest < 0 || rest as u64 > self.max_plausible {
                    return bad(self);
                }
                self.skew = Some(s);
                s
            }
        };
        let incl = delta - skew;
        let end = i128::from(end_wall) - i128::from(skew);
        if incl < 0 || incl as u64 > self.max_plausible || end < i128::from(incl) {
            return bad(self);
        }
        let incl = incl as u64;
        let Ok(end) = u64::try_from(end) else {
            return bad(self);
        };
        let start = end - incl;

        let excl = self.exclusive(cpu, start, end, incl);
        self.stats.dpc_isr_events = self.stats.dpc_isr_events.saturating_add(1);
        self.seen_window = Some(match self.seen_window {
            None => (start, end),
            Some((a, b)) => (a.min(start), b.max(end)),
        });

        let w = Worst {
            kind,
            cpu,
            start_utc: start,
            excl,
            incl,
            routine,
        };
        let keep = self.limits.worst_per_driver.min(crate::MAX_WORST);
        if let Some(agg) = self.routines.get_mut(&routine) {
            agg.add(w, keep);
        } else if self.routines.len() < self.limits.max_routines {
            self.routines.entry(routine).or_default().add(w, keep);
        } else {
            self.stats.routines_over_cap = self.stats.routines_over_cap.saturating_add(1);
            self.overflow.add(w, keep);
        }
    }

    /// Exclusive time of a run on `cpu`: its elapsed time minus the runs that finished inside
    /// it on the same CPU (an ISR that interrupted a DPC, a DPC inside a threaded DPC).
    fn exclusive(&mut self, cpu: u32, start: u64, end: u64, incl: u64) -> u64 {
        if !self.cpus.contains_key(&cpu) && self.cpus.len() >= self.limits.max_cpus {
            self.stats.nesting_untracked = self.stats.nesting_untracked.saturating_add(1);
            return incl;
        }
        let max_pending = self.limits.max_pending_per_cpu.max(2);
        let max_plausible = self.max_plausible;
        let state = self.cpus.entry(cpu).or_default();
        if end < state.last_end {
            self.stats.out_of_order = self.stats.out_of_order.saturating_add(1);
        }
        state.last_end = state.last_end.max(end);

        let mut nested: u64 = 0;
        while let Some(top) = state.pending.back() {
            if top.start >= start && top.end <= end {
                nested = nested.saturating_add(top.incl);
                state.pending.pop_back();
            } else {
                break;
            }
        }
        // Forget runs that no later run can contain (it would be longer than plausible).
        let horizon = end.saturating_sub(max_plausible);
        while state.pending.front().is_some_and(|p| p.start < horizon) {
            state.pending.pop_front();
        }
        state.pending.push_back(Pending { start, end, incl });
        if state.pending.len() > max_pending {
            // Merge the two oldest so the list stays bounded; their sum is still subtracted
            // from any run that contains both.
            if let (Some(a), Some(b)) = (state.pending.pop_front(), state.pending.pop_front()) {
                state.pending.push_front(Pending {
                    start: a.start,
                    end: b.end,
                    incl: a.incl.saturating_add(b.incl),
                });
            }
        }
        incl - nested.min(incl)
    }

    fn on_image(&mut self, base: u64, size: u64, pid: Option<u64>, file_name: &str) {
        self.stats.image_events = self.stats.image_events.saturating_add(1);
        // Only kernel-mode images can contain DPC and ISR routines: images loaded in the
        // System context (process 0 or 4) or mapped in the kernel half of the address space.
        let kernel = matches!(pid, Some(0 | 4)) || base >= 0x8000_0000_0000_0000;
        if !kernel {
            return;
        }
        let Some(name) = file_name_only(file_name) else {
            self.stats.malformed = self.stats.malformed.saturating_add(1);
            return;
        };
        let end = base.saturating_add(size);
        let key = (base, end, name.to_ascii_lowercase());
        if self.image_keys.contains(&key) {
            return;
        }
        if self.images.len() >= self.limits.max_images {
            self.stats.images_over_cap = self.stats.images_over_cap.saturating_add(1);
            return;
        }
        self.image_keys.insert(key);
        self.images.push(Image { base, end, name });
    }

    fn finish(mut self, scan: crate::scan::ScanStats) -> Report {
        self.stats.bytes_read = scan.bytes_read;
        self.stats.oversized = scan.oversized;
        self.stats.broken = scan.broken;
        self.stats.truncated_tail = scan.truncated_tail;
        self.stats.utf16 = scan.utf16;
        self.stats.clock_offset_minutes = self.skew.map(|s| s / MINUTE);

        let keep = self.limits.worst_per_driver.min(crate::MAX_WORST);
        self.images.sort_by_key(|i| (i.base, i.end));

        // Resolve each routine to a module, then merge per module (case-insensitive name).
        let mut by_module: HashMap<String, (String, Agg)> = HashMap::new();
        let mut unresolved = Agg::default();
        unresolved.merge(&self.overflow, keep);
        for (addr, agg) in &self.routines {
            match resolve(&self.images, *addr) {
                Some(name) => {
                    let entry = by_module
                        .entry(name.to_ascii_lowercase())
                        .or_insert_with(|| (name.to_string(), Agg::default()));
                    entry.1.merge(agg, keep);
                }
                None => unresolved.merge(agg, keep),
            }
        }

        let resolved_excl = by_module
            .values()
            .fold(0u64, |a, (_, g)| a.saturating_add(g.excl_total()));
        let resolved_events = by_module
            .values()
            .fold(0u64, |a, (_, g)| a.saturating_add(g.count()));
        let total_excl = resolved_excl.saturating_add(unresolved.excl_total());
        let total_events = resolved_events.saturating_add(unresolved.count());

        let window = self.header_window.or(self.seen_window);
        let trace_start = window.map(|w| w.0);
        let trace_length = window.map_or(Duration::ZERO, |(a, b)| dur(b.saturating_sub(a)));

        let mut all: Vec<(ModuleName, Agg)> = by_module
            .into_values()
            .map(|(name, agg)| (ModuleName::Driver(name), agg))
            .collect();
        if unresolved.count() > 0 {
            all.push((ModuleName::Unresolved, unresolved));
        }
        let dpc_runs = all
            .iter()
            .fold(0u64, |a, (_, g)| a.saturating_add(g.dpc.count));
        let isr_runs = all
            .iter()
            .fold(0u64, |a, (_, g)| a.saturating_add(g.isr.count));
        let mut drivers: Vec<DriverLatency> = all
            .into_iter()
            .map(|(module, agg)| DriverLatency {
                module,
                dpc: agg.dpc.to_stats(),
                isr: agg.isr.to_stats(),
                worst: agg
                    .worst
                    .iter()
                    .map(|w| WorstEvent {
                        kind: w.kind,
                        cpu: w.cpu,
                        at: dur(w
                            .start_utc
                            .saturating_sub(trace_start.unwrap_or(w.start_utc))),
                        start_filetime_utc: w.start_utc,
                        exclusive: dur(w.excl),
                        inclusive: dur(w.incl),
                        routine: w.routine,
                    })
                    .collect(),
            })
            .collect();
        drivers.sort_by(|a, b| {
            b.max_exclusive()
                .cmp(&a.max_exclusive())
                .then(b.total_exclusive().cmp(&a.total_exclusive()))
                .then_with(|| a.module.to_string().cmp(&b.module.to_string()))
        });
        let shown = self.limits.max_drivers.min(crate::MAX_DRIVERS);
        let drivers_not_shown = drivers.len().saturating_sub(shown);
        drivers.truncate(shown);

        Report {
            trace_length,
            trace_start_filetime_utc: trace_start,
            drivers,
            drivers_not_shown,
            dpc_runs,
            isr_runs,
            coverage: Coverage {
                resolved_exclusive: dur(resolved_excl),
                total_exclusive: dur(total_excl),
                resolved_events,
                total_events,
            },
            losses: self.losses,
            input: self.stats,
        }
    }
}

/// The module whose address range contains `addr`. When ranges overlap (a module unloaded and
/// another loaded at the same place), the one with the highest base wins.
fn resolve(images: &[Image], addr: u64) -> Option<&str> {
    let upto = images.partition_point(|i| i.base <= addr);
    images[..upto]
        .iter()
        .rev()
        .take(16)
        .find(|i| addr < i.end)
        .map(|i| i.name.as_str())
}

/// The file name part of a path, without any folders (so no user names), with control
/// characters removed and at most 64 characters long.
pub(crate) fn file_name_only(path: &str) -> Option<String> {
    let last = path.rsplit(['\\', '/']).next().unwrap_or(path);
    let clean: String = last
        .chars()
        .filter(|c| !c.is_control() && *c != '\u{FFFD}')
        .take(MAX_NAME_CHARS)
        .collect();
    let clean = clean.trim().to_string();
    (!clean.is_empty()).then_some(clean)
}

pub(crate) fn analyze<R: Read>(reader: R, limits: &Limits) -> io::Result<Report> {
    let mut state = State {
        limits,
        max_plausible: ticks(limits.max_plausible_duration).max(1),
        skew: None,
        header_window: None,
        losses: None,
        seen_window: None,
        cpus: HashMap::new(),
        routines: HashMap::new(),
        overflow: Agg::default(),
        images: Vec::new(),
        image_keys: HashSet::new(),
        stats: InputStats::default(),
    };
    let scan = for_each_block(reader, limits.max_event_bytes, |b| state.on_block(b))?;
    Ok(state.finish(scan))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_drop_folders() {
        assert_eq!(
            file_name_only(r"\SystemRoot\System32\drivers\ndis.sys").as_deref(),
            Some("ndis.sys")
        );
        assert_eq!(
            file_name_only(r"C:\Users\alice\AppData\x.sys").as_deref(),
            Some("x.sys")
        );
        assert_eq!(file_name_only(r"C:\dir\"), None);
        assert_eq!(file_name_only(&"a".repeat(500)).unwrap().len(), 64);
    }

    #[test]
    fn worst_list_stays_sorted_and_bounded() {
        let mut l = Vec::new();
        for e in [5, 1, 9, 3, 9, 7] {
            push_worst(
                &mut l,
                Worst {
                    kind: ExecKind::Dpc,
                    cpu: 0,
                    start_utc: 0,
                    excl: e,
                    incl: e,
                    routine: 0,
                },
                3,
            );
        }
        let v: Vec<u64> = l.iter().map(|w| w.excl).collect();
        assert_eq!(v, vec![9, 9, 7]);
    }

    #[test]
    fn durations() {
        assert_eq!(dur(10), Duration::from_micros(1));
        assert_eq!(dur(u64::MAX).as_secs(), u64::MAX / 10_000_000);
    }
}
