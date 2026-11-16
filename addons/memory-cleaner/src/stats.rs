//! Memory and process sampling. No UI here, so the logic stays testable.

use protected_processes::Parent;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

use super::audio::AudioState;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcInfo {
    pub pid: u32,
    pub name: String,
    /// Physical memory currently held by the process (working set).
    pub working_set: u64,
    /// Private bytes: memory committed to this process alone.
    pub private: u64,
    /// 1 for a single process; the number of processes in a grouped row.
    pub count: usize,
    /// The parent this snapshot vouched for (to protect children of game launchers). A parent
    /// that has exited, whose pid was reused, or whose creation time could not be read is
    /// [`Parent::Unverified`], and the process is then protected.
    pub parent: Parent,
    /// Pid of the parent, only when `parent` is known.
    pub parent_pid: Option<u32>,
    /// Creation time (FILETIME) of the process this snapshot saw, or `None` when it could not
    /// be read or the pid may have been reused while the snapshot was taken. Actions open a
    /// process only when its live creation time equals this, so the parent and name above
    /// always describe the process that is changed.
    pub created: Option<u64>,
}

/// Merges processes with the same name (case-insensitive) into one row each: memory is
/// summed and `pid` is the lowest pid of the group.
pub fn group_by_name(procs: &[ProcInfo]) -> Vec<ProcInfo> {
    let mut groups: Vec<ProcInfo> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for p in procs {
        match index.get(&p.name.to_lowercase()) {
            Some(&i) => {
                let g = &mut groups[i];
                g.pid = g.pid.min(p.pid);
                g.working_set += p.working_set;
                g.private += p.private;
                g.count += p.count;
            }
            None => {
                index.insert(p.name.to_lowercase(), groups.len());
                groups.push(p.clone());
            }
        }
    }
    groups
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    pub total: u64,
    pub used: u64,
    pub available: u64,
}

impl Totals {
    pub fn used_fraction(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            (self.used as f64 / self.total as f64) as f32
        }
    }
}

/// Reads system memory and the process list through `sysinfo`.
pub struct Sampler {
    sys: System,
}

pub struct Snapshot {
    pub totals: Totals,
    pub processes: Vec<ProcInfo>,
    /// Audio sessions read with this snapshot for a run; [`AudioState::Unknown`] otherwise.
    pub audio: AudioState,
}

/// Fills in each process's parent from the same snapshot. `parent_pids[i]` is the parent pid
/// the snapshot reported for `procs[i]`. A parent is known only when it is in the snapshot and
/// was created before the child ([`Parent::vouch`]); otherwise it stays unverified. Pure.
pub fn link_parents(procs: &mut [ProcInfo], parent_pids: &[Option<u32>]) {
    let seen: std::collections::HashMap<u32, (String, Option<u64>)> = procs
        .iter()
        .map(|p| (p.pid, (p.name.clone(), p.created)))
        .collect();
    for (p, ppid) in procs.iter_mut().zip(parent_pids) {
        let found = ppid.and_then(|ppid| seen.get(&ppid).map(|parent| (ppid, parent)));
        p.parent = Parent::vouch(
            p.created,
            found.map(|(_, (name, created))| (name.as_str(), *created)),
        );
        p.parent_pid = match p.parent {
            Parent::Known(_) => found.map(|(ppid, _)| ppid),
            Parent::Unverified => None,
        };
    }
}

impl Sampler {
    /// `None` if the platform refuses to provide system info.
    pub fn new() -> Option<Self> {
        Some(Self {
            sys: System::new().ok()?,
        })
    }

    /// Full refresh: totals and the process list.
    pub fn refresh(&mut self) -> Snapshot {
        let sample_start = super::trim::snapshot_start();
        self.sys.refresh_memory();
        let totals = Totals {
            total: self.sys.total_memory(),
            used: self.sys.used_memory(),
            available: self.sys.available_memory(),
        };
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_memory(),
        );
        let (mut processes, parent_pids): (Vec<ProcInfo>, Vec<Option<u32>>) = self
            .sys
            .processes()
            .values()
            .map(|p| {
                let info = ProcInfo {
                    pid: p.pid().as_u32(),
                    name: p.name().to_string_lossy().into_owned(),
                    working_set: p.memory(),
                    private: p.virtual_memory(),
                    count: 1,
                    parent: Parent::Unverified,
                    parent_pid: None,
                    created: super::trim::snapshot_created(
                        p.pid().as_u32(),
                        p.start_time(),
                        sample_start,
                    ),
                };
                (info, p.parent().map(|pp| pp.as_u32()))
            })
            .unzip();
        link_parents(&mut processes, &parent_pids);
        Snapshot {
            totals,
            processes,
            audio: AudioState::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Name,
    /// Kept for filtering and tests; the workbench table has no pid column.
    #[allow(dead_code)]
    Pid,
    WorkingSet,
    Private,
}

/// Rows matching `filter` (case-insensitive, on name or pid), ordered by `key`.
pub fn sort_and_filter<'a>(
    procs: &'a [ProcInfo],
    key: SortKey,
    descending: bool,
    filter: &str,
) -> Vec<&'a ProcInfo> {
    let needle = filter.trim().to_lowercase();
    let mut rows: Vec<&ProcInfo> = procs
        .iter()
        .filter(|p| {
            needle.is_empty()
                || p.name.to_lowercase().contains(&needle)
                || p.pid.to_string().contains(&needle)
        })
        .collect();

    rows.sort_by(|a, b| {
        let ord = match key {
            SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            SortKey::Pid => a.pid.cmp(&b.pid),
            SortKey::WorkingSet => a.working_set.cmp(&b.working_set),
            SortKey::Private => a.private.cmp(&b.private),
        };
        let ord = if descending { ord.reverse() } else { ord };
        ord.then(a.pid.cmp(&b.pid))
    });
    rows
}

