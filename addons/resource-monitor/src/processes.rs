//! The Processes page: a compact, selectable process table with an inspector on the right.
//!
//! Sampling runs on its own worker thread (every 2 s, only while the page is on screen), and
//! ending or re-prioritising processes runs on a short worker thread, so the UI never blocks.
//! Everything here lives in memory; nothing is written to disk.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{self, Align, Color32, Layout, RichText, Sense, Vec2};
use egui_extras::{Column, TableBuilder};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use windows::core::{HRESULT, PWSTR};
use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, FILETIME, HANDLE};
use windows::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
use windows::Win32::System::Threading::{
    GetPriorityClass, GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, SetPriorityClass,
    TerminateProcess, ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS,
    HIGH_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS, PROCESS_ACCESS_RIGHTS, PROCESS_CREATION_FLAGS,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION,
    PROCESS_TERMINATE, REALTIME_PRIORITY_CLASS,
};

use protected_processes::Parent;
use toolboks_shared::format::{friendly_name, rate, size_plain, uptime};
use toolboks_shared::ui::{
    body_cell, header_cell, inspector_edge, inspector_frame, pill, row_rule, table_style, CARD_PAD,
    DATA_CPU, DATA_DISK, DATA_GPU, DATA_MEMORY, DATA_NETWORK, GAP, MUTED, RED, SPACE_M,
    TEXT_SECTION,
};
use toolboks_shared::win::Handle;

use crate::lower_confirm::{is_confirmed, lower_text, settle, Pending};

mod graphs;
mod groups;
use groups::Kind;

pub(super) const ROW_HEIGHT: f32 = 28.0;
/// Header row: the column name above the system total.
const HEADER_HEIGHT: f32 = 40.0;
pub(super) const INSPECTOR_WIDTH: f32 = 240.0;
/// Width of the open/close button in front of a merged row's name.
const TOGGLE_WIDTH: f32 = 14.0;
/// Extra step in for a process shown under its opened group.
const INDENT: f32 = 12.0;
/// Time between two process refreshes while the page is visible.
pub(super) const REFRESH: Duration = Duration::from_secs(2);
/// Wait before the second refresh that makes CPU percentages meaningful.
const PRIME: Duration = Duration::from_secs(1);
/// A ping older than this means the page is no longer shown.
pub(super) const VISIBLE_WINDOW_MS: u64 = 2500;
/// Result lines shown in the inspector before "and N more".
const MAX_LINES: usize = 6;
pub(super) const PROTECTED_NOTE: &str = "Windows needs this to run";

/// System and shell processes that are never ended or re-prioritised. Lowercase.
const PROTECTED: &[&str] = &[
    "system",
    "system idle process",
    "registry",
    "memory compression",
    "secure system",
    "smss.exe",
    "csrss.exe",
    "wininit.exe",
    "winlogon.exe",
    "services.exe",
    "lsass.exe",
    "svchost.exe",
    "fontdrvhost.exe",
    "dwm.exe",
    "toolboks.exe",
];

// ---------------------------------------------------------------------------------------------
// Pure logic
// ---------------------------------------------------------------------------------------------

/// One process as sampled.
#[derive(Clone, Debug, PartialEq)]
struct Proc {
    pid: u32,
    identity: Option<ProcessIdentity>,
    name: String,
    lower_name: String,
    /// Percent of the whole machine.
    cpu: f32,
    /// Working set in bytes.
    mem: u64,
    /// Bytes per second read plus written since the previous refresh; None until this process
    /// has been seen in two refreshes in a row.
    disk: Option<f64>,
    /// GPU load, percent 0 to 100, from the main sampler's latest GPU reading; None while there
    /// is no reading (or no usable GPU).
    gpu: Option<f32>,
    /// Open TCP and UDP connections owned by the process; None when the tables could not be read.
    conns: Option<u32>,
    run_secs: u64,
    exe: Option<String>,
    /// The parent this refresh vouched for (to protect children of game launchers). A parent
    /// that has exited, whose pid was reused, or whose creation time could not be read is
    /// unverified, and the process is then protected.
    parent: Parent,
    /// Apps, Background processes or Windows processes.
    kind: Kind,
    /// The parent pid for the tree view: only a parent in the same refresh that started before
    /// this process (a reused pid is never taken for the parent). None puts it at the top.
    tree_parent: Option<u32>,
}

/// One table row: a process, or all processes of one name when grouped.
#[derive(Clone, Debug, PartialEq)]
struct Row {
    key: String,
    name: String,
    lower_name: String,
    /// The pid, or the lowest pid of a group.
    pid: u32,
    pids: Vec<u32>,
    cpu: f32,
    mem: u64,
    /// Sum of the known disk rates in the row; None while none of its processes has one.
    disk: Option<f64>,
    /// Sum of the GPU loads in the row, capped at 100 (the Overview GPU card's rule).
    gpu: Option<f32>,
    /// Open connections of every process in the row.
    conns: Option<u32>,
    run_secs: u64,
    exe: Option<String>,
    /// Parent of the first process in the row.
    parent: Parent,
    /// True when every process in the row is protected.
    protected: bool,
    /// The section: the most visible one of its processes ([`groups::merge`]).
    kind: Kind,
    /// 0 for a top-level row, 1 for a process shown under its expanded group, the tree level in
    /// the tree view.
    depth: u8,
    /// Tree parent of a one-process row ([`Proc::tree_parent`]).
    tree_parent: Option<u32>,
    /// Child rows under it in the tree view (after the filter); 0 elsewhere.
    kids: u32,
    /// True when it is opened: its processes or tree children are listed under it.
    open: bool,
    /// A process listed under its merged row: the name shows its pid.
    show_pid: bool,
}

/// How the table is laid out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum View {
    /// Apps, Background processes and Windows processes sections, rows merged by name.
    ByType,
    /// One row per program name, no sections.
    ByName,
    /// One row per process, each under the process that started it.
    Tree,
    /// One row per process.
    Flat,
}

impl View {
    const ALL: [View; 4] = [View::ByType, View::ByName, View::Tree, View::Flat];

    fn label(self) -> &'static str {
        match self {
            View::ByType => "By type",
            View::ByName => "By name",
            View::Tree => "Tree",
            View::Flat => "Flat",
        }
    }

    /// True when rows merge every process of one name.
    fn grouped(self) -> bool {
        matches!(self, View::ByType | View::ByName)
    }
}

/// One line of the table: a section caption or a row.
// Nearly every line is a row (at most three are captions), so boxing rows would only add an
// allocation per line.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
enum Line {
    /// Caption with the number of rows under it.
    Section(Kind, usize),
    Row(Row),
}

impl Line {
    fn key(&self) -> Option<&str> {
        match self {
            Line::Section(..) => None,
            Line::Row(r) => Some(&r.key),
        }
    }
}

/// The rows of one snapshot: the top-level rows of the view and, in the merged views, one row
/// per process to show under an expanded group.
#[derive(Default)]
struct Rows {
    top: Vec<Row>,
    children: Vec<Row>,
}

impl Rows {
    fn build(procs: &[Proc], view: View, protected: &dyn Fn(&Proc) -> bool) -> Rows {
        let children = if view.grouped() {
            let mut flat = build_rows(procs, false, protected);
            for r in &mut flat {
                r.depth = 1;
                r.show_pid = true;
            }
            flat
        } else {
            Vec::new()
        };
        Rows {
            top: build_rows(procs, view.grouped(), protected),
            children,
        }
    }

    fn all(&self) -> impl Iterator<Item = &Row> {
        self.top.iter().chain(&self.children)
    }
}

/// True when the row can open: a merged row of several processes, or a tree row with children.
fn expandable(r: &Row) -> bool {
    r.kids > 0 || (r.depth == 0 && r.pids.len() > 1 && !r.key.starts_with("pid:"))
}

/// Deepest tree level that is still indented further; deeper levels line up with it.
const MAX_INDENT_LEVEL: u8 = 8;

/// The tree view's lines: every process under the process that started it, siblings sorted by
/// the current column, all open except the ones in `collapsed`. With a filter, the matching
/// processes stay together with every ancestor, so you still see where they came from. Pure.
fn tree_lines(
    rows: &[Row],
    collapsed: &HashSet<String>,
    filter: &str,
    sort: SortKey,
    descending: bool,
) -> Vec<Line> {
    let mut all = rows.to_vec();
    sort_rows(&mut all, sort, descending);
    let index: std::collections::HashMap<u32, usize> =
        all.iter().enumerate().map(|(i, r)| (r.pid, i)).collect();
    let parent_of = |i: usize| {
        all[i]
            .tree_parent
            .filter(|pp| *pp != all[i].pid)
            .and_then(|pp| index.get(&pp).copied())
    };
    let keep: Vec<bool> = if filter.trim().is_empty() {
        vec![true; all.len()]
    } else {
        let matched: HashSet<u32> = filter_rows(all.clone(), filter)
            .iter()
            .map(|r| r.pid)
            .collect();
        let mut keep = vec![false; all.len()];
        for start in (0..all.len()).filter(|&i| matched.contains(&all[i].pid)) {
            // Up to the root; the step limit only guards against a malformed table.
            let mut at = Some(start);
            for _ in 0..all.len() {
                let Some(i) = at else { break };
                if keep[i] && i != start {
                    break;
                }
                keep[i] = true;
                at = parent_of(i);
            }
        }
        keep
    };
    // `all` is sorted, so every child list and the roots come out sorted too.
    let mut kids: std::collections::HashMap<usize, Vec<usize>> = std::collections::HashMap::new();
    let mut roots = Vec::new();
    for i in (0..all.len()).filter(|&i| keep[i]) {
        match parent_of(i).filter(|&pi| keep[pi]) {
            Some(pi) => kids.entry(pi).or_default().push(i),
            None => roots.push(i),
        }
    }
    let mut lines = Vec::new();
    let mut placed = vec![false; all.len()];
    let mut stack: Vec<(usize, u8)> = roots.into_iter().rev().map(|i| (i, 0)).collect();
    while let Some((i, depth)) = stack.pop() {
        if std::mem::replace(&mut placed[i], true) {
            continue;
        }
        let children = kids.get(&i).map_or(&[][..], Vec::as_slice);
        let mut row = all[i].clone();
        row.depth = depth;
        row.kids = children.len() as u32;
        row.open = !children.is_empty() && !collapsed.contains(&row.key);
        if row.open {
            stack.extend(children.iter().rev().map(|&c| (c, depth.saturating_add(1))));
        }
        lines.push(Line::Row(row));
    }
    lines
}

/// The table lines for a view: filtered and sorted rows, under section captions in the By type
/// view, each expanded group followed by its processes (sorted the same way). The filter picks
/// the top-level rows; an expanded group shows all its processes. In the tree view `expanded`
/// holds the rows closed instead (the tree starts open), see [`tree_lines`]. Pure.
fn build_lines(
    rows: &Rows,
    view: View,
    expanded: &HashSet<String>,
    filter: &str,
    sort: SortKey,
    descending: bool,
) -> Vec<Line> {
    if view == View::Tree {
        return tree_lines(&rows.top, expanded, filter, sort, descending);
    }
    let mut top = filter_rows(rows.top.clone(), filter);
    sort_rows(&mut top, sort, descending);
    let mut children: Vec<Row> = rows
        .children
        .iter()
        .filter(|c| expanded.contains(&c.lower_name))
        .cloned()
        .collect();
    sort_rows(&mut children, sort, descending);
    let push_row = |lines: &mut Vec<Line>, mut r: Row| {
        let open = expandable(&r) && expanded.contains(&r.key);
        r.open = open;
        let key = r.lower_name.clone();
        lines.push(Line::Row(r));
        if open {
            lines.extend(
                children
                    .iter()
                    .filter(|c| c.lower_name == key)
                    .cloned()
                    .map(Line::Row),
            );
        }
    };
    let mut lines = Vec::new();
    if view == View::ByType {
        for kind in Kind::ALL {
            let section: Vec<Row> = top.iter().filter(|r| r.kind == kind).cloned().collect();
            if section.is_empty() {
                continue;
            }
            lines.push(Line::Section(kind, section.len()));
            for r in section {
                push_row(&mut lines, r);
            }
        }
    } else {
        for r in top {
            push_row(&mut lines, r);
        }
    }
    lines
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SortKey {
    Name,
    Pid,
    Cpu,
    Memory,
    Disk,
    Gpu,
    Connections,
}

/// True for processes Windows needs, this app itself, and game, launcher and anti-cheat
/// processes. Protected processes are only ever opened with query access for the listing.
/// `parent` is the parent the refresh vouched for; an unverified one protects the process.
pub(super) fn is_protected(pid: u32, name: &str, parent: &Parent, own_pid: u32) -> bool {
    pid == own_pid
        || pid == 0
        || pid == 4
        || PROTECTED
            .iter()
            .any(|protected| name.eq_ignore_ascii_case(protected))
        || protected_processes::protected_reason(name, parent).is_some()
}

/// True for the processes Windows itself runs (the system part of [`PROTECTED`]); they go under
/// Windows processes. Toolboks is on that list only to protect it, so it is left out here.
fn is_system(pid: u32, name: &str) -> bool {
    pid == 0
        || pid == 4
        || PROTECTED
            .iter()
            .filter(|n| **n != "toolboks.exe")
            .any(|n| name.eq_ignore_ascii_case(n))
}

/// Why a protected process is left alone, for hovers and the inspector.
pub(super) fn protected_note(name: &str, parent: &Parent) -> &'static str {
    protected_processes::protected_reason(name, parent).map_or(PROTECTED_NOTE, |p| p.describe())
}

/// Columns after Name: PID (flat view only), CPU, Memory, Disk, GPU and Connections.
fn data_columns(grouped: bool) -> usize {
    if grouped {
        5
    } else {
        6
    }
}

/// Stable key of a row: lowercase name when grouped, process identity when not.
fn row_key(grouped: bool, p: &Proc) -> String {
    if grouped {
        p.lower_name.clone()
    } else {
        format!("pid:{}:{}", p.pid, p.identity.map_or(0, |id| id.created))
    }
}

/// The processes to show: the frozen snapshot while paused, else the live list.
#[cfg(test)]
fn shown<'a>(live: &'a [Proc], frozen: Option<&'a [Proc]>) -> &'a [Proc] {
    frozen.unwrap_or(live)
}

