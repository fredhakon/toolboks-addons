//! Real `ProcCtl` on Win32: liveness by Windows pid, BELOW_NORMAL/NORMAL priority and a CPU core lock on a process tree, and
//! `taskkill /T /F` plus the ticket's job object for killing a tree.
//!
//! A ticket's process set is its Windows job object (see `job_pids`) plus the parent-pid walk. The walk alone loses
//! processes whose parent exited (the msys wrapper between `jt.sh` and `npx jest` does), the job does not.
use crate::state::ProcCtl;
use std::collections::HashMap;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, E_ACCESSDENIED, HANDLE};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicProcessIdList,
    QueryInformationJobObject, TerminateJobObject, JOBOBJECT_BASIC_PROCESS_ID_LIST,
};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, QueryFullProcessImageNameW, SetPriorityClass,
    SetProcessAffinityMask, BELOW_NORMAL_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION, PROCESS_SET_QUOTA,
    PROCESS_TERMINATE,
};

const STILL_ACTIVE: u32 = 259;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub struct WinProc;

impl ProcCtl for WinProc {
    fn is_alive(&self, win_pid: u32) -> bool {
        unsafe {
            match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, win_pid) {
                Ok(h) => {
                    let mut code = 0u32;
                    let read = GetExitCodeProcess(h, &mut code).is_ok();
                    let _ = CloseHandle(h);
                    // An unreadable exit code counts as alive: the heartbeat timeout still catches a dead client.
                    !read || code == STILL_ACTIVE
                }
                // Access denied means the process exists but runs elevated or as another user.
                Err(e) => e.code() == E_ACCESSDENIED,
            }
        }
    }

    fn track(&self, win_pid: u32) {
        job_pids(win_pid);
    }

    fn set_tree_priority(&self, win_pid: u32, below_normal: bool) {
        let class = if below_normal {
            BELOW_NORMAL_PRIORITY_CLASS
        } else {
            NORMAL_PRIORITY_CLASS
        };
        for pid in tree(win_pid, &snapshot()) {
            unsafe {
                if let Ok(h) = OpenProcess(PROCESS_SET_INFORMATION, false, pid) {
                    let _ = SetPriorityClass(h, class);
                    let _ = CloseHandle(h);
                }
            }
        }
    }

    fn set_tree_affinity(&self, win_pid: u32, mask: u64) {
        for pid in tree(win_pid, &snapshot()) {
            unsafe {
                if let Ok(h) = OpenProcess(PROCESS_SET_INFORMATION, false, pid) {
                    let _ = SetProcessAffinityMask(h, mask as usize);
                    let _ = CloseHandle(h);
                }
            }
        }
    }

    fn kill_tree(&self, win_pid: u32) {
        // The job holds the processes whose parent already exited, which taskkill /T cannot reach.
        let job = JOBS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
            .and_then(|j| j.remove(&win_pid));
        if let Some(h) = job {
            unsafe {
                let h = HANDLE(h as *mut core::ffi::c_void);
                let _ = TerminateJobObject(h, 1);
                let _ = CloseHandle(h);
            }
        }
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &win_pid.to_string()])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    fn kill_descendants(&self, win_pid: u32) -> usize {
        // Only the native work dies (node, npx, cmd wrappers, each with its tree). Git Bash processes are walked
        // through but spared: the client shell, its heartbeat loop and, on the --rdb path, rdb-lock.sh and the
        // nested jt.sh, which then see jest exit, ask td_requeue_pending and release their locks normally.
        let procs = snapshot_named();
        // Job members whose parent is gone or outside the job hang under nothing the walk can see: treat them as
        // children of the root.
        let members = job_pids(win_pid);
        let detached: Vec<u32> = procs
            .iter()
            .filter(|(pid, ppid, _)| {
                members.contains(pid)
                    && *pid != win_pid
                    && !members.contains(ppid)
                    && *ppid != win_pid
            })
            .map(|(pid, _, _)| *pid)
            .collect();
        let mut seen = vec![win_pid];
        let mut walk = vec![win_pid];
        let mut killed = 0;
        while let Some(parent) = walk.pop() {
            for (pid, ppid, name) in &procs {
                let child = *ppid == parent || (parent == win_pid && detached.contains(pid));
                if !child || *pid == 0 || seen.contains(pid) {
                    continue;
                }
                seen.push(*pid);
                if is_msys(*pid, name) {
                    walk.push(*pid);
                } else {
                    self.kill_tree(*pid);
                    killed += 1;
                }
            }
        }
        killed
    }
}

