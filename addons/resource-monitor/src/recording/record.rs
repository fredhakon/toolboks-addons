//! The in-memory record types of the history recorder. Part 2 (#40, #41) writes and reads
//! exactly these, so they hold numbers and short exe names only.

use std::collections::HashMap;

/// A small number that stands for one exe name in [`NameTable`].
pub type AppId = u32;

/// Exe names longer than this (in bytes) are cut at a character boundary.
pub const MAX_NAME_LEN: usize = 128;
/// Distinct exe names kept. A new name after this is counted in "Everything else" and gets no
/// events, so the table cannot grow without bound on a PC that runs many one-off tools.
pub const MAX_NAMES: usize = 4096;

/// Exe name to [`AppId`]. Names are matched without regard to ASCII case (`Chrome.exe` and
/// `chrome.exe` are one app); the casing first seen is kept for display. Ids are never reused.
#[derive(Debug, Default)]
pub struct NameTable {
    ids: HashMap<String, AppId>,
    names: Vec<String>,
}

impl NameTable {
    /// The id for `name`, adding it when new. `None` when the table is full or the name is empty.
    pub fn intern(&mut self, name: &str) -> Option<AppId> {
        let name = clip(name.trim());
        if name.is_empty() {
            return None;
        }
        let key = name.to_ascii_lowercase();
        if let Some(id) = self.ids.get(&key) {
            return Some(*id);
        }
        if self.names.len() >= MAX_NAMES {
            return None;
        }
        let id = self.names.len() as AppId;
        self.names.push(name.to_string());
        self.ids.insert(key, id);
        Some(id)
    }

    /// The display name of `id`.
    pub fn name(&self, id: AppId) -> Option<&str> {
        self.names.get(id as usize).map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    #[cfg(test)]
    pub fn id(&self, name: &str) -> Option<AppId> {
        self.ids.get(&name.to_ascii_lowercase()).copied()
    }
}

fn clip(name: &str) -> &str {
    if name.len() <= MAX_NAME_LEN {
        return name;
    }
    let mut end = MAX_NAME_LEN;
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

/// Resource use of one app (or of "Everything else") over one step.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    /// Percent of the whole machine, 0 to 100.
    pub cpu: f32,
    /// Working set in bytes.
    pub mem: u64,
    /// Percent of the main GPU, 0 to 100 (busiest engine per process, summed).
    pub gpu: f32,
    /// Bytes per second read and written, from the processes whose counters Windows gave.
    pub disk_read: f32,
    pub disk_write: f32,
}

/// Number of figures in [`Usage::values`].
pub const USAGE_FIGURES: usize = 5;

impl Usage {
    /// The figures in a fixed order: cpu, mem, gpu, disk read, disk write.
    pub fn values(&self) -> [f64; USAGE_FIGURES] {
        [
            self.cpu as f64,
            self.mem as f64,
            self.gpu as f64,
            self.disk_read as f64,
            self.disk_write as f64,
        ]
    }

    pub fn add(&mut self, other: &Usage) {
        self.cpu += other.cpu;
        self.mem += other.mem;
        self.gpu += other.gpu;
        self.disk_read += other.disk_read;
        self.disk_write += other.disk_write;
    }
}

/// One app in one step: all processes with the same exe name merged.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AppUsage {
    pub app: AppId,
    /// Processes merged into this record.
    pub procs: u16,
    /// False when Windows gave disk counters for none of its processes (protected processes
    /// without admin rights): the disk figures are then unknown, not zero.
    pub disk_known: bool,
    pub usage: Usage,
}

/// Apps below the thresholds, over the per-step cap, or without a name-table slot, summed.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Folded {
    pub apps: u16,
    pub procs: u32,
    pub usage: Usage,
}