/// Fills in each process's parent from the same refresh. `parent_pids[i]` is the parent pid
/// the refresh reported for `procs[i]`. A parent is known only when it is in the refresh and
/// was created before the child ([`Parent::vouch`]); otherwise it stays unverified. Pure.
fn link_parents(procs: &mut [Proc], parent_pids: &[Option<u32>]) {
    let seen: std::collections::HashMap<u32, (String, Option<u64>)> = procs
        .iter()
        .map(|p| (p.pid, (p.name.clone(), p.identity.map(|id| id.created))))
        .collect();
    for (p, ppid) in procs.iter_mut().zip(parent_pids) {
        let parent = ppid.and_then(|ppid| seen.get(&ppid));
        p.parent = Parent::vouch(
            p.identity.map(|id| id.created),
            parent.map(|(name, created)| (name.as_str(), *created)),
        );
        // The tree uses the same rule: a parent that started after the child holds a reused pid.
        let child_created = p.identity.map(|id| id.created);
        p.tree_parent = match (ppid, parent, child_created) {
            (Some(ppid), Some((_, Some(created))), Some(child)) if *created < child => Some(*ppid),
            _ => None,
        };
    }
}

/// Rows for the table; grouped rows sum CPU, memory and disk of every process with the same name.
fn build_rows(procs: &[Proc], grouped: bool, protected: &dyn Fn(&Proc) -> bool) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for p in procs {
        let key = row_key(grouped, p);
        let is_prot = protected(p);
        match index.get(&key) {
            Some(&i) => {
                let r = &mut rows[i];
                r.pid = r.pid.min(p.pid);
                r.pids.push(p.pid);
                r.cpu += p.cpu;
                r.mem += p.mem;
                r.disk = match (r.disk, p.disk) {
                    (Some(a), Some(b)) => Some(a + b),
                    (a, b) => a.or(b),
                };
                r.gpu = match (r.gpu, p.gpu) {
                    (Some(a), Some(b)) => Some((a + b).min(100.0)),
                    (a, b) => a.or(b),
                };
                r.conns = match (r.conns, p.conns) {
                    (Some(a), Some(b)) => Some(a.saturating_add(b)),
                    (a, b) => a.or(b),
                };
                r.run_secs = r.run_secs.max(p.run_secs);
                if r.exe.is_none() {
                    r.exe = p.exe.clone();
                }
                r.protected &= is_prot;
                r.kind = groups::merge(r.kind, p.kind);
            }
            None => {
                index.insert(key.clone(), rows.len());
                rows.push(Row {
                    key,
                    name: p.name.clone(),
                    lower_name: p.lower_name.clone(),
                    pid: p.pid,
                    pids: vec![p.pid],
                    cpu: p.cpu,
                    mem: p.mem,
                    disk: p.disk,
                    gpu: p.gpu.map(|g| g.min(100.0)),
                    conns: p.conns,
                    run_secs: p.run_secs,
                    exe: p.exe.clone(),
                    parent: p.parent.clone(),
                    protected: is_prot,
                    kind: p.kind,
                    depth: 0,
                    tree_parent: if grouped { None } else { p.tree_parent },
                    kids: 0,
                    open: false,
                    show_pid: false,
                });
            }
        }
    }
    rows
}

/// Orders rows by `key`; ties fall back to name, then pid, whatever the direction.
fn sort_rows(rows: &mut [Row], key: SortKey, descending: bool) {
    rows.sort_by(|a, b| {
        let ord = match key {
            SortKey::Name => a.lower_name.cmp(&b.lower_name),
            SortKey::Pid => a.pid.cmp(&b.pid),
            SortKey::Cpu => a.cpu.total_cmp(&b.cpu),
            SortKey::Memory => a.mem.cmp(&b.mem),
            SortKey::Disk => a.disk.unwrap_or(0.0).total_cmp(&b.disk.unwrap_or(0.0)),
            SortKey::Gpu => a.gpu.unwrap_or(0.0).total_cmp(&b.gpu.unwrap_or(0.0)),
            SortKey::Connections => a.conns.unwrap_or(0).cmp(&b.conns.unwrap_or(0)),
        };
        let ord = if descending { ord.reverse() } else { ord };
        ord.then_with(|| a.lower_name.cmp(&b.lower_name))
            .then(a.pid.cmp(&b.pid))
    });
}

/// Keeps rows whose name or any pid contains `filter` (case-insensitive).
fn filter_rows(rows: Vec<Row>, filter: &str) -> Vec<Row> {
    let needle = filter.trim().to_lowercase();
    if needle.is_empty() {
        return rows;
    }
    rows.into_iter()
        .filter(|r| {
            r.lower_name.contains(&needle) || r.pids.iter().any(|p| p.to_string().contains(&needle))
        })
        .collect()
}

/// Splits the selected processes into those that may be ended and those left alone.
fn plan_end(selected: &[Proc], protected: &dyn Fn(&Proc) -> bool) -> (Vec<Proc>, Vec<Proc>) {
    let (skipped, to_end): (Vec<Proc>, Vec<Proc>) = selected
        .iter()
        .cloned()
        .partition(|p| p.identity.is_none() || protected(p));
    (to_end, skipped)
}

fn action_identities(processes: &[Proc]) -> Vec<ProcessIdentity> {
    let mut identities: Vec<_> = processes.iter().filter_map(|p| p.identity).collect();
    identities.sort_unstable();
    identities
}

/// The question asked before ending: `label` is an app name or a short description.
pub(super) fn confirm_text(label: &str, count: usize) -> String {
    if count <= 1 {
        format!("End {label}? Unsaved work in this app will be lost.")
    } else {
        format!("End {count} processes of {label}? Unsaved work in them will be lost.")
    }
}

/// The selected row's values for its graphs, from the newest snapshot: a name key sums every
/// process of that name, a pid key is one process. NaN where unknown, all NaN when the row's
/// processes have exited (a gap in the graphs).
fn row_values(procs: &[Proc], key: &str, cpu_ready: bool) -> graphs::Values {
    let merged = !key.starts_with("pid:");
    let matching: Vec<Proc> = procs
        .iter()
        .filter(|p| row_key(merged, p) == key)
        .cloned()
        .collect();
    let Some(r) = build_rows(&matching, merged, &|_| false).into_iter().next() else {
        return [f32::NAN; 5];
    };
    [
        if cpu_ready { r.cpu } else { f32::NAN },
        r.mem as f32,
        r.disk.map_or(f32::NAN, |d| d as f32),
        r.gpu.unwrap_or(f32::NAN),
        r.conns.map_or(f32::NAN, |n| n as f32),
    ]
}

/// Plain text for the clipboard: one line per row.
fn copy_text(rows: &[&Row], ready: bool) -> String {
    rows.iter()
        .map(|r| {
            let cpu = if ready {
                format!("{:.1}%", r.cpu)
            } else {
                "...".to_string()
            };
            let id = if r.pids.len() > 1 {
                format!("{} processes", r.pids.len())
            } else {
                format!("pid {}", r.pid)
            };
            format!("{}, {}, CPU {}, {}", r.name, id, cpu, size_plain(r.mem))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn cpu_text(cpu: f32, ready: bool) -> String {
    if ready {
        format!("{cpu:.1}%")
    } else {
        "...".to_string()
    }
}

fn disk_text(disk: Option<f64>) -> String {
    disk.map_or_else(|| "...".to_string(), rate)
}

fn gpu_text(gpu: Option<f32>) -> String {
    gpu.map_or_else(|| "...".to_string(), |g| format!("{g:.1}%"))
}

fn conns_text(conns: Option<u32>) -> String {
    conns.map_or_else(|| "...".to_string(), |n| n.to_string())
}

/// Header hover of the Connections column. Windows has no per-app network speed without admin
/// rights (#37), so the column counts connections and says so.
const CONNECTIONS_HINT: &str = "Open network connections (TCP and UDP, every state) per app, \
     the same list as the Connections page. This is a count, not traffic: Windows does not \
     report network use per app without administrator rights.";

/// Gives each process its open-connection count; `None` for all when the tables could not be
/// read. A process missing from the tables has none.
fn apply_conns(procs: &mut [Proc], by_pid: Option<&std::collections::HashMap<u32, u32>>) {
    for p in procs {
        p.conns = by_pid.map(|m| m.get(&p.pid).copied().unwrap_or(0));
    }
}

/// Gives each process its GPU load from the newest per-pid reading (`None` while there is no
/// reading). A process missing from the reading uses no GPU.
fn apply_gpu(procs: &mut [Proc], by_pid: Option<&std::collections::HashMap<u32, f32>>) {
    for p in procs {
        p.gpu = by_pid.map(|m| m.get(&p.pid).copied().unwrap_or(0.0));
    }
}

/// Which processes were present in the previous refresh. sysinfo reports a process's disk
/// bytes as a delta against its previous reading, which is 0 for a process it sees for the
/// first time, so that first "delta" is the whole lifetime I/O. A rate is only trusted for a
/// process (pid plus start time) that was already there last time.
#[derive(Default)]
struct DiskBaseline {
    seen: HashSet<ProcessIdentity>,
}

impl DiskBaseline {
    fn rate(&self, id: Option<ProcessIdentity>, bytes: u64, secs: f64) -> Option<f64> {
        let known = id.is_some_and(|id| self.seen.contains(&id));
        (known && secs > 0.0).then(|| bytes as f64 / secs)
    }

    /// Remembers the processes of this refresh; ones that are gone are dropped.
    fn commit(&mut self, ids: impl Iterator<Item = ProcessIdentity>) {
        self.seen = ids.collect();
    }
}

/// Tints the cell background; the text is drawn plain on top. The fill reaches half the gap to
/// its neighbours so a column of shaded cells reads as one band.
fn shade_cell(ui: &egui::Ui, base: Color32, level: usize) {
    if level == 0 {
        return;
    }
    let reach = ui.spacing().item_spacing / 2.0;
    let rect = ui.max_rect().expand2(reach);
    ui.painter()
        .with_clip_rect(rect)
        .rect_filled(rect, 0.0, shade_color(base, level));
}

pub(super) fn sort_arrow(active: bool, descending: bool) -> &'static str {
    match (active, descending) {
        // Plain letters: the bundled font has none of the arrow or triangle glyphs.
        (true, true) => " v",
        (true, false) => " ^",
        _ => "",
    }
}

/// Number of shading levels above "none": a cell has level 0 to `SHADE_STEPS`.
const SHADE_STEPS: usize = 4;
/// Fixed thresholds, so a shade changes only when a value really crosses a step.
/// CPU: percent of the whole machine.
const CPU_SHADE: [f64; SHADE_STEPS] = [1.0, 10.0, 25.0, 50.0];
/// Memory: percent of the installed RAM.
const MEM_SHADE: [f64; SHADE_STEPS] = [1.0, 5.0, 10.0, 25.0];
/// GPU: percent of the GPU's busiest engine, the same steps as CPU.
const GPU_SHADE: [f64; SHADE_STEPS] = CPU_SHADE;
/// Connections: open connections (5, 20, 50 and 100).
const CONN_SHADE: [f64; SHADE_STEPS] = [5.0, 20.0, 50.0, 100.0];
/// Disk: bytes per second (0.1, 1, 10 and 100 MB/s).
const DISK_SHADE: [f64; SHADE_STEPS] = [100_000.0, 1_000_000.0, 10_000_000.0, 100_000_000.0];
/// Tint opacity per level (of 255). Low enough that the plain text stays readable on top, in a
/// light or a dark theme.
const SHADE_ALPHA: [u8; SHADE_STEPS + 1] = [0, 26, 52, 78, 104];

/// How many of the ascending `steps` the value has reached: 0 (none) to `SHADE_STEPS`.
fn shade_level(value: f64, steps: &[f64; SHADE_STEPS]) -> usize {
    steps.iter().filter(|&&step| value >= step).count()
}

/// Share of the installed RAM in percent; 0 when the total is unknown.
fn mem_share(mem: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        mem as f64 / total as f64 * 100.0
    }
}

/// Background tint of `base` for a level; fully transparent at level 0.
fn shade_color(base: Color32, level: usize) -> Color32 {
    let alpha = SHADE_ALPHA[level.min(SHADE_STEPS)];
    Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), alpha)
}

/// System totals for the column headers, from the latest main-sampler reading.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Totals {
    /// Percent of the machine, 0 to 100.
    pub cpu: f32,
    pub mem_used: u64,
    /// Installed RAM; 0 while unknown.
    pub mem_total: u64,
    /// Share of time the drives were busy, when Windows reports it.
    pub disk_active: Option<f32>,
    /// Bytes per second read plus written, used when `disk_active` is missing.
    pub disk_bps: f64,
    /// Load of the main GPU's busiest engine, 0 to 100; None without a usable GPU.
    pub gpu: Option<f32>,
}

impl Totals {
    fn cpu_text(&self) -> String {
        format!("{:.0}%", self.cpu)
    }

    /// None while the installed RAM is unknown, instead of a fake 0 %.
    fn mem_text(&self) -> Option<String> {
        (self.mem_total > 0).then(|| {
            format!(
                "{:.0}%",
                mem_share(self.mem_used, self.mem_total).clamp(0.0, 100.0)
            )
        })
    }

    fn disk_text(&self) -> String {
        match self.disk_active {
            Some(active) => format!("{active:.0}%"),
            None => rate(self.disk_bps),
        }
    }

    fn gpu_text(&self) -> Option<String> {
        self.gpu.map(|g| format!("{:.0}%", g.clamp(0.0, 100.0)))
    }
}

/// Name for the screen: "Chrome", with the count after it for a group, and the pid for a
/// process shown under its opened group.
fn display_name(r: &Row) -> String {
    let name = friendly_name(&r.name);
    if r.show_pid {
        format!("{name} (pid {})", r.pid)
    } else if r.pids.len() > 1 {
        format!("{name} ({})", r.pids.len())
    } else {
        name
    }
}

/// The tiny selection model: rows remembered by key so they survive the refresh.
#[derive(Default)]
struct Selection {
    keys: HashSet<String>,
    anchor: Option<String>,
}

impl Selection {
    fn only(&mut self, key: &str) {
        self.keys.clear();
        self.keys.insert(key.to_string());
        self.anchor = Some(key.to_string());
    }

    fn toggle(&mut self, key: &str) {
        if !self.keys.remove(key) {
            self.keys.insert(key.to_string());
        }
        self.anchor = Some(key.to_string());
    }