/// Local wall-clock time as minutes after midnight (the night window).
pub fn local_minutes() -> u32 {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    u32::from(t.wHour) * 60 + u32::from(t.wMinute)
}

/// (pid, parent pid) of every process on the machine, from a Toolhelp snapshot.
fn snapshot() -> Vec<(u32, u32)> {
    snapshot_named()
        .into_iter()
        .map(|(pid, ppid, _)| (pid, ppid))
        .collect()
}

/// (pid, parent pid, lowercase exe name) of every process on the machine.
fn snapshot_named() -> Vec<(u32, u32, String)> {
    let mut out = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return out;
        };
        let mut e = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut more = Process32FirstW(snap, &mut e).is_ok();
        while more {
            let len = e
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(e.szExeFile.len());
            let name = String::from_utf16_lossy(&e.szExeFile[..len]).to_lowercase();
            out.push((e.th32ProcessID, e.th32ParentProcessID, name));
            more = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    out
}

/// Git Bash (MSYS) processes: the shells and tools of the bash client, or any exe under a `usr\bin` dir.
fn is_msys(pid: u32, name: &str) -> bool {
    const NAMES: [&str; 6] = [
        "bash.exe",
        "sh.exe",
        "dash.exe",
        "sleep.exe",
        "curl.exe",
        "cat.exe",
    ];
    NAMES.contains(&name)
        || image_path(pid).is_some_and(|p| {
            let dir = p.to_lowercase().replace('/', "\\");
            dir.rsplit_once('\\')
                .is_some_and(|(d, _)| d.ends_with("\\usr\\bin"))
        })
}

fn image_path(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let r =
            QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        r.ok()?;
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

/// Job object per running ticket root, as raw handle values (a `HANDLE` is not `Send`). Never kill-on-close: the
/// job only lists and, in `kill_tree`, terminates.
static JOBS: Mutex<Option<HashMap<u32, usize>>> = Mutex::new(None);

/// Pids of every process in the root's job object, creating the job and putting the root into it on first use.
/// Processes the root spawns afterwards stay in the job even when their parent exits. Empty when the root cannot
/// be assigned (gone, access denied, or already in a job that forbids nesting): callers then use the parent walk.
/// shortcut: processes spawned before the first call are not in the job (the walk still finds those whose parent
/// lives); `track` makes the first call at the grant, before the client knows it was granted, so only a root that
/// already runs work at that point (an old client that registers late) loses the processes whose parent exited.
fn job_pids(root: u32) -> Vec<u32> {
    let mut g = JOBS.lock().unwrap_or_else(|e| e.into_inner());
    let jobs = g.get_or_insert_with(HashMap::new);
    let h = match jobs.get(&root) {
        Some(&h) => HANDLE(h as *mut core::ffi::c_void),
        None => {
            let Some(h) = new_job(root) else {
                return Vec::new();
            };
            jobs.insert(root, h.0 as usize);
            h
        }
    };
    let pids = job_members(h);
    if pids.is_empty() {
        jobs.remove(&root);
        unsafe {
            let _ = CloseHandle(h);
        }
    }
    pids
}

fn new_job(root: u32) -> Option<HANDLE> {
    unsafe {
        let p = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, root).ok()?;
        let job = CreateJobObjectW(None, windows::core::PCWSTR::null()).ok();
        let ok = job.is_some_and(|j| AssignProcessToJobObject(j, p).is_ok());
        let _ = CloseHandle(p);
        if !ok {
            if let Some(j) = job {
                let _ = CloseHandle(j);
            }
            return None;
        }
        job
    }
}

fn job_members(job: HANDLE) -> Vec<u32> {
    // usize slots keep the buffer aligned for the struct: 2 header words, then room for 1022 pids.
    let mut buf = [0usize; 1024];
    unsafe {
        let list = buf.as_mut_ptr() as *mut JOBOBJECT_BASIC_PROCESS_ID_LIST;
        if QueryInformationJobObject(
            Some(job),
            JobObjectBasicProcessIdList,
            list as *mut core::ffi::c_void,
            std::mem::size_of_val(&buf) as u32,
            None,
        )
        .is_err()
        {
            return Vec::new();
        }
        let n = ((*list).NumberOfProcessIdsInList as usize).min(1022);
        std::slice::from_raw_parts(std::ptr::addr_of!((*list).ProcessIdList) as *const usize, n)
            .iter()
            .map(|&p| p as u32)
            .collect()
    }
}