/// System-wide figures for one step, from the system counters (not the per-process sum).
/// Per-app network use is not recorded: #37 found no way to measure it without admin rights.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SystemTotals {
    pub cpu: f32,
    pub mem_used: u64,
    pub mem_total: u64,
    pub gpu_load: Option<f32>,
    pub gpu_mem_used: Option<u64>,
    pub gpu_temp_c: Option<f32>,
    pub gpu_power_w: Option<f32>,
    pub disk_read_bps: f32,
    pub disk_write_bps: f32,
    pub net_down_bps: f32,
    pub net_up_bps: f32,
}

/// Number of figures in [`SystemTotals::values`].
pub const TOTAL_FIGURES: usize = 10;

impl SystemTotals {
    /// The figures in a fixed order: cpu, mem used, gpu load, gpu memory, gpu temperature,
    /// gpu power, disk read, disk write, net down, net up. `None` = not available this step.
    pub fn values(&self) -> [Option<f64>; TOTAL_FIGURES] {
        [
            Some(self.cpu as f64),
            Some(self.mem_used as f64),
            self.gpu_load.map(f64::from),
            self.gpu_mem_used.map(|v| v as f64),
            self.gpu_temp_c.map(f64::from),
            self.gpu_power_w.map(f64::from),
            Some(self.disk_read_bps as f64),
            Some(self.disk_write_bps as f64),
            Some(self.net_down_bps as f64),
            Some(self.net_up_bps as f64),
        ]
    }
}

/// One recording step (about 5 s).
#[derive(Clone, Debug, PartialEq)]
pub struct StepRecord {
    /// Running number, never reused within a run; orders records even when the clock moves.
    pub seq: u64,
    /// Wall clock (UTC, ms since 1970) when the reading was taken.
    pub utc_ms: i64,
    /// Real time the figures cover, from the monotonic clock (can exceed the step under load).
    pub secs: f32,
    pub totals: SystemTotals,
    /// Apps above the thresholds, at most `MAX_APPS_PER_STEP`, largest first.
    pub apps: Vec<AppUsage>,
    /// Everything else. `apps` plus `other` equals the sum over all processes.
    pub other: Folded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    /// Running when recording began (or after a gap, when `during_gap` is set on others).
    AlreadyRunning,
    /// The first process with this exe name appeared.
    Started,
    /// The last process with this exe name went away.
    Exited,
}

/// An app starting or stopping, by exe name with all its processes merged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppEvent {
    pub seq: u64,
    pub utc_ms: i64,
    pub app: AppId,
    pub kind: EventKind,
    /// True when it happened at some unknown moment inside a gap that ends at `utc_ms`.
    pub during_gap: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkerKind {
    /// Recording began (Toolboks started or recording was switched on).
    Began,
    /// Nothing was recorded from `since_utc_ms` to the marker's time (sleep, hibernate, a
    /// stalled reading, or the wall clock jumping forward).
    Gap { since_utc_ms: i64 },
    /// The wall clock moved backward or by much more than the real time that passed. Records
    /// stay in `seq` order; their times follow the new clock.
    ClockChanged { before_utc_ms: i64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Marker {
    pub seq: u64,
    pub utc_ms: i64,
    pub kind: MarkerKind,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_merge_case_and_keep_first_casing() {
        let mut t = NameTable::default();
        let a = t.intern("Chrome.exe").unwrap();
        assert_eq!(t.intern("chrome.EXE"), Some(a));
        assert_eq!(t.name(a), Some("Chrome.exe"));
        assert_eq!(t.intern("  "), None);
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn name_table_is_bounded_and_names_are_clipped() {
        let mut t = NameTable::default();
        for i in 0..MAX_NAMES {
            assert!(t.intern(&format!("app{i}.exe")).is_some());
        }
        assert_eq!(t.intern("one-too-many.exe"), None);
        assert!(t.intern("app7.exe").is_some(), "known names still resolve");
        let mut t = NameTable::default();
        let long = "é".repeat(200);
        let id = t.intern(&long).unwrap();
        assert!(t.name(id).unwrap().len() <= MAX_NAME_LEN);
    }
}