    /// Every row from the anchor to `key` in the order shown; just `key` without an anchor.
    fn range(&mut self, key: &str, visible: &[String]) {
        let pos = |k: &str| visible.iter().position(|v| v == k);
        let (Some(to), Some(from)) = (pos(key), self.anchor.as_deref().and_then(pos)) else {
            self.only(key);
            return;
        };
        self.keys = visible[from.min(to)..=from.max(to)]
            .iter()
            .cloned()
            .collect();
    }

    fn prune(&mut self, existing: &HashSet<String>) {
        self.keys.retain(|k| existing.contains(k));
        if self.anchor.as_ref().is_some_and(|a| !existing.contains(a)) {
            self.anchor = None;
        }
    }

    fn clear(&mut self) {
        self.keys.clear();
        self.anchor = None;
    }
}

/// One result line for the inspector.
pub(super) fn result_line(
    done: &str,
    name: &str,
    pid: u32,
    result: Result<(), ActionError>,
) -> String {
    match result {
        Ok(()) => format!("{done} {name} (pid {pid})"),
        Err(ActionError::AccessDenied) => format!("{name} (pid {pid}): Access denied"),
        Err(ActionError::AlreadyLow) => format!("{name} (pid {pid}): Priority is already low"),
        Err(ActionError::Protected(note)) => format!("{name} (pid {pid}): {note}"),
        Err(ActionError::IdentityChanged) => {
            format!("{name} (pid {pid}): Windows could not verify the selected process")
        }
        Err(ActionError::Failed) => {
            format!("{name} (pid {pid}): Could not be changed, it may have closed already")
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Windows actions
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ActionError {
    AlreadyLow,
    /// Windows, this app, or a game or anti-cheat process: never opened for write access.
    /// Carries the reason shown to the user.
    Protected(&'static str),
    AccessDenied,
    IdentityChanged,
    Failed,
}

fn classify(e: &windows::core::Error) -> ActionError {
    if e.code() == HRESULT::from_win32(ERROR_ACCESS_DENIED.0) {
        ActionError::AccessDenied
    } else {
        ActionError::Failed
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct ProcessIdentity {
    pub(super) pid: u32,
    pub(super) created: u64,
}

fn filetime(time: FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

pub(super) fn snapshot_start() -> u64 {
    // SAFETY: this call reads the system time and takes no arguments.
    filetime(unsafe { GetSystemTimePreciseAsFileTime() })
}

fn creation_time(handle: HANDLE) -> windows::core::Result<u64> {
    let (mut created, mut exited, mut kernel, mut user) = Default::default();
    // SAFETY: the handle has query access and all four outputs are valid FILETIME buffers.
    unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) }?;
    Ok(filetime(created))
}

pub(super) fn sampled_identity(
    process: &sysinfo::Process,
    sample_start: u64,
) -> Option<ProcessIdentity> {
    let pid = process.pid().as_u32();
    // SAFETY: Windows validates the PID. Only query access is requested.
    let handle =
        Handle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?);
    let created = creation_time(handle.0).ok()?;
    let unix_secs = (created / 10_000_000).checked_sub(11_644_473_600)?;
    // Reject processes born during the sample, including a reused connection-owner PID.
    (created < sample_start && unix_secs == process.start_time())
        .then_some(ProcessIdentity { pid, created })
}

/// True when the live image name still matches the sampled one and the process is still not
/// protected, so a reused pid never gets a write-capable handle. Pure.
fn live_name_ok(pid: u32, sampled: &str, live: &str, parent: &Parent, own_pid: u32) -> bool {
    live.eq_ignore_ascii_case(sampled) && !is_protected(pid, live, parent, own_pid)
}

fn action_handle(
    identity: ProcessIdentity,
    expected_name: &str,
    parent: &Parent,
    access: PROCESS_ACCESS_RIGHTS,
) -> Result<Handle, ActionError> {
    let own_pid = std::process::id();
    if is_protected(identity.pid, expected_name, parent, own_pid) {
        return Err(ActionError::Protected(protected_note(
            expected_name,
            parent,
        )));
    }
    // Verify with a query-only handle first. While it stays open the pid cannot be reused, so
    // the write-capable handle below belongs to the verified process.
    // SAFETY: plain FFI call; the returned handle is wrapped for closing right away.
    let query = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, identity.pid) }
        .map(Handle)
        .map_err(|e| classify(&e))?;
    if creation_time(query.0).ok() != Some(identity.created) {
        return Err(ActionError::IdentityChanged);
    }

    let mut image = vec![0u16; 32_768];
    let mut len = image.len() as u32;
    // SAFETY: the handle has query access and the output buffer holds `len` UTF-16 units.
    unsafe {
        QueryFullProcessImageNameW(
            query.0,
            PROCESS_NAME_WIN32,
            PWSTR(image.as_mut_ptr()),
            &mut len,
        )
    }
    .map_err(|_| ActionError::IdentityChanged)?;
    let image = String::from_utf16_lossy(&image[..len as usize]);
    let live = image.rsplit('\\').next().unwrap_or_default();
    if !live_name_ok(identity.pid, expected_name, live, parent, own_pid) {
        return Err(ActionError::IdentityChanged);
    }
    // SAFETY: as above.
    let handle = unsafe {
        OpenProcess(
            access | PROCESS_QUERY_LIMITED_INFORMATION,
            false,
            identity.pid,
        )
    }
    .map(Handle)
    .map_err(|e| classify(&e))?;
    drop(query);
    Ok(handle)
}

pub(super) fn end_process(
    identity: ProcessIdentity,
    name: &str,
    parent: &Parent,
) -> Result<(), ActionError> {
    let handle = action_handle(identity, name, parent, PROCESS_TERMINATE)?;
    // SAFETY: the handle is valid and was opened with PROCESS_TERMINATE.
    unsafe { TerminateProcess(handle.0, 1) }.map_err(|e| classify(&e))
}

/// Sets the priority class and returns the previous one.
fn can_lower_priority(class: u32) -> bool {
    [
        NORMAL_PRIORITY_CLASS,
        ABOVE_NORMAL_PRIORITY_CLASS,
        HIGH_PRIORITY_CLASS,
        REALTIME_PRIORITY_CLASS,
    ]
    .iter()
    .any(|candidate| candidate.0 == class)
}

#[cfg(test)]
#[test]
fn lowering_keeps_idle_and_below_normal_priorities() {
    use windows::Win32::System::Threading::IDLE_PRIORITY_CLASS;
    assert!(!can_lower_priority(IDLE_PRIORITY_CLASS.0));
    assert!(!can_lower_priority(BELOW_NORMAL_PRIORITY_CLASS.0));
    assert!(can_lower_priority(NORMAL_PRIORITY_CLASS.0));
    assert!(can_lower_priority(HIGH_PRIORITY_CLASS.0));
}

fn set_priority(
    identity: ProcessIdentity,
    name: &str,
    parent: &Parent,
    class: u32,
    lower_only: bool,
) -> Result<u32, ActionError> {
    let handle = action_handle(identity, name, parent, PROCESS_SET_INFORMATION)?;
    // SAFETY: the handle is valid and was opened with the needed access.
    let previous = unsafe { GetPriorityClass(handle.0) };
    if previous == 0 {
        return Err(ActionError::Failed);
    }
    if lower_only && !can_lower_priority(previous) {
        return Err(ActionError::AlreadyLow);
    }
    // SAFETY: as above.
    unsafe { SetPriorityClass(handle.0, PROCESS_CREATION_FLAGS(class)) }
        .map_err(|e| classify(&e))?;
    Ok(previous)
}

/// Opens Explorer with the file selected.
fn open_location(path: &str) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let Ok(windows) = toolboks_shared::win::windows_dir() else {
        return;
    };
    let _ = std::process::Command::new(windows.join("explorer.exe"))
        .raw_arg(format!("/select,\"{path}\""))
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

enum Action {
    End(Vec<Proc>),
    Lower(Vec<Proc>),
    /// Process and the class it had before.
    Restore(Vec<(Proc, u32)>),
}

#[derive(Default)]
struct Done {
    lines: Vec<String>,
    /// Access results keyed to the exact process that was acted on.
    access: Vec<(ProcessIdentity, bool)>,
    lowered: Vec<(ProcessIdentity, u32)>,
    restored: Vec<ProcessIdentity>,
}

fn run_action(action: Action) -> Done {
    let mut done = Done::default();
    let note = |done: &mut Done, p: &Proc, ok: &str, r: Result<(), ActionError>| {
        if let Some(id) = p.identity {
            if matches!(
                r,
                Ok(()) | Err(ActionError::AccessDenied | ActionError::AlreadyLow)
            ) {
                done.access.push((id, r == Err(ActionError::AccessDenied)));
            }
        }
        done.lines.push(result_line(ok, &p.name, p.pid, r));
    };
    match action {
        Action::End(list) => {
            for p in &list {
                let result = p
                    .identity
                    .ok_or(ActionError::IdentityChanged)
                    .and_then(|id| end_process(id, &p.name, &p.parent));
                note(&mut done, p, "Ended", result);
            }
        }
        Action::Lower(list) => {
            for p in &list {
                let result = p
                    .identity
                    .ok_or(ActionError::IdentityChanged)
                    .and_then(|id| {
                        set_priority(id, &p.name, &p.parent, BELOW_NORMAL_PRIORITY_CLASS.0, true)
                            .map(|prev| (id, prev))
                    });
                match result {
                    Ok((id, prev)) => {
                        if prev != BELOW_NORMAL_PRIORITY_CLASS.0 {
                            done.lowered.push((id, prev));
                        }
                        note(&mut done, p, "Lowered priority of", Ok(()));
                    }
                    Err(e) => note(&mut done, p, "", Err(e)),
                }
            }
        }
        Action::Restore(list) => {
            for (p, prev) in &list {
                let r = p
                    .identity
                    .ok_or(ActionError::IdentityChanged)
                    .and_then(|id| {
                        set_priority(id, &p.name, &p.parent, *prev, false)?;
                        done.restored.push(id);
                        Ok(())
                    });
                note(&mut done, p, "Restored priority of", r);
            }
        }
    }
    done
}

// ---------------------------------------------------------------------------------------------
// Sampling worker
// ---------------------------------------------------------------------------------------------

struct Snapshot {
    procs: Vec<Proc>,
    /// False until the second refresh, when CPU percentages become real.
    cpu_ready: bool,
    /// Every open TCP and UDP connection on the PC; None when the tables could not be read.
    conns_total: Option<u32>,
}

struct Worker {
    rx: Receiver<Snapshot>,
    last_ping: Arc<AtomicU64>,
    epoch: Instant,
}

pub(super) fn visible(last_ping_plus_one: u64, now_ms: u64) -> bool {
    last_ping_plus_one != 0 && (now_ms + 1).saturating_sub(last_ping_plus_one) < VISIBLE_WINDOW_MS
}

impl Worker {
    fn start(ctx: &egui::Context) -> Worker {
        let (tx, rx) = mpsc::channel();
        let last_ping = Arc::new(AtomicU64::new(0));
        let epoch = Instant::now();
        let ping = Arc::clone(&last_ping);
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("process-sampler".into())
            .spawn(move || {
                let Ok(mut sys) = System::new() else {
                    return;
                };
                let logical = std::thread::available_parallelism().map_or(1, |n| n.get()) as f32;
                let kind = ProcessRefreshKind::nothing()
                    .with_cpu()
                    .with_memory()
                    .with_disk_usage()
                    .with_exe(UpdateKind::OnlyIfNotSet);
                let mut last: Option<Instant> = None;
                let mut ready = false;
                let mut disk_baseline = DiskBaseline::default();
                let windows_dir = toolboks_shared::win::windows_dir()
                    .ok()
                    .map(|dir| dir.display().to_string());
                loop {
                    std::thread::sleep(Duration::from_millis(250));
                    if Arc::strong_count(&ping) == 1 {
                        return;
                    }
                    let now_ms = epoch.elapsed().as_millis() as u64;
                    if !visible(ping.load(Ordering::Relaxed), now_ms) {
                        // The CPU baseline goes stale while hidden.
                        last = None;
                        ready = false;
                        disk_baseline = DiskBaseline::default();
                        continue;
                    }
                    let wait = if ready { REFRESH } else { PRIME };
                    if last.is_some_and(|t| t.elapsed() < wait) {
                        continue;
                    }
                    let sample_start = snapshot_start();
                    let secs = last.map_or(0.0, |t| t.elapsed().as_secs_f64());
                    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
                    let was_primed = last.is_some();
                    last = Some(Instant::now());
                    let owners = groups::window_owners();
                    let (mut procs, parent_pids): (Vec<Proc>, Vec<Option<u32>>) = sys
                        .processes()
                        .values()
                        .map(|p| {
                            let name = p.name().to_string_lossy().into_owned();
                            let pid = p.pid().as_u32();
                            let exe = p.exe().map(|e| e.display().to_string());
                            let kind = groups::kind_of(
                                exe.as_deref(),
                                owners.contains(&pid),
                                is_system(pid, &name),
                                windows_dir.as_deref(),
                            );
                            let u = p.disk_usage();
                            let identity = sampled_identity(p, sample_start);
                            let disk = disk_baseline.rate(
                                identity,
                                u.read_bytes.saturating_add(u.written_bytes),
                                secs,
                            );
                            let proc = Proc {
                                pid,
                                identity,
                                lower_name: name.to_lowercase(),
                                name,
                                cpu: (p.cpu_usage() / logical).clamp(0.0, 100.0),
                                mem: p.memory(),
                                disk,
                                gpu: None,
                                conns: None,
                                run_secs: p.run_time(),
                                exe,
                                parent: Parent::Unverified,
                                kind,
                                tree_parent: None,
                            };
                            (proc, p.parent().map(|parent| parent.as_u32()))
                        })
                        .unzip();
                    link_parents(&mut procs, &parent_pids);
                    let conns = crate::connections::count_per_pid().ok();
                    apply_conns(&mut procs, conns.as_ref());
                    let conns_total = conns.map(|m| m.values().sum());
                    disk_baseline.commit(procs.iter().filter_map(|p| p.identity));
                    ready = was_primed;
                    if tx
                        .send(Snapshot {
                            procs,
                            cpu_ready: ready,
                            conns_total,
                        })
                        .is_err()
                    {
                        return;
                    }
                    ctx.request_repaint();
                }
            });
        // If the thread cannot start, the channel just stays empty.
        let _ = spawned;
        Worker {
            rx,
            last_ping,
            epoch,
        }
    }

    fn ping(&self) {
        let now_ms = self.epoch.elapsed().as_millis() as u64;
        self.last_ping.store(now_ms + 1, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------------------------

/// A click on the table, applied once the table is drawn.
enum Click {
    Row(String, egui::Modifiers),
    Box(String),
    /// The open/close button of a merged row.
    Expand(String),
}

/// The visible lines and what they were built for: filter, sort column, direction, and the
/// count of open/close clicks so far.
type LinesCache = (String, SortKey, bool, u64, Arc<Vec<Line>>);

/// What the inspector asked for this frame.
enum Intent {
    AskEnd(Vec<ProcessIdentity>),
    ConfirmEnd,
    CancelEnd,
    AskLower(Vec<ProcessIdentity>),
    CancelLower,
    Lower,
    Restore,
    Copy(String),
    Open(String),
    /// Show this exe name on the History page.
    OpenHistory(String),
}

pub(super) struct Page {
    worker: Option<Worker>,
    live: Arc<Vec<Proc>>,
    cpu_ready: bool,
    /// Connections header total of the newest snapshot.
    conns_total: Option<u32>,
    frozen: Option<Arc<Vec<Proc>>>,
    rows: Option<Arc<Rows>>,
    line_cache: Option<LinesCache>,
    filter: String,
    /// The view picked in the toolbar, and the one the rows were built for.
    view: View,
    shown_view: View,
    /// Merged rows opened to show their processes, by row key. Kept across refreshes.
    expanded: HashSet<String>,
    /// Tree rows closed by hand, by row key (the tree starts fully open).
    collapsed: HashSet<String>,
    /// Bumped on every open or close, so the cached lines are rebuilt.
    expanded_gen: u64,
    sort_key: SortKey,
    descending: bool,
    selection: Selection,
    /// Process identities waiting for the second click on End task.
    confirm: Option<Vec<ProcessIdentity>>,
    /// The open "Lower priority" question, if any.
    lower_confirm: Option<Pending<ProcessIdentity>>,
    /// Previous priority class keyed by process identity.
    lowered: Vec<(ProcessIdentity, u32)>,
    denied: HashSet<ProcessIdentity>,
    job: Option<Receiver<Done>>,
    lines: Vec<String>,
    /// Newest per-pid GPU load from the main sampler; None until a reading with a GPU arrives.
    gpu: Option<std::collections::HashMap<u32, f32>>,
    /// Graphs of the single selected row; None when no single row is selected.
    own: Option<graphs::OwnGraphs>,
    /// An "Open in history" click, waiting for the add-on to switch pages.
    open_history: Option<String>,
}

#[cfg(test)]
impl Page {
    fn only_for_test(&mut self, key: &str) {
        self.selection.only(key);
    }
}

impl Page {
    pub(super) fn new() -> Self {
        Self {
            worker: None,
            live: Arc::default(),
            cpu_ready: false,
            conns_total: None,
            frozen: None,
            rows: None,
            line_cache: None,
            filter: String::new(),
            view: View::ByType,
            shown_view: View::ByType,
            expanded: HashSet::new(),
            collapsed: HashSet::new(),
            expanded_gen: 0,
            sort_key: SortKey::Cpu,
            descending: true,
            selection: Selection::default(),
            confirm: None,
            lower_confirm: None,
            lowered: Vec::new(),
            denied: HashSet::new(),
            job: None,
            lines: Vec::new(),
            gpu: None,
            own: None,
            open_history: None,
        }
    }

    /// The exe name of an "Open in history" click, once.
    pub(super) fn take_open_history(&mut self) -> Option<String> {
        self.open_history.take()
    }

    /// The key of the selected row when exactly one row is selected.
    fn single_key(&self) -> Option<String> {
        match self.selection.keys.len() {
            1 => self.selection.keys.iter().next().cloned(),
            _ => None,
        }
    }

    /// Starts, keeps or drops the selected row's graphs, and fills new ones with the current
    /// reading so they never start blank.
    fn follow_graphs(&mut self) {
        let key = self.single_key();
        graphs::follow(&mut self.own, key.as_deref());
        if let Some(own) = self.own.as_mut().filter(|g| g.is_empty()) {
            own.push(row_values(&self.live, own.key(), self.cpu_ready));
        }
    }

    /// Takes the newest per-process GPU reading (None when the PC has no usable GPU). It is
    /// applied to the next process snapshot, so a paused list keeps the values it froze with.
    pub(super) fn set_gpu(&mut self, by_pid: Option<&[(u32, f32)]>) {
        self.gpu = by_pid.map(|pids| pids.iter().copied().collect());
    }

    /// Takes in whatever the workers have produced. Never blocks.
    pub(super) fn tick(&mut self) {
        if let Some(worker) = &self.worker {
            for snap in worker.rx.try_iter() {
                let mut procs = snap.procs;
                apply_gpu(&mut procs, self.gpu.as_ref());
                self.live = Arc::new(procs);
                self.cpu_ready = snap.cpu_ready;
                self.conns_total = snap.conns_total;
                if self.frozen.is_none() {
                    self.rows = None;
                }
                if let Some(own) = self.own.as_mut() {
                    own.push(row_values(&self.live, own.key(), self.cpu_ready));
                }
            }
            if self.rows.is_none() && self.frozen.is_none() {
                let identities: HashSet<_> = self.live.iter().filter_map(|p| p.identity).collect();
                self.lowered.retain(|(id, _)| identities.contains(id));
                self.denied.retain(|id| identities.contains(id));
            }
        }
        let finished = self.job.as_ref().and_then(|rx| rx.try_recv().ok());
        if let Some(done) = finished {
            self.job = None;
            for (id, denied) in done.access {
                if denied {
                    self.denied.insert(id);
                } else {
                    self.denied.remove(&id);
                }
            }
            self.lowered.extend(done.lowered);
            self.lowered.retain(|(id, _)| !done.restored.contains(id));
            self.lines = done.lines;
        }
    }

    /// `totals` is None until the main sampler has produced a reading.
    pub(super) fn ui(&mut self, ui: &mut egui::Ui, totals: Option<Totals>) {
        let worker = self.worker.get_or_insert_with(|| Worker::start(ui.ctx()));
        worker.ping();

        if self.shown_view != self.view {
            self.shown_view = self.view;
            self.selection.clear();
            self.confirm = None;
            self.rows = None;
        }
        let own_pid = std::process::id();
        let protected = move |p: &Proc| is_protected(p.pid, &p.name, &p.parent, own_pid);
        let procs = Arc::clone(self.frozen.as_ref().unwrap_or(&self.live));
        if self.rows.is_none() {
            let rows = Rows::build(&procs, self.shown_view, &protected);
            let existing = rows.all().map(|r| r.key.clone()).collect();
            self.selection.prune(&existing);
            self.rows = Some(Arc::new(rows));
            self.line_cache = None;
        }
        let all_rows = self.rows.as_ref().unwrap().clone();

        let inspector = inspector_frame();
        let mut intent: Option<Intent> = None;
        let panel = egui::Panel::right("processes_inspector")
            .resizable(false)
            .exact_size(INSPECTOR_WIDTH)
            .frame(inspector)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        intent = self.inspector(ui, &procs, &all_rows, &protected);
                    });
            });
        let rect = panel.response.rect;
        toolboks_addon_sdk::tutorial::mark(ui, "inspector", rect);
        inspector_edge(ui, rect);

        let side = egui::Frame::NONE.inner_margin(CARD_PAD);
        egui::CentralPanel::default().frame(side).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.spacing_mut().button_padding = Vec2::new(6.0, 1.0);
            let lines = self.display_lines(&all_rows);
            let grouped = self.shown_view.grouped();
            let row_count = lines
                .iter()
                .filter(|l| matches!(l, Line::Row(r) if r.depth == 0 || !grouped))
                .count();
            toolboks_addon_sdk::tutorial::region(ui, "toolbar", |ui| {
                self.toolbar(ui, procs.len(), row_count);
            });
            ui.add_space(2.0);
            toolboks_addon_sdk::tutorial::region(ui, "table", |ui| {
                ui.scope(|ui| {
                    table_style(ui);
                    self.table(ui, &lines, totals);
                });
            });
        });

        if let Some(intent) = intent {
            self.apply(intent, ui.ctx(), &procs, &protected);
        }
        self.follow_graphs();
    }

    /// The processes behind the selected rows: a merged row stands for every process of its
    /// name, a process row (flat view, or opened under its group) for that one process.
    fn selected_procs(&self, procs: &[Proc]) -> Vec<Proc> {
        let grouped = self.shown_view.grouped();
        procs
            .iter()
            .filter(|p| {
                let keys = &self.selection.keys;
                (grouped && keys.contains(&row_key(true, p))) || keys.contains(&row_key(false, p))
            })
            .cloned()
            .collect()
    }

    fn apply(
        &mut self,
        intent: Intent,
        ctx: &egui::Context,
        procs: &[Proc],
        protected: &dyn Fn(&Proc) -> bool,
    ) {
        if self.frozen.is_some()
            && matches!(
                intent,
                Intent::AskEnd(_)
                    | Intent::ConfirmEnd
                    | Intent::AskLower(_)
                    | Intent::Lower
                    | Intent::Restore
            )
        {
            return;
        }
        match intent {
            Intent::AskEnd(pids) => {
                self.lower_confirm = None;
                self.confirm = Some(pids);
            }
            Intent::CancelEnd => self.confirm = None,
            Intent::AskLower(ids) => {
                self.confirm = None;
                self.lower_confirm = Pending::ask(ids);
            }
            Intent::CancelLower => self.lower_confirm = None,
            Intent::ConfirmEnd => {
                let (to_end, _) = plan_end(&self.selected_procs(procs), protected);
                if self
                    .confirm
                    .take()
                    .is_some_and(|ids| ids == action_identities(&to_end))
                {
                    self.start_job(ctx, Action::End(to_end));
                }
            }
            Intent::Lower => {
                let (list, _) = plan_end(&self.selected_procs(procs), protected);
                let asked = self.lower_confirm.take();
                if is_confirmed(asked.as_ref(), &action_identities(&list)) {
                    self.start_job(ctx, Action::Lower(list));
                }
            }
            Intent::Restore => {
                let list: Vec<(Proc, u32)> = self
                    .selected_procs(procs)
                    .into_iter()
                    .filter_map(|p| {
                        let identity = p.identity?;
                        let prev = self.lowered.iter().find(|(id, _)| *id == identity)?.1;
                        Some((p, prev))
                    })
                    .collect();
                self.start_job(ctx, Action::Restore(list));
            }
            Intent::Copy(text) => ctx.copy_text(text),
            Intent::Open(path) => open_location(&path),
            Intent::OpenHistory(name) => self.open_history = Some(name),
        }
    }

    fn start_job(&mut self, ctx: &egui::Context, action: Action) {
        if self.job.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("process-action".into())
            .spawn(move || {
                let _ = tx.send(run_action(action));
                ctx.request_repaint();
            });
        if spawned.is_ok() {
            self.job = Some(rx);
            self.lines.clear();
        } else {
            self.lines = vec!["Could not start the action".to_string()];
        }
    }

    fn display_lines(&mut self, rows: &Rows) -> Arc<Vec<Line>> {
        if let Some((filter, sort, descending, gen, lines)) = &self.line_cache {
            if filter == &self.filter
                && *sort == self.sort_key
                && *descending == self.descending
                && *gen == self.expanded_gen
            {
                return lines.clone();
            }
        }
        let lines = Arc::new(build_lines(
            rows,
            self.shown_view,
            if self.shown_view == View::Tree {
                &self.collapsed
            } else {
                &self.expanded
            },
            &self.filter,
            self.sort_key,
            self.descending,
        ));
        self.line_cache = Some((
            self.filter.clone(),
            self.sort_key,
            self.descending,
            self.expanded_gen,
            lines.clone(),
        ));
        lines
    }

    /// Filter, view, pause and count.
    fn toolbar(&mut self, ui: &mut egui::Ui, procs_total: usize, row_count: usize) {
        let mut paused = self.frozen.is_some();
        let was_paused = paused;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = SPACE_M;
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text("Filter by name or pid")
                    .desired_width(150.0),
            );
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                for view in View::ALL {
                    if pill(ui, view.label(), self.view == view).clicked() {
                        self.view = view;
                    }
                }
            });
            ui.toggle_value(&mut paused, if was_paused { "Paused" } else { "Pause" })
                .on_hover_text("Freeze the list so rows stop moving. Sampling carries on.");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let text = if self.shown_view.grouped() {
                    format!("{row_count} programs, {procs_total} processes")
                } else {
                    format!("{row_count} processes")
                };
                ui.label(RichText::new(text).weak());
            });
        });
        if paused != was_paused {
            self.frozen = paused.then(|| self.live.clone());
            self.rows = None;
            self.confirm = None;
        }
    }

    fn table(&mut self, ui: &mut egui::Ui, lines: &[Line], totals: Option<Totals>) {
        let mut sort_click: Option<SortKey> = None;
        let mut click: Option<Click> = None;
        let (sort_key, descending) = (self.sort_key, self.descending);
        let grouped = self.shown_view.grouped();
        let selection = &self.selection;
        let nested = self.shown_view != View::Flat;
        let ready = self.cpu_ready;
        let conns_total = self.conns_total;
        let mem_total = totals.map_or(0, |t| t.mem_total);
        let modifiers = ui.input(|i| i.modifiers);

        let mut table = TableBuilder::new(ui)
            .cell_layout(Layout::left_to_right(Align::Center))
            .striped(false)
            .sense(Sense::click())
            .auto_shrink([false, false])
            .column(Column::exact(22.0))
            .column(Column::remainder().at_least(90.0).clip(true));
        if !grouped {
            table = table.column(Column::exact(52.0));
        }
        // Sized so every column fits next to the inspector at the smallest window (1060 px).
        table
            .column(Column::exact(56.0))
            .column(Column::exact(76.0))
            .column(Column::exact(76.0))
            .column(Column::exact(52.0))
            .column(Column::exact(76.0))
            .header(HEADER_HEIGHT, |mut header| {
                header.col(|_| {});
                // The caption sorts; the system total sits under it, right-aligned, and is
                // left out until a first reading exists.
                let sortable = |ui: &mut egui::Ui, label: &str, key: SortKey, right: bool| {
                    let arrow = sort_arrow(sort_key == key, descending);
                    header_cell(ui, label, arrow, sort_key == key, right).then_some(key)
                };
                let with_total = |ui: &mut egui::Ui,
                                  label: &str,
                                  key: SortKey,
                                  total: Option<String>|
                 -> Option<SortKey> {
                    ui.with_layout(Layout::top_down(Align::Max), |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        let clicked = sortable(ui, label, key, false);
                        if let Some(total) = total {
                            ui.label(RichText::new(total).small().strong().color(MUTED));
                        }
                        clicked
                    })
                    .inner
                };
                header.col(|ui| {
                    sort_click = sortable(ui, "Name", SortKey::Name, false).or(sort_click)
                });
                if !grouped {
                    header.col(|ui| {
                        sort_click = sortable(ui, "PID", SortKey::Pid, true).or(sort_click)
                    });
                }
                header.col(|ui| {
                    let total = totals.map(|t| t.cpu_text());
                    sort_click = with_total(ui, "CPU", SortKey::Cpu, total).or(sort_click)
                });
                header.col(|ui| {
                    let total = totals.and_then(|t| t.mem_text());
                    sort_click = with_total(ui, "Memory", SortKey::Memory, total).or(sort_click)
                });
                header.col(|ui| {
                    let total = totals.map(|t| t.disk_text());
                    sort_click = with_total(ui, "Disk", SortKey::Disk, total).or(sort_click)
                });
                header.col(|ui| {
                    let total = totals.and_then(|t| t.gpu_text());
                    sort_click = with_total(ui, "GPU", SortKey::Gpu, total).or(sort_click)
                });
                header.col(|ui| {
                    let total = conns_total.map(|n| n.to_string());
                    sort_click =
                        with_total(ui, "Connections", SortKey::Connections, total).or(sort_click);
                    let cell = ui.response();
                    if cell.contains_pointer() {
                        cell.show_tooltip_text(CONNECTIONS_HINT);
                    }
                });
            })
            .body(|mut body| {
                let rule = body.ui_mut().painter().clone();
                body.rows(ROW_HEIGHT, lines.len(), |mut row| {
                    let r = match &lines[row.index()] {
                        Line::Section(kind, count) => {
                            // A caption across the row; it cannot be selected.
                            row.col(|_| {});
                            row.col(|ui| {
                                ui.label(
                                    RichText::new(format!("{} ({count})", kind.title())).strong(),
                                );
                            });
                            for _ in 0..data_columns(grouped) {
                                row.col(|_| {});
                            }
                            row_rule(&rule, row.response().rect);
                            return;
                        }
                        Line::Row(r) => r,
                    };
                    let is_selected = selection.keys.contains(&r.key);
                    row.set_selected(is_selected);
                    let cell = |ui: &mut egui::Ui, text: String| {
                        let l = body_cell(ui, text, true, r.protected);
                        if r.protected {
                            l.on_hover_text(protected_note(&r.name, &r.parent));
                        }
                    };
                    row.col(|ui| {
                        let mut on = is_selected;
                        if ui.checkbox(&mut on, "").changed() {
                            click = Some(Click::Box(r.key.clone()));
                        }
                    });
                    row.col(|ui| {
                        if nested {
                            // Room for the open/close button, so names line up; processes
                            // under an opened row sit one step further in per level.
                            let toggle = Vec2::new(TOGGLE_WIDTH, ROW_HEIGHT - 8.0);
                            let level = r.depth.min(MAX_INDENT_LEVEL);
                            if level > 0 {
                                ui.add_space(INDENT * f32::from(level));
                            }
                            if expandable(r) {
                                let open = r.open;
                                let label = RichText::new(if open { "-" } else { "+" }).strong();
                                let button = egui::Button::new(label).frame(false);
                                let hint = match (open, r.kids > 0) {
                                    (true, true) => "Hide the processes it started",
                                    (false, true) => "Show the processes it started",
                                    (true, false) => "Hide its processes",
                                    (false, false) => "Show its processes",
                                };
                                if ui.add_sized(toggle, button).on_hover_text(hint).clicked() {
                                    click = Some(Click::Expand(r.key.clone()));
                                }
                            } else {
                                // The button is followed by the usual gap; match it.
                                ui.add_space(toggle.x + ui.spacing().item_spacing.x);
                            }
                        }
                        let l = body_cell(ui, display_name(r), false, r.protected);
                        if r.protected {
                            l.on_hover_text(protected_note(&r.name, &r.parent));
                        }
                    });
                    if !grouped {
                        row.col(|ui| cell(ui, r.pid.to_string()));
                    }
                    let shaded = |ui: &mut egui::Ui, base: Color32, level: usize, text: String| {
                        shade_cell(ui, base, level);
                        cell(ui, text);
                    };
                    let cpu_level = if ready {
                        shade_level(f64::from(r.cpu), &CPU_SHADE)
                    } else {
                        0
                    };
                    let disk_level = r.disk.map_or(0, |d| shade_level(d, &DISK_SHADE));
                    let mem_level = shade_level(mem_share(r.mem, mem_total), &MEM_SHADE);
                    row.col(|ui| shaded(ui, DATA_CPU, cpu_level, cpu_text(r.cpu, ready)));
                    row.col(|ui| shaded(ui, DATA_MEMORY, mem_level, size_plain(r.mem)));
                    row.col(|ui| shaded(ui, DATA_DISK, disk_level, disk_text(r.disk)));
                    let gpu_level = r.gpu.map_or(0, |g| shade_level(f64::from(g), &GPU_SHADE));
                    row.col(|ui| shaded(ui, DATA_GPU, gpu_level, gpu_text(r.gpu)));
                    let conn_level = r
                        .conns
                        .map_or(0, |n| shade_level(f64::from(n), &CONN_SHADE));
                    row.col(|ui| shaded(ui, DATA_NETWORK, conn_level, conns_text(r.conns)));
                    let response = row.response();
                    row_rule(&rule, response.rect);
                    if response.clicked() && click.is_none() {
                        click = Some(Click::Row(r.key.clone(), modifiers));
                    }
                });
            });

        let visible: Vec<String> = match click {
            Some(Click::Row(..)) if modifiers.shift => lines
                .iter()
                .filter_map(|l| l.key().map(str::to_string))
                .collect(),
            _ => Vec::new(),
        };
        match click {
            Some(Click::Expand(key)) => {
                let set = if self.shown_view == View::Tree {
                    &mut self.collapsed
                } else {
                    &mut self.expanded
                };
                if !set.remove(&key) {
                    set.insert(key);
                }
                self.expanded_gen += 1;
            }
            Some(Click::Box(key)) => self.selection.toggle(&key),
            Some(Click::Row(key, m)) if m.shift => self.selection.range(&key, &visible),
            Some(Click::Row(key, m)) if m.command || m.ctrl => self.selection.toggle(&key),
            Some(Click::Row(key, _)) => self.selection.only(&key),
            None => {}
        }
        if let Some(key) = sort_click {
            if self.sort_key == key {
                self.descending = !self.descending;
            } else {
                self.sort_key = key;
                // Names and pids read naturally upwards, numbers biggest first.
                self.descending = matches!(
                    key,
                    SortKey::Cpu
                        | SortKey::Memory
                        | SortKey::Disk
                        | SortKey::Gpu
                        | SortKey::Connections
                );
            }
        }
    }

    fn inspector(
        &mut self,
        ui: &mut egui::Ui,
        procs: &[Proc],
        all_rows: &Rows,
        protected: &dyn Fn(&Proc) -> bool,
    ) -> Option<Intent> {
        ui.spacing_mut().item_spacing = Vec2::new(6.0, 4.0);
        let selected: Vec<&Row> = all_rows
            .all()
            .filter(|r| self.selection.keys.contains(&r.key))
            .collect();
        if selected.is_empty() {
            ui.label(RichText::new("Nothing selected.").weak());
            ui.label(
                RichText::new(
                    "Click a row. Ctrl+click adds or removes one, Shift+click selects a range.",
                )
                .weak()
                .small(),
            );
            self.result_lines(ui);
            return None;
        }

        let ready = self.cpu_ready;
        let title = match selected.as_slice() {
            [one] => display_name(one),
            many => format!("{} selected", many.len()),
        };
        ui.label(RichText::new(&title).size(TEXT_SECTION).strong());
        let proc_count: usize = selected.iter().map(|r| r.pids.len()).sum();
        let cpu: f32 = selected.iter().map(|r| r.cpu).sum();
        let mem: u64 = selected.iter().map(|r| r.mem).sum();
        if let [one] = selected.as_slice() {
            kv(ui, "Name", &one.name);
            if one.pids.len() > 1 {
                kv(ui, "Processes", &one.pids.len().to_string());
            } else {
                kv(ui, "PID", &one.pid.to_string());
            }
        } else {
            kv(ui, "Processes", &proc_count.to_string());
        }
        kv(ui, "CPU", &cpu_text(cpu, ready));
        kv(ui, "Memory", &size_plain(mem));
        let gpu: Option<f32> = selected
            .iter()
            .filter_map(|r| r.gpu)
            .reduce(|a, b| a + b)
            .map(|g| g.min(100.0));
        if let Some(gpu) = gpu {
            kv(ui, "GPU", &gpu_text(Some(gpu)));
        }
        let conns: Option<u32> = selected.iter().filter_map(|r| r.conns).reduce(|a, b| a + b);
        if let Some(conns) = conns {
            kv(ui, "Connections", &conns.to_string());
        }
        if let [one] = selected.as_slice() {
            kv(ui, "Running for", &uptime(one.run_secs));
            if let Some(path) = &one.exe {
                kv(ui, "Path", path);
            }
            if let Some(own) = self.own.as_ref().filter(|g| g.key() == one.key) {
                ui.add_space(4.0);
                graphs::ui(ui, own);
            }
        }

        let chosen = self.selected_procs(procs);
        if chosen
            .iter()
            .any(|p| p.identity.is_some_and(|id| self.denied.contains(&id)))
        {
            ui.label(
                RichText::new("Windows denied a previous action for this selection.")
                    .weak()
                    .small(),
            );
        }
        let (to_end, skipped) = plan_end(&chosen, protected);
        if to_end.is_empty() {
            let note = if chosen.iter().any(|p| p.identity.is_none() && !protected(p)) {
                "Windows could not verify the selected processes."
            } else {
                chosen
                    .first()
                    .map_or(PROTECTED_NOTE, |p| protected_note(&p.name, &p.parent))
            };
            ui.label(RichText::new(note).weak().small());
        } else if !skipped.is_empty() {
            ui.label(
                RichText::new(format!(
                    "{} of {} are protected or could not be verified.",
                    skipped.len(),
                    chosen.len()
                ))
                .weak()
                .small(),
            );
        }
        ui.separator();

        let paused = self.frozen.is_some();
        if paused {
            ui.label(
                RichText::new("Resume to change running processes.")
                    .weak()
                    .small(),
            );
        }
        let busy = self.job.is_some() || paused;
        let mut intent = None;
        let end_pids = action_identities(&to_end);
        if self.confirm.as_ref().is_some_and(|c| *c != end_pids) {
            self.confirm = None;
        }
        let label = match selected.as_slice() {
            [one] => display_name(one)
                .split(" (")
                .next()
                .unwrap_or("")
                .to_string(),
            many => format!("{} apps", many.len()),
        };
        if self.confirm.is_some() && !end_pids.is_empty() {
            ui.label(RichText::new(confirm_text(&label, end_pids.len())).small());
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        !busy,
                        egui::Button::new(RichText::new("End task").color(RED)),
                    )
                    .clicked()
                {
                    intent = Some(Intent::ConfirmEnd);
                }
                if ui.button("Cancel").clicked() {
                    intent = Some(Intent::CancelEnd);
                }
            });
        } else if ui
            .add_enabled(!end_pids.is_empty() && !busy, egui::Button::new("End task"))
            .clicked()
        {
            intent = Some(Intent::AskEnd(end_pids.clone()));
        }

        let can_restore = chosen
            .iter()
            .any(|p| self.lowered.iter().any(|(id, _)| Some(*id) == p.identity));
        let lower_allowed = !paused && !can_restore && self.confirm.is_none();
        self.lower_confirm = settle(self.lower_confirm.take(), &end_pids, lower_allowed);
        if can_restore {
            if ui
                .add_enabled(!busy, egui::Button::new("Restore priority"))
                .clicked()
            {
                intent = Some(Intent::Restore);
            }
        } else if self.lower_confirm.is_some() {
            ui.label(RichText::new(lower_text(&label, end_pids.len())).small());
            ui.horizontal(|ui| {
                let cancel = ui.button("Cancel");
                if self
                    .lower_confirm
                    .as_mut()
                    .is_some_and(Pending::take_cancel_focus)
                {
                    cancel.request_focus();
                }
                if cancel.clicked() {
                    intent = Some(Intent::CancelLower);
                }
                if ui
                    .add_enabled(!busy, egui::Button::new("Lower priority"))
                    .clicked()
                {
                    intent = Some(Intent::Lower);
                }
            });
        } else if ui
            .add_enabled(
                !to_end.is_empty() && !busy,
                egui::Button::new("Lower priority"),
            )
            .on_hover_text("Lets other apps use the processor first. Undo it here any time.")
            .clicked()
        {
            intent = Some(Intent::AskLower(end_pids.clone()));
        }
        if ui.button("Copy").clicked() {
            intent = Some(Intent::Copy(copy_text(&selected, ready)));
        }
        if let [one] = selected.as_slice() {
            if let Some(path) = &one.exe {
                if ui.button("Open file location").clicked() {
                    intent = Some(Intent::Open(path.clone()));
                }
            }
            if ui
                .button("Open in history")
                .on_hover_text("See what this app used over time on the History page")
                .clicked()
            {
                intent = Some(Intent::OpenHistory(one.name.clone()));
            }
        }
        if self.job.is_some() {
            ui.label(RichText::new("Working...").weak().small());
        }
        ui.add_space(GAP - 2.0);
        self.result_lines(ui);
        intent
    }

    fn result_lines(&self, ui: &mut egui::Ui) {
        for line in self.lines.iter().take(MAX_LINES) {
            ui.label(RichText::new(line).small());
        }
        if self.lines.len() > MAX_LINES {
            ui.label(
                RichText::new(format!("and {} more", self.lines.len() - MAX_LINES))
                    .weak()
                    .small(),
            );
        }
    }
}