/// Drops the job handle of every root that is no longer a running ticket (its processes keep running; a later
/// reuse of the pid starts a fresh job).
pub fn forget_jobs_except(running: &[u32]) {
    let mut g = JOBS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(jobs) = g.as_mut() {
        jobs.retain(|root, h| {
            let keep = running.contains(root);
            if !keep {
                unsafe {
                    let _ = CloseHandle(HANDLE(*h as *mut core::ffi::c_void));
                }
            }
            keep
        });
    }
}

/// `root`, everything in its job object, and all their descendants: the one process set of a ticket, used for
/// RAM and CPU, affinity, priority and the orphan filter.
pub fn tree(root: u32, pairs: &[(u32, u32)]) -> Vec<u32> {
    let mut roots = vec![root];
    roots.extend(job_pids(root));
    walk(&roots, pairs)
}

/// `roots` plus all their descendants, given (pid, parent pid) pairs. A process whose parent exited is only found
/// when one of `roots` names it (the job members do); a stray process is never adopted.
/// shortcut: Windows keeps a dead parent's pid in its children, so a reused pid can adopt unrelated processes;
/// compare creation times if that ever shows up in practice.
fn walk(roots: &[u32], pairs: &[(u32, u32)]) -> Vec<u32> {
    let mut out: Vec<u32> = Vec::new();
    for &r in roots {
        if !out.contains(&r) {
            out.push(r);
        }
    }
    let mut i = 0;
    while i < out.len() {
        let parent = out[i];
        for &(pid, ppid) in pairs {
            if ppid == parent && pid != 0 && !out.contains(&pid) {
                out.push(pid);
            }
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ticket root 10 (bash) -> 20 (wrapper, exited) -> 30 (bash) -> 31, 32 (jest workers). 40 is another tool's node.
    const PAIRS: [(u32, u32); 5] = [(10, 1), (30, 20), (31, 30), (32, 30), (40, 99)];

    #[test]
    fn parent_walk_alone_loses_the_tree_below_an_exited_process() {
        assert_eq!(walk(&[10], &PAIRS), vec![10]);
    }

    #[test]
    fn job_members_pull_in_their_descendants_and_nothing_unrelated() {
        let mut t = walk(&[10, 30], &PAIRS);
        t.sort();
        assert_eq!(t, vec![10, 30, 31, 32]);
        assert!(!t.contains(&40));
    }

    #[test]
    fn real_job_keeps_a_grandchild_after_its_parent_exits() {
        // cmd waits ~1 s (so the job exists first), starts a detached ping and exits.
        let mut cmd = Command::new("cmd")
            .args([
                "/c",
                "ping -n 2 127.0.0.1 >nul & start /b ping -n 20 127.0.0.1 >nul",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let root = cmd.id();
        assert!(job_pids(root).contains(&root));
        cmd.wait().unwrap();
        let mut found = false;
        for _ in 0..50 {
            // The ping's parent (cmd) is gone, so only the job can still reach it.
            if tree(root, &snapshot()).iter().any(|&p| p != root) {
                found = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        WinProc.kill_tree(root);
        assert!(found, "ping not found through the job");
        assert!(job_pids(root).is_empty());
    }

    #[test]
    fn track_covers_the_root_and_its_later_children_but_never_an_unrelated_process() {
        let ping = |secs: u32| {
            Command::new("ping")
                .args(["-n", &secs.to_string(), "127.0.0.1"])
                .creation_flags(CREATE_NO_WINDOW)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        };
        // Another tool's process, started before and outside the ticket.
        let mut other = ping(20);
        let mut root = Command::new("cmd")
            .args(["/c", "ping -n 3 127.0.0.1 >nul & ping -n 20 127.0.0.1 >nul"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        WinProc.track(root.id());
        let mut seen = Vec::new();
        for _ in 0..40 {
            seen = tree(root.id(), &snapshot());
            if seen.len() > 1 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        WinProc.kill_tree(root.id());
        let _ = other.kill();
        let _ = (root.wait(), other.wait());
        assert!(seen.contains(&root.id()) && seen.len() > 1, "{seen:?}");
        assert!(!seen.contains(&other.id()), "unrelated process adopted");
    }
}