pub fn format_bytes(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.2} GiB", b / GIB)
    } else {
        format!("{:.1} MiB", b / MIB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, name: &str, working_set: u64, private: u64) -> ProcInfo {
        ProcInfo {
            pid,
            name: name.to_string(),
            working_set,
            private,
            count: 1,
            parent: Parent::Unverified,
            parent_pid: None,
            created: None,
        }
    }

    /// A process the snapshot tied to creation time `created`.
    fn born(pid: u32, name: &str, created: u64) -> ProcInfo {
        ProcInfo {
            created: Some(created),
            ..proc(pid, name, 0, 0)
        }
    }

    #[test]
    fn a_parent_older_than_its_child_is_known() {
        let mut procs = vec![born(10, "steam.exe", 100), born(11, "IndieGame.exe", 200)];
        link_parents(&mut procs, &[None, Some(10)]);
        assert_eq!(procs[1].parent, Parent::known("steam.exe"));
        assert_eq!(procs[1].parent_pid, Some(10));
        // No parent pid at all: nothing vouches for it.
        assert_eq!(procs[0].parent, Parent::Unverified);
    }

    #[test]
    fn a_reused_parent_pid_is_unverified() {
        // Steam (pid 10) started the game and exited; notepad then got pid 10.
        let mut procs = vec![born(10, "notepad.exe", 300), born(11, "IndieGame.exe", 200)];
        link_parents(&mut procs, &[None, Some(10)]);
        assert_eq!(procs[1].parent, Parent::Unverified);
        assert_eq!(procs[1].parent_pid, None);
    }

    #[test]
    fn an_exited_or_unreadable_parent_is_unverified() {
        // The launcher exited and nothing holds its pid.
        let mut procs = vec![born(11, "IndieGame.exe", 200)];
        link_parents(&mut procs, &[Some(10)]);
        assert_eq!(procs[0].parent, Parent::Unverified);
        // The parent's creation time could not be read (other user, or born mid-snapshot).
        let mut procs = vec![proc(10, "steam.exe", 0, 0), born(11, "IndieGame.exe", 200)];
        link_parents(&mut procs, &[None, Some(10)]);
        assert_eq!(procs[1].parent, Parent::Unverified);
    }

    #[test]
    fn snapshot_ties_processes_to_their_creation_time() {
        let snap = Sampler::new().expect("sampler").refresh();
        let own = snap
            .processes
            .iter()
            .find(|p| p.pid == std::process::id())
            .expect("own process in the snapshot");
        // The test process started long before this snapshot, so it is vouched for.
        assert!(own.created.is_some(), "{own:?}");
    }

    #[test]
    fn groups_by_name_and_sums_memory() {
        let grouped = group_by_name(&sample());
        assert_eq!(grouped.len(), 2);
        let chrome = grouped.iter().find(|p| p.name == "chrome.exe").unwrap();
        assert_eq!(chrome.count, 2);
        assert_eq!(chrome.pid, 10, "lowest pid represents the group");
        assert_eq!(chrome.working_set, 400);
        assert_eq!(chrome.private, 500);
        let code = grouped.iter().find(|p| p.name == "Code.exe").unwrap();
        assert_eq!(code.count, 1);
    }

    #[test]
    fn grouping_ignores_name_case() {
        let procs = vec![proc(1, "Chrome.exe", 10, 1), proc(2, "chrome.EXE", 20, 2)];
        let grouped = group_by_name(&procs);
        assert_eq!(grouped.len(), 1);
        assert_eq!(grouped[0].count, 2);
    }

    fn sample() -> Vec<ProcInfo> {
        vec![
            proc(10, "chrome.exe", 300, 100),
            proc(20, "Code.exe", 500, 50),
            proc(30, "chrome.exe", 100, 400),
        ]
    }

    #[test]
    fn sorts_by_working_set_descending() {
        let procs = sample();
        let pids: Vec<u32> = sort_and_filter(&procs, SortKey::WorkingSet, true, "")
            .iter()
            .map(|p| p.pid)
            .collect();
        assert_eq!(pids, vec![20, 10, 30]);
    }

    #[test]
    fn sorts_by_name_case_insensitive_with_pid_tiebreak() {
        let procs = sample();
        let pids: Vec<u32> = sort_and_filter(&procs, SortKey::Name, false, "")
            .iter()
            .map(|p| p.pid)
            .collect();
        assert_eq!(pids, vec![10, 30, 20]);
    }

    #[test]
    fn filters_on_name_or_pid() {
        let procs = sample();
        assert_eq!(
            sort_and_filter(&procs, SortKey::Pid, false, "CHROME").len(),
            2
        );
        let by_pid = sort_and_filter(&procs, SortKey::Pid, false, "20");
        assert_eq!(by_pid.len(), 1);
        assert_eq!(by_pid[0].name, "Code.exe");
        assert!(sort_and_filter(&procs, SortKey::Pid, false, "nope").is_empty());
    }

    #[test]
    fn formats_bytes() {
        assert_eq!(format_bytes(0), "0.0 MiB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.00 GiB");
    }

    #[test]
    fn used_fraction_handles_zero_total() {
        assert_eq!(Totals::default().used_fraction(), 0.0);
        let t = Totals {
            total: 200,
            used: 50,
            available: 150,
        };
        assert_eq!(t.used_fraction(), 0.25);
    }
}