/// A small caption over a value.
pub(super) fn kv(ui: &mut egui::Ui, key: &str, value: &str) {
    ui.spacing_mut().item_spacing.y = 0.0;
    ui.label(RichText::new(key).small().weak());
    ui.add(egui::Label::new(value).wrap());
    ui.spacing_mut().item_spacing.y = 4.0;
}

/// A child that waits on its stdin, for the tests that act on a real process.
///
/// The path must use backslashes only. With `System32/cmd.exe` (a forward slash) `cmd` reads
/// part of its own path as a switch, prints "The syntax of the command is incorrect." and exits
/// about 15 ms after it starts. A test that finished inside that window passed; a test that was
/// slowed down by a busy PC lost the race (issue #99).
#[cfg(test)]
pub(super) fn spawn_idle_child() -> std::process::Child {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    let cmd = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
        .join("System32")
        .join("cmd.exe");
    let mut child = Command::new(cmd)
        .args(["/d", "/q", "/c", "set /p toolboks_test_wait="])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(0x0800_0000)
        .spawn()
        .unwrap();
    // Say so at once if the child is not waiting, instead of failing later in a sampling step.
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        child.try_wait().unwrap().is_none(),
        "the test child exited right after it was started"
    );
    child
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: u32, name: &str, cpu: f32, mem: u64) -> Proc {
        Proc {
            pid,
            identity: Some(ProcessIdentity {
                pid,
                created: u64::from(pid),
            }),
            name: name.to_string(),
            lower_name: name.to_lowercase(),
            cpu,
            mem,
            disk: None,
            gpu: None,
            conns: None,
            run_secs: pid as u64,
            exe: None,
            parent: ok(),
            kind: Kind::Background,
            tree_parent: None,
        }
    }

    fn of_kind(mut p: Proc, kind: Kind) -> Proc {
        p.kind = kind;
        p
    }

    /// Chrome (an app through its window process), a background updater, and two Windows
    /// processes, one of them merged from two svchost processes.
    fn typed() -> Vec<Proc> {
        vec![
            of_kind(p(10, "chrome.exe", 1.0, 100), Kind::App),
            of_kind(p(11, "chrome.exe", 4.0, 300), Kind::Background),
            of_kind(p(20, "updater.exe", 2.0, 50), Kind::Background),
            of_kind(p(30, "svchost.exe", 0.5, 20), Kind::Windows),
            of_kind(p(31, "svchost.exe", 3.0, 10), Kind::Windows),
            of_kind(p(40, "lsass.exe", 0.1, 5), Kind::Windows),
        ]
    }

    fn line_names(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| match l {
                Line::Section(kind, n) => format!("# {} ({n})", kind.title()),
                Line::Row(r) if r.depth > 0 => format!("  {}", r.pid),
                Line::Row(r) => r.lower_name.clone(),
            })
            .collect()
    }

    #[test]
    fn a_merged_row_is_an_app_when_any_of_its_processes_is() {
        let rows = build_rows(&typed(), true, &none);
        let kind = |name: &str| rows.iter().find(|r| r.lower_name == name).unwrap().kind;
        assert_eq!(kind("chrome.exe"), Kind::App);
        assert_eq!(kind("updater.exe"), Kind::Background);
        assert_eq!(kind("svchost.exe"), Kind::Windows);
        // The flat rows keep each process's own section.
        let flat = build_rows(&typed(), false, &none);
        assert_eq!(
            flat.iter().find(|r| r.pid == 11).unwrap().kind,
            Kind::Background
        );
    }

    /// A process the refresh saw with `parent` as its parent pid, born at `created`.
    fn child(pid: u32, name: &str, created: u64, cpu: f32) -> Proc {
        Proc {
            identity: Some(ProcessIdentity { pid, created }),
            cpu,
            ..p(pid, name, cpu, 0)
        }
    }

    fn tree_names(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| match l {
                Line::Row(r) => format!("{}{}", "  ".repeat(r.depth as usize), r.lower_name),
                Line::Section(..) => unreachable!("the tree has no sections"),
            })
            .collect()
    }

    /// explorer (1) started chrome (2, 3) and code (4); chrome 2 started its helper 5.
    fn family() -> Vec<Proc> {
        let mut procs = vec![
            child(1, "explorer.exe", 10, 1.0),
            child(2, "chrome.exe", 20, 5.0),
            child(3, "chrome.exe", 21, 9.0),
            child(4, "code.exe", 22, 2.0),
            child(5, "helper.exe", 30, 0.5),
            child(6, "lonely.exe", 5, 0.1),
        ];
        link_parents(
            &mut procs,
            &[None, Some(1), Some(1), Some(1), Some(2), None],
        );
        procs
    }

    #[test]
    fn the_tree_nests_children_and_sorts_siblings_by_the_column() {
        let rows = Rows::build(&family(), View::Tree, &none);
        let none_closed = HashSet::new();
        let lines = tree_lines(&rows.top, &none_closed, "", SortKey::Cpu, true);
        assert_eq!(
            tree_names(&lines),
            [
                "explorer.exe",
                "  chrome.exe",
                "  chrome.exe",
                "    helper.exe",
                "  code.exe",
                "lonely.exe",
            ]
        );
        // Busiest sibling first: chrome 3 (9 %) before chrome 2 (5 %) before code (2 %).
        let pids: Vec<u32> = lines
            .iter()
            .filter_map(|l| match l {
                Line::Row(r) => Some(r.pid),
                Line::Section(..) => None,
            })
            .collect();
        assert_eq!(pids, [1, 3, 2, 5, 4, 6]);
        // By name, ascending, the order of siblings follows.
        let lines = tree_lines(&rows.top, &none_closed, "", SortKey::Name, false);
        assert_eq!(
            tree_names(&lines)[1..4],
            ["  chrome.exe", "    helper.exe", "  chrome.exe"]
        );
        // Closing explorer hides everything under it; it still says it has children.
        let closed: HashSet<String> = [rows.top[0].key.clone()].into();
        let lines = tree_lines(&rows.top, &closed, "", SortKey::Cpu, true);
        assert_eq!(tree_names(&lines), ["explorer.exe", "lonely.exe"]);
        match &lines[0] {
            Line::Row(r) => assert!(expandable(r) && !r.open && r.kids == 3),
            Line::Section(..) => unreachable!(),
        }
    }

    #[test]
    fn a_reused_parent_pid_puts_the_process_at_the_top() {
        // Pid 1 was explorer, which started 7 at time 15; explorer exited and a new process
        // (started at 40) got pid 1. It is not 7's parent.
        let mut procs = vec![
            child(1, "newcomer.exe", 40, 0.0),
            child(7, "orphan.exe", 15, 0.0),
        ];
        link_parents(&mut procs, &[None, Some(1)]);
        assert_eq!(procs[1].tree_parent, None);
        let rows = Rows::build(&procs, View::Tree, &none);
        let lines = tree_lines(&rows.top, &HashSet::new(), "", SortKey::Name, false);
        assert_eq!(tree_names(&lines), ["newcomer.exe", "orphan.exe"]);
        // An unknown start time on either side proves nothing either.
        let mut procs = vec![born(1, "explorer.exe", None), child(7, "app.exe", 15, 0.0)];
        link_parents(&mut procs, &[None, Some(1)]);
        assert_eq!(procs[1].tree_parent, None);
        // The genuine case: the parent started first.
        let procs = family();
        assert_eq!(procs[4].tree_parent, Some(2));
    }

    #[test]
    fn the_tree_filter_keeps_the_ancestors_of_a_match() {
        let rows = Rows::build(&family(), View::Tree, &none);
        let lines = tree_lines(&rows.top, &HashSet::new(), "helper", SortKey::Cpu, true);
        assert_eq!(
            tree_names(&lines),
            ["explorer.exe", "  chrome.exe", "    helper.exe"]
        );
        // A pid matches too; a match with no parent stands alone.
        let lines = tree_lines(&rows.top, &HashSet::new(), "6", SortKey::Cpu, true);
        assert_eq!(tree_names(&lines), ["lonely.exe"]);
        // Matching a parent and its child keeps both once.
        let lines = tree_lines(&rows.top, &HashSet::new(), "chrome", SortKey::Cpu, true);
        assert_eq!(
            tree_names(&lines),
            ["explorer.exe", "  chrome.exe", "  chrome.exe"]
        );
        assert!(tree_lines(&rows.top, &HashSet::new(), "zzz", SortKey::Cpu, true).is_empty());
    }

    #[test]
    fn the_tree_view_shows_pids_and_selects_single_processes() {
        assert!(!View::Tree.grouped());
        let rows = Rows::build(&family(), View::Tree, &none);
        assert!(rows.children.is_empty());
        assert!(rows
            .top
            .iter()
            .all(|r| r.key.starts_with("pid:") && !r.show_pid));
        let mut page = Page::new();
        page.shown_view = View::Tree;
        page.only_for_test(&rows.top[1].key);
        let pids: Vec<u32> = page
            .selected_procs(&family())
            .iter()
            .map(|p| p.pid)
            .collect();
        assert_eq!(pids, [2]);
    }

    #[test]
    fn the_selected_rows_graphs_follow_the_selection_and_read_its_processes() {
        let mut page = Page::new();
        page.live = Arc::new(typed());
        page.cpu_ready = true;
        page.only_for_test("chrome.exe");
        page.follow_graphs();
        let own = page.own.as_ref().expect("one row selected: graphs");
        assert_eq!(own.key(), "chrome.exe");
        // Two chrome processes: 1 + 4 % CPU, 100 + 300 bytes.
        let v = row_values(&page.live, "chrome.exe", true);
        assert_eq!((v[0], v[1]), (5.0, 400.0));
        let one = row_key(false, &page.live[1]);
        let v = row_values(&page.live, &one, true);
        assert_eq!((v[0], v[1]), (4.0, 300.0));
        // Not ready yet, unknown disk, a row whose processes exited: gaps, not zeros.
        assert!(row_values(&page.live, "chrome.exe", false)[0].is_nan());
        assert!(row_values(&page.live, "chrome.exe", true)[2].is_nan());
        assert!(row_values(&page.live, "gone.exe", true)
            .iter()
            .all(|v| v.is_nan()));
        // Two rows selected, or none: nothing is kept.
        page.selection.toggle("updater.exe");
        page.follow_graphs();
        assert!(page.own.is_none());
        page.selection.clear();
        page.follow_graphs();
        assert!(page.own.is_none());
    }

    #[test]
    fn open_in_history_hands_the_exe_name_over_once() {
        let mut page = Page::new();
        let ctx = egui::Context::default();
        page.apply(Intent::OpenHistory("chrome.exe".into()), &ctx, &[], &none);
        assert_eq!(page.take_open_history().as_deref(), Some("chrome.exe"));
        assert_eq!(page.take_open_history(), None);
    }

    #[test]
    fn system_names_are_windows_processes_but_toolboks_is_not() {
        assert!(is_system(4, "System"));
        assert!(is_system(900, "csrss.exe"));
        assert!(is_system(900, "Memory Compression"));
        assert!(!is_system(900, "Toolboks.exe"));
        assert!(!is_system(900, "chrome.exe"));
    }

    #[test]
    fn by_type_puts_rows_under_sections_in_task_manager_order() {
        let rows = Rows::build(&typed(), View::ByType, &none);
        let lines = build_lines(&rows, View::ByType, &HashSet::new(), "", SortKey::Cpu, true);
        assert_eq!(
            line_names(&lines),
            [
                "# Apps (1)",
                "chrome.exe",
                "# Background processes (1)",
                "updater.exe",
                "# Windows processes (2)",
                "svchost.exe",
                "lsass.exe",
            ]
        );
        // An empty section is left out; the filter counts what is left.
        let lines = build_lines(
            &rows,
            View::ByType,
            &HashSet::new(),
            "svc",
            SortKey::Cpu,
            true,
        );
        assert_eq!(
            line_names(&lines),
            ["# Windows processes (1)", "svchost.exe"]
        );
    }

    #[test]
    fn an_opened_group_lists_its_processes_sorted_under_it() {
        let rows = Rows::build(&typed(), View::ByType, &none);
        let open: HashSet<String> = ["chrome.exe".to_string()].into();
        let lines = build_lines(&rows, View::ByType, &open, "", SortKey::Cpu, true);
        assert_eq!(
            line_names(&lines)[..4],
            ["# Apps (1)", "chrome.exe", "  11", "  10"]
        );
        let child = match &lines[2] {
            Line::Row(r) => r,
            Line::Section(..) => unreachable!(),
        };
        assert!(child.key.starts_with("pid:11:"));
        assert!(!expandable(child));
        assert_eq!(display_name(child), "Chrome (pid 11)");
        // By name: same rows and opening, no sections.
        let rows = Rows::build(&typed(), View::ByName, &none);
        let lines = build_lines(&rows, View::ByName, &open, "", SortKey::Name, false);
        assert_eq!(
            line_names(&lines),
            [
                "chrome.exe",
                "  10",
                "  11",
                "lsass.exe",
                "svchost.exe",
                "updater.exe"
            ]
        );
        // A single process has nothing to open; the flat view has no merged rows at all.
        assert!(!expandable(&rows.top[1]));
        let flat = Rows::build(&typed(), View::Flat, &none);
        assert!(flat.children.is_empty());
        let lines = build_lines(&flat, View::Flat, &open, "", SortKey::Pid, false);
        assert_eq!(lines.len(), 6);
        assert!(lines
            .iter()
            .all(|l| matches!(l, Line::Row(r) if r.depth == 0)));
    }

    #[test]
    fn a_process_row_selects_one_process_and_a_group_row_all_of_them() {
        let mut page = Page::new();
        let procs = typed();
        page.only_for_test("chrome.exe");
        let pids: Vec<u32> = page.selected_procs(&procs).iter().map(|p| p.pid).collect();
        assert_eq!(pids, [10, 11]);
        let one = row_key(false, &procs[1]);
        page.only_for_test(&one);
        let pids: Vec<u32> = page.selected_procs(&procs).iter().map(|p| p.pid).collect();
        assert_eq!(pids, [11]);
        // In the flat view a name key means nothing.
        page.shown_view = View::Flat;
        page.only_for_test("chrome.exe");
        assert!(page.selected_procs(&procs).is_empty());
    }

    /// Every text drawn in one frame, with where it was drawn.
    fn frame_texts(ctx: &egui::Context, page: &mut Page, width: f32) -> Vec<(String, egui::Rect)> {
        fn walk(shape: &egui::Shape, out: &mut Vec<(String, egui::Rect)>) {
            match shape {
                egui::Shape::Text(t) => out.push((
                    t.galley.text().to_owned(),
                    t.galley.rect.translate(t.pos.to_vec2()),
                )),
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, out)),
                _ => {}
            }
        }
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width, 720.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| page.ui(ui, None));
        output.textures_delta.clear();
        let mut out = Vec::new();
        for clipped in &output.shapes {
            walk(&clipped.shape, &mut out);
        }
        out
    }

    /// The page at the smallest window: the add-on gets 820 points next to the open sidebar
    /// (DESIGN.md). Every column header must be left of the inspector, the sections must show,
    /// and the guided tour must still find the toolbar, table and inspector.
    #[test]
    fn the_page_fits_the_smallest_window_and_the_tour_finds_its_regions() {
        const CONTENT: f32 = 820.0;
        let ctx = egui::Context::default();
        toolboks_shared::ui::apply_style(&ctx);
        let mut page = Page::new();
        page.live = Arc::new(typed());
        page.cpu_ready = true;
        for view in [View::ByType, View::Tree, View::Flat] {
            page.view = view;
            let mut texts = Vec::new();
            for _ in 0..3 {
                texts = frame_texts(&ctx, &mut page, CONTENT);
            }
            let find = |text: &str| {
                texts
                    .iter()
                    .find(|(t, _)| t.starts_with(text))
                    .map(|(_, r)| *r)
                    .unwrap_or_else(|| panic!("{text:?} not drawn in {view:?}"))
            };
            let inspector_left = CONTENT - INSPECTOR_WIDTH;
            for header in ["Name", "CPU", "Memory", "Disk", "GPU", "Connections"] {
                let rect = find(header);
                assert!(
                    rect.right() <= inspector_left,
                    "{header} {rect:?} in {view:?}"
                );
            }
            let count = find(if view.grouped() {
                "4 programs"
            } else {
                "6 processes"
            });
            assert!(count.right() <= inspector_left, "{count:?}");
            if view == View::ByType {
                find("Apps (1)");
                // Names line up whether or not the row has an open button in front.
                let merged = find("Chrome (2)");
                let single = find("Updater");
                assert!(
                    (merged.left() - single.left()).abs() < 0.5,
                    "{merged:?} {single:?}"
                );
                find("Background processes (1)");
                find("Windows processes (2)");
            } else {
                find("PID");
            }
        }
        let mut anchors = Vec::new();
        for target in ["toolbar", "table", "inspector"] {
            toolboks_addon_sdk::tutorial::prepare(&ctx, Some(target), false);
            frame_texts(&ctx, &mut page, CONTENT);
            let rect = toolboks_addon_sdk::tutorial::anchor(&ctx)
                .unwrap_or_else(|| panic!("tour target {target} not marked"));
            anchors.push(rect);
        }
        let [toolbar, table, inspector] = anchors[..] else {
            unreachable!()
        };
        assert!(
            toolbar.bottom() <= table.top() + 1.0,
            "{toolbar:?} {table:?}"
        );
        assert!(
            inspector.left() >= table.right() - 1.0,
            "{inspector:?} {table:?}"
        );
    }

    /// A parent the refresh vouched for that protects nothing.
    fn ok() -> Parent {
        Parent::known("explorer.exe")
    }

    fn none(_: &Proc) -> bool {
        false
    }

    fn sample() -> Vec<Proc> {
        vec![
            p(10, "chrome.exe", 1.0, 100),
            p(11, "Chrome.exe", 2.0, 200),
            p(20, "code.exe", 5.0, 50),
            p(4, "System", 0.5, 10),
        ]
    }

    #[test]
    fn grouping_sums_and_counts() {
        let rows = build_rows(&sample(), true, &none);
        assert_eq!(rows.len(), 3);
        let chrome = &rows[0];
        assert_eq!(chrome.pids, vec![10, 11]);
        assert_eq!(chrome.pid, 10);
        assert_eq!(chrome.mem, 300);
        assert!((chrome.cpu - 3.0).abs() < 1e-6);
        assert_eq!(display_name(chrome), "Chrome (2)");
        assert_eq!(build_rows(&sample(), false, &none).len(), 4);
    }

    fn with_disk(mut p: Proc, disk: f64) -> Proc {
        p.disk = Some(disk);
        p
    }

    #[test]
    fn grouping_sums_disk() {
        let procs = vec![
            with_disk(p(10, "chrome.exe", 1.0, 100), 1_500_000.0),
            with_disk(p(11, "Chrome.exe", 2.0, 200), 500_000.0),
            with_disk(p(20, "code.exe", 5.0, 50), 10.0),
        ];
        let grouped = build_rows(&procs, true, &none);
        assert_eq!(grouped[0].disk, Some(2_000_000.0));
        assert_eq!(grouped[1].disk, Some(10.0));
        let flat = build_rows(&procs, false, &none);
        assert_eq!(flat.len(), 3);
        assert_eq!(flat[0].disk, Some(1_500_000.0));
    }

    fn with_gpu(mut p: Proc, gpu: f32) -> Proc {
        p.gpu = Some(gpu);
        p
    }

    #[test]
    fn grouping_sums_gpu_like_the_overview_card() {
        // The same processes the Overview GPU card gets from the sampler.
        let readings = [
            (10, "game.exe", 70.0),
            (11, "Game.exe", 45.0),
            (20, "chrome.exe", 3.5),
            (21, "chrome.exe", 1.0),
            (30, "code.exe", 0.4),
        ];
        let mut procs: Vec<Proc> = readings
            .iter()
            .map(|(pid, name, _)| p(*pid, name, 0.0, 0))
            .chain([p(40, "notepad.exe", 0.0, 0)])
            .collect();
        let by_pid: std::collections::HashMap<u32, f32> =
            readings.iter().map(|(pid, _, g)| (*pid, *g)).collect();
        apply_gpu(&mut procs, Some(&by_pid));
        let rows = build_rows(&procs, true, &none);
        let card =
            crate::sampler::gpu_by_name(readings.iter().map(|(_, n, g)| (n.to_string(), *g)));
        assert_eq!(card.len(), 3);
        for top in &card {
            let row = rows
                .iter()
                .find(|r| r.lower_name == top.name.to_lowercase())
                .unwrap();
            assert_eq!(row.gpu, Some(top.value as f32), "{}", top.name);
            assert_eq!(row.pids.len(), top.count, "{}", top.name);
        }
        // Capped at 100 % like the card; a process missing from the reading uses no GPU.
        assert_eq!(rows[0].gpu, Some(100.0));
        let notepad = rows.iter().find(|r| r.lower_name == "notepad.exe").unwrap();
        assert_eq!(notepad.gpu, Some(0.0));
        // No GPU reading: unknown, not 0.
        apply_gpu(&mut procs, None);
        assert!(build_rows(&procs, true, &none)
            .iter()
            .all(|r| r.gpu.is_none()));
    }

    #[test]
    fn connections_are_counted_per_process_and_summed_per_group() {
        let mut procs = vec![
            p(10, "chrome.exe", 0.0, 0),
            p(11, "Chrome.exe", 0.0, 0),
            p(20, "code.exe", 0.0, 0),
            p(30, "notepad.exe", 0.0, 0),
        ];
        let by_pid: std::collections::HashMap<u32, u32> =
            [(10, 12), (11, 3), (20, 1), (999, 40)].into();
        apply_conns(&mut procs, Some(&by_pid));
        let rows = build_rows(&procs, true, &none);
        let get = |name: &str| rows.iter().find(|r| r.lower_name == name).unwrap().conns;
        assert_eq!(get("chrome.exe"), Some(15));
        assert_eq!(get("code.exe"), Some(1));
        // Not in the tables: no connections, not unknown.
        assert_eq!(get("notepad.exe"), Some(0));
        let mut flat = build_rows(&procs, false, &none);
        sort_rows(&mut flat, SortKey::Connections, true);
        let order: Vec<u32> = flat.iter().map(|r| r.pid).collect();
        assert_eq!(order, vec![10, 11, 20, 30]);
        // The tables could not be read: unknown everywhere.
        apply_conns(&mut procs, None);
        assert!(build_rows(&procs, true, &none)
            .iter()
            .all(|r| r.conns.is_none()));
        assert_eq!(conns_text(None), "...");
        assert_eq!(conns_text(Some(7)), "7");
        assert_eq!(shade_level(4.0, &CONN_SHADE), 0);
        assert_eq!(shade_level(20.0, &CONN_SHADE), 2);
        assert_eq!(shade_level(500.0, &CONN_SHADE), 4);
        // The hover says plainly it is a count, not traffic.
        assert!(CONNECTIONS_HINT.contains("not traffic"));
    }

    #[test]
    fn sorts_by_gpu_and_shades_like_cpu() {
        let procs = vec![
            with_gpu(p(3, "b.exe", 1.0, 5), 12.0),
            p(2, "a.exe", 1.0, 5),
            with_gpu(p(1, "c.exe", 1.0, 5), 80.0),
        ];
        let mut rows = build_rows(&procs, false, &none);
        sort_rows(&mut rows, SortKey::Gpu, true);
        let order: Vec<u32> = rows.iter().map(|r| r.pid).collect();
        assert_eq!(order, vec![1, 3, 2]);
        assert_eq!(gpu_text(None), "...");
        assert_eq!(gpu_text(Some(12.04)), "12.0%");
        assert_eq!(shade_level(0.5, &GPU_SHADE), 0);
        assert_eq!(shade_level(12.0, &GPU_SHADE), 2);
        assert_eq!(shade_level(80.0, &GPU_SHADE), 4);
    }

    #[test]
    fn sorts_by_disk_with_name_ties() {
        let procs = vec![
            with_disk(p(3, "b.exe", 1.0, 5), 10.0),
            with_disk(p(2, "a.exe", 1.0, 5), 10.0),
            with_disk(p(1, "c.exe", 1.0, 5), 900.0),
        ];
        let mut rows = build_rows(&procs, false, &none);
        sort_rows(&mut rows, SortKey::Disk, true);
        let order: Vec<u32> = rows.iter().map(|r| r.pid).collect();
        assert_eq!(order, vec![1, 2, 3]);
        sort_rows(&mut rows, SortKey::Disk, false);
        let order: Vec<u32> = rows.iter().map(|r| r.pid).collect();
        assert_eq!(order, vec![2, 3, 1]);
    }

    #[test]
    fn shade_levels_follow_fixed_steps() {
        // Below the first step there is no tint; each step reached adds one level, at most 4.
        assert_eq!(shade_level(0.0, &CPU_SHADE), 0);
        assert_eq!(shade_level(0.99, &CPU_SHADE), 0);
        assert_eq!(shade_level(1.0, &CPU_SHADE), 1);
        assert_eq!(shade_level(9.9, &CPU_SHADE), 1);
        assert_eq!(shade_level(10.0, &CPU_SHADE), 2);
        assert_eq!(shade_level(25.0, &CPU_SHADE), 3);
        assert_eq!(shade_level(50.0, &CPU_SHADE), 4);
        assert_eq!(shade_level(100.0, &CPU_SHADE), 4);
        // Disk: 0.1 / 1 / 10 / 100 MB/s.
        assert_eq!(shade_level(99_999.0, &DISK_SHADE), 0);
        assert_eq!(shade_level(100_000.0, &DISK_SHADE), 1);
        assert_eq!(shade_level(1_000_000.0, &DISK_SHADE), 2);
        assert_eq!(shade_level(10_000_000.0, &DISK_SHADE), 3);
        assert_eq!(shade_level(100_000_000.0, &DISK_SHADE), 4);
        // Memory: share of the installed RAM; unknown total never shades.
        let gb = 1024 * 1024 * 1024;
        assert_eq!(shade_level(mem_share(gb, 16 * gb), &MEM_SHADE), 2);
        assert_eq!(shade_level(mem_share(8 * gb, 16 * gb), &MEM_SHADE), 4);
        assert_eq!(shade_level(mem_share(8 * gb, 0), &MEM_SHADE), 0);
    }

    #[test]
    fn shade_tint_is_transparent_at_level_zero_and_grows() {
        assert_eq!(shade_color(DATA_DISK, 0).a(), 0);
        let alphas: Vec<u8> = (0..=SHADE_STEPS)
            .map(|l| shade_color(DATA_DISK, l).a())
            .collect();
        assert!(alphas.windows(2).all(|w| w[0] < w[1]));
        // A level past the top is clamped instead of panicking.
        assert_eq!(shade_color(DATA_CPU, 9).a(), alphas[SHADE_STEPS]);
    }

    #[test]
    fn header_totals_text() {
        let t = Totals {
            cpu: 34.4,
            mem_used: 8 * 1024 * 1024 * 1024,
            mem_total: 16 * 1024 * 1024 * 1024,
            disk_active: Some(12.6),
            disk_bps: 5_000_000.0,
            gpu: Some(41.6),
        };
        assert_eq!(t.gpu_text().as_deref(), Some("42%"));
        assert_eq!(Totals { gpu: None, ..t }.gpu_text(), None);
        assert_eq!(t.cpu_text(), "34%");
        assert_eq!(t.mem_text().as_deref(), Some("50%"));
        assert_eq!(t.disk_text(), "13%");
        let no_counter = Totals {
            disk_active: None,
            ..t
        };
        assert_eq!(no_counter.disk_text(), "4.8 MB/s");
        // Unknown RAM total: no total at all, not "0%".
        let unknown = Totals { mem_total: 0, ..t };
        assert_eq!(unknown.mem_text(), None);
    }

    #[test]
    fn disk_cell_waits_for_the_baseline() {
        assert_eq!(disk_text(None), "...");
        assert_eq!(disk_text(Some(0.0)), "0 B/s");
    }

    #[test]
    fn disk_rate_needs_a_previous_sighting() {
        let id = |pid| {
            Some(ProcessIdentity {
                pid,
                created: u64::from(pid),
            })
        };
        let mut base = DiskBaseline::default();
        // First refresh: nothing was seen before, so no rate.
        assert_eq!(base.rate(id(1), 4_000_000, 2.0), None);
        base.commit([1].into_iter().filter_map(id));
        // Second refresh: pid 1 gets a rate; pid 2 started in between and reports lifetime
        // I/O as its first delta, so it still has none.
        assert_eq!(base.rate(id(1), 4_000_000, 2.0), Some(2_000_000.0));
        assert_eq!(base.rate(id(2), 900_000_000, 2.0), None);
        assert_eq!(base.rate(None, 10, 2.0), None);
        assert_eq!(base.rate(id(1), 10, 0.0), None);
        base.commit([1, 2].into_iter().filter_map(id));
        assert_eq!(base.rate(id(2), 100, 2.0), Some(50.0));
        // A process that disappeared is dropped; a reused pid with a new start time is new.
        base.commit([2].into_iter().filter_map(id));
        assert_eq!(base.rate(id(1), 100, 2.0), None);
        let reused = Some(ProcessIdentity {
            pid: 2,
            created: 99,
        });
        assert_eq!(base.rate(reused, 100, 2.0), None);
    }

    #[test]
    fn group_disk_ignores_processes_without_a_rate() {
        let new_proc = p(12, "chrome.exe", 0.0, 10);
        let procs = vec![with_disk(p(10, "chrome.exe", 1.0, 100), 500.0), new_proc];
        let rows = build_rows(&procs, true, &none);
        assert_eq!(rows[0].disk, Some(500.0));
        let only_new = build_rows(&[p(12, "chrome.exe", 0.0, 10)], true, &none);
        assert_eq!(only_new[0].disk, None);
    }

    #[test]
    fn grouped_row_is_protected_only_when_all_are() {
        let some = |q: &Proc| q.pid == 10;
        assert!(!build_rows(&sample(), true, &some)[0].protected);
        let all = |q: &Proc| q.name.to_lowercase() == "chrome.exe";
        assert!(build_rows(&sample(), true, &all)[0].protected);
    }

    #[test]
    fn sorts_by_every_column_with_name_ties() {
        let mut rows = build_rows(&sample(), false, &none);
        sort_rows(&mut rows, SortKey::Memory, true);
        let order: Vec<u32> = rows.iter().map(|r| r.pid).collect();
        assert_eq!(order, vec![11, 10, 20, 4]);
        sort_rows(&mut rows, SortKey::Cpu, false);
        assert_eq!(rows[0].pid, 4);
        sort_rows(&mut rows, SortKey::Pid, false);
        assert_eq!(rows[0].pid, 4);
        sort_rows(&mut rows, SortKey::Name, false);
        assert_eq!(rows[0].name, "chrome.exe");
        assert_eq!(rows[0].pid, 10);

        let ties = vec![
            p(3, "b.exe", 1.0, 5),
            p(2, "a.exe", 1.0, 5),
            p(1, "a.exe", 1.0, 5),
        ];
        let mut rows = build_rows(&ties, false, &none);
        sort_rows(&mut rows, SortKey::Cpu, true);
        let order: Vec<u32> = rows.iter().map(|r| r.pid).collect();
        assert_eq!(order, vec![1, 2, 3]);
    }

    #[test]
    fn filter_matches_name_or_pid() {
        let rows = build_rows(&sample(), false, &none);
        assert_eq!(filter_rows(rows.clone(), "CHR").len(), 2);
        assert_eq!(filter_rows(rows.clone(), "20").len(), 1);
        assert_eq!(filter_rows(rows.clone(), "  ").len(), 4);
        assert!(filter_rows(rows, "zzz").is_empty());
        let grouped = build_rows(&sample(), true, &none);
        assert_eq!(filter_rows(grouped, "11").len(), 1);
    }

    #[test]
    fn paused_view_uses_the_frozen_snapshot() {
        let live = vec![p(1, "a.exe", 9.0, 9)];
        let frozen = vec![p(1, "a.exe", 1.0, 1), p(2, "b.exe", 2.0, 2)];
        assert_eq!(shown(&live, None).len(), 1);
        assert_eq!(shown(&live, Some(&frozen)).len(), 2);
        assert!((shown(&live, Some(&frozen))[0].cpu - 1.0).abs() < 1e-6);
    }

    #[test]
    fn reused_pid_changes_selection_and_confirmation_keys() {
        let original = p(10, "app.exe", 0.0, 0);
        let mut replacement = original.clone();
        replacement.identity.as_mut().unwrap().created += 1;
        assert_ne!(row_key(false, &original), row_key(false, &replacement));
        assert_ne!(
            action_identities(&[original]),
            action_identities(&[replacement])
        );
    }

    #[test]
    fn process_actions_verify_identity_on_the_handle_they_modify() {
        let mut child = spawn_idle_child();
        let sample_start = snapshot_start();
        let pid = sysinfo::Pid::from_u32(child.id());
        let mut system = System::new().unwrap();
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing(),
        );
        let process = system.process(pid).unwrap();
        let identity = sampled_identity(process, sample_start).unwrap();
        let name = process.name().to_string_lossy();
        let stale = ProcessIdentity {
            created: identity.created - 1,
            ..identity
        };

        assert_eq!(
            end_process(stale, &name, &ok()),
            Err(ActionError::IdentityChanged)
        );
        assert_eq!(
            end_process(identity, "other.exe", &ok()),
            Err(ActionError::IdentityChanged)
        );
        assert_eq!(
            set_priority(stale, &name, &ok(), BELOW_NORMAL_PRIORITY_CLASS.0, true),
            Err(ActionError::IdentityChanged)
        );
        assert!(child.try_wait().unwrap().is_none());

        let previous =
            set_priority(identity, &name, &ok(), BELOW_NORMAL_PRIORITY_CLASS.0, false).unwrap();
        assert_eq!(
            set_priority(identity, &name, &ok(), previous, false),
            Ok(BELOW_NORMAL_PRIORITY_CLASS.0)
        );
        end_process(identity, &name, &ok()).unwrap();
        assert_eq!(child.wait().unwrap().code(), Some(1));
    }

    #[test]
    fn game_and_anti_cheat_processes_are_refused_before_opening() {
        for name in ["steam.exe", "BEService.exe", "cs2.exe"] {
            assert!(is_protected(500, name, &ok(), 1), "{name}");
        }
        assert!(is_protected(
            500,
            "IndieGame.exe",
            &Parent::known("steam.exe"),
            1
        ));
        assert!(!is_protected(
            500,
            "notepad.exe",
            &Parent::known("explorer.exe"),
            1
        ));
        assert_eq!(
            protected_note("vgc.exe", &ok()),
            "Protected: anti-cheat process, Toolboks leaves it alone."
        );
        assert_eq!(
            protected_note("IndieGame.exe", &Parent::known("Steam.exe")),
            "Protected: started by a game launcher, Toolboks leaves it alone."
        );
        assert_eq!(protected_note("csrss.exe", &ok()), PROTECTED_NOTE);
        // Pids are multiples of 4 on Windows, so no real process is touched here.
        let fake = ProcessIdentity {
            pid: u32::MAX - 1,
            created: 1,
        };
        let refused = Err(ActionError::Protected(
            "Protected: game process, Toolboks leaves it alone.",
        ));
        assert_eq!(end_process(fake, "cs2.exe", &ok()), refused);
        assert!(matches!(
            end_process(
                fake,
                "IndieGame.exe",
                &Parent::known("EpicGamesLauncher.exe")
            ),
            Err(ActionError::Protected(_))
        ));
        assert!(matches!(
            set_priority(
                fake,
                "EasyAntiCheat.exe",
                &ok(),
                BELOW_NORMAL_PRIORITY_CLASS.0,
                true
            ),
            Err(ActionError::Protected(_))
        ));
        assert_eq!(
            result_line("Ended", "cs2.exe", 5, refused),
            "cs2.exe (pid 5): Protected: game process, Toolboks leaves it alone."
        );
    }

    #[test]
    fn live_name_gate_rejects_changed_or_protected_names() {
        assert!(live_name_ok(40, "Chrome.exe", "chrome.exe", &ok(), 1));
        // The pid now belongs to another program.
        assert!(!live_name_ok(40, "chrome.exe", "notepad.exe", &ok(), 1));
        assert!(!live_name_ok(40, "chrome.exe", "cs2.exe", &ok(), 1));
        // Same name, but protected through its launcher parent.
        assert!(!live_name_ok(
            40,
            "game.exe",
            "game.exe",
            &Parent::known("steam.exe"),
            1
        ));
    }

    #[test]
    fn protected_names_and_pids() {
        for name in [
            "System",
            "Registry",
            "SVCHOST.EXE",
            "Memory Compression",
            "dwm.exe",
        ] {
            assert!(is_protected(500, name, &ok(), 1), "{name}");
        }
        assert!(is_protected(0, "anything", &ok(), 1));
        assert!(is_protected(1234, "whatever.exe", &ok(), 1234));
        assert!(!is_protected(777, "refused.exe", &ok(), 1));
        assert!(!is_protected(500, "chrome.exe", &ok(), 1));
    }

    /// A process the refresh tied to creation time `created`, parent not linked yet.
    fn born(pid: u32, name: &str, created: Option<u64>) -> Proc {
        Proc {
            identity: created.map(|created| ProcessIdentity { pid, created }),
            parent: Parent::Unverified,
            ..p(pid, name, 0.0, 0)
        }
    }

    #[test]
    fn a_parent_older_than_its_child_is_known() {
        let mut procs = vec![
            born(10, "steam.exe", Some(100)),
            born(11, "IndieGame.exe", Some(200)),
            born(12, "notepad.exe", Some(300)),
            born(13, "explorer.exe", Some(50)),
        ];
        link_parents(&mut procs, &[None, Some(10), Some(13), None]);
        assert_eq!(procs[1].parent, Parent::known("steam.exe"));
        assert!(is_protected(11, "IndieGame.exe", &procs[1].parent, 1));
        // A normal parent leaves an ordinary app unprotected.
        assert_eq!(procs[2].parent, Parent::known("explorer.exe"));
        assert!(!is_protected(12, "notepad.exe", &procs[2].parent, 1));
    }

    #[test]
    fn a_reused_parent_pid_is_unverified_and_protects_the_child() {
        // Steam (pid 10) started the game and exited; notepad then got pid 10.
        let mut procs = vec![
            born(10, "notepad.exe", Some(300)),
            born(11, "IndieGame.exe", Some(200)),
        ];
        link_parents(&mut procs, &[None, Some(10)]);
        assert_eq!(procs[1].parent, Parent::Unverified);
        assert!(is_protected(11, "IndieGame.exe", &procs[1].parent, 1));
        assert_eq!(
            protected_note("IndieGame.exe", &procs[1].parent),
            "Protected: the program that started it has closed or cannot be checked, \
             Toolboks leaves it alone."
        );
    }

    #[test]
    fn an_exited_or_unreadable_parent_is_unverified() {
        // The launcher exited and nothing holds its pid.
        let mut procs = vec![born(11, "IndieGame.exe", Some(200))];
        link_parents(&mut procs, &[Some(10)]);
        assert_eq!(procs[0].parent, Parent::Unverified);
        // The parent's creation time could not be read (other user, or born mid-refresh).
        let mut procs = vec![
            born(10, "steam.exe", None),
            born(11, "IndieGame.exe", Some(200)),
        ];
        link_parents(&mut procs, &[None, Some(10)]);
        assert_eq!(procs[1].parent, Parent::Unverified);
    }

    #[test]
    fn an_unverified_parent_is_refused_before_opening() {
        // Pids are multiples of 4 on Windows, so no real process is touched here.
        let fake = ProcessIdentity {
            pid: u32::MAX - 1,
            created: 1,
        };
        let refused = Err(ActionError::Protected(
            "Protected: the program that started it has closed or cannot be checked, \
             Toolboks leaves it alone.",
        ));
        assert_eq!(
            end_process(fake, "notepad.exe", &Parent::Unverified),
            refused
        );
        assert_eq!(
            set_priority(
                fake,
                "notepad.exe",
                &Parent::Unverified,
                BELOW_NORMAL_PRIORITY_CLASS.0,
                true
            )
            .map(|_| ()),
            refused
        );
        assert!(!live_name_ok(
            40,
            "notepad.exe",
            "notepad.exe",
            &Parent::Unverified,
            1
        ));
    }

    #[test]
    fn plan_end_splits_protected() {
        let sel = sample();
        let prot = |q: &Proc| q.pid == 4;
        let (to_end, skipped) = plan_end(&sel, &prot);
        assert_eq!(to_end.len(), 3);
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].pid, 4);
        let (to_end, skipped) = plan_end(&[], &prot);
        assert!(to_end.is_empty() && skipped.is_empty());
    }

    #[test]
    fn confirm_wording() {
        assert_eq!(
            confirm_text("Notepad", 1),
            "End Notepad? Unsaved work in this app will be lost."
        );
        assert_eq!(
            confirm_text("Chrome", 14),
            "End 14 processes of Chrome? Unsaved work in them will be lost."
        );
    }

    #[test]
    fn copy_text_lines() {
        let rows = build_rows(&sample(), true, &none);
        let refs: Vec<&Row> = rows.iter().take(2).collect();
        let text = copy_text(&refs, true);
        assert_eq!(
            text,
            "chrome.exe, 2 processes, CPU 3.0%, 0 MB\ncode.exe, pid 20, CPU 5.0%, 0 MB"
        );
        assert!(copy_text(&refs, false).contains("CPU ..."));
    }

    #[test]
    fn result_lines_are_plain() {
        assert_eq!(
            result_line("Ended", "a.exe", 5, Ok(())),
            "Ended a.exe (pid 5)"
        );
        assert_eq!(
            result_line("Ended", "a.exe", 5, Err(ActionError::AccessDenied)),
            "a.exe (pid 5): Access denied"
        );
    }

    #[test]
    fn selection_click_ctrl_shift() {
        let keys: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect();
        let mut s = Selection::default();
        s.only("b");
        s.toggle("d");
        assert_eq!(s.keys.len(), 2);
        s.toggle("d");
        assert!(!s.keys.contains("d"));
        s.only("b");
        s.range("d", &keys);
        assert_eq!(s.keys.len(), 3);
        assert!(s.keys.contains("c"));
        let existing: HashSet<String> = ["b".to_string()].into();
        s.prune(&existing);
        assert_eq!(s.keys.len(), 1);
        let mut fresh = Selection::default();
        fresh.range("c", &keys);
        assert_eq!(fresh.keys.len(), 1);
    }

    #[test]
    fn sort_arrows_use_letters() {
        assert_eq!(sort_arrow(true, true), " v");
        assert_eq!(sort_arrow(true, false), " ^");
        assert_eq!(sort_arrow(false, true), "");
    }

    #[test]
    fn visibility_window() {
        assert!(!visible(0, 100));
        assert!(visible(1001, 1500));
        assert!(!visible(1001, 1000 + VISIBLE_WINDOW_MS));
    }
}
