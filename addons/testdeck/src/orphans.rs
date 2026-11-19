//! Orphan finder: node processes running jest, tsc or next, and tsc.exe, that belong to no running ticket's
//! process tree and whose parent process is gone. It only lists them; a kill is always a user's click (never
//! automatic), and it re-checks that the process is still the same orphan first.
use crate::state::AppState;
use crate::winproc::tree;
use serde::Serialize;
use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

#[derive(Clone, Debug, Serialize)]
pub struct Orphan {
    pub pid: u32,
    pub name: String,
    /// Process start time (Unix seconds); with `pid` it identifies the process across scans.
    pub start_unix: u64,
    pub age_secs: u64,
    pub cmd: String,
}

/// Lists the orphans, oldest first. Takes a full process snapshot, so call it off the UI thread.
pub fn scan(s: &AppState) -> Vec<Orphan> {
    let roots: Vec<u32> = s
        .lock()
        .sched
        .running
        .iter()
        .filter_map(|r| r.req.win_pid)
        .collect();
    let mut sys = System::new();
    let kind = ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always);
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
    let procs = sys.processes();
    let pairs: Vec<(u32, u32)> = procs
        .iter()
        .filter_map(|(pid, p)| p.parent().map(|pp| (pid.as_u32(), pp.as_u32())))
        .collect();
    let owned: HashSet<u32> = roots.iter().flat_map(|&r| tree(r, &pairs)).collect();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut out: Vec<Orphan> = procs
        .iter()
        .filter_map(|(pid, p)| {
            let pid = pid.as_u32();
            let name = p.name().to_string_lossy().to_lowercase();
            let cmd = p
                .cmd()
                .iter()
                .map(|a| a.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ");
            let lc = cmd.to_lowercase();
            let wanted = name == "tsc.exe"
                || (name == "node.exe" && ["jest", "tsc", "next"].iter().any(|k| lc.contains(k)));
            if !wanted || owned.contains(&pid) {
                return None;
            }
            // Windows keeps a dead parent's pid; a live process with that pid that started later is not the parent.
            let parent_alive = p
                .parent()
                .and_then(|pp| procs.get(&pp))
                .is_some_and(|pp| pp.start_time() <= p.start_time());
            if parent_alive {
                return None;
            }
            let start = p.start_time();
            Some(Orphan {
                pid,
                name,
                start_unix: start,
                age_secs: now.saturating_sub(start),
                cmd,
            })
        })
        .collect();
    out.sort_by_key(|o| o.start_unix);
    out
}

/// Kills the orphan's process tree, only when a fresh scan still lists the same process (pid and start time), so
/// a reused pid or a process that got a ticket since is never hit. Returns whether it killed.
pub fn kill(s: &AppState, pid: u32, start_unix: u64) -> bool {
    let still = scan(s)
        .iter()
        .any(|o| o.pid == pid && o.start_unix == start_unix);
    if still {
        s.proc_ctl.kill_tree(pid);
    }
    still
}
