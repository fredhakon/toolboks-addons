//! Real `ProcCtl` on Win32: liveness by Windows pid, BELOW_NORMAL/NORMAL priority and a CPU core lock on a process tree, and
//! `taskkill /T /F` for killing a tree.
use crate::state::ProcCtl;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, E_ACCESSDENIED};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, QueryFullProcessImageNameW, SetPriorityClass,
    SetProcessAffinityMask, BELOW_NORMAL_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION,
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
        let mut seen = vec![win_pid];
        let mut walk = vec![win_pid];
        let mut killed = 0;
        while let Some(parent) = walk.pop() {
            for (pid, ppid, name) in &procs {
                if *ppid != parent || *pid == 0 || seen.contains(pid) {
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

/// `root` plus all its descendants, given (pid, parent pid) pairs.
/// shortcut: Windows keeps a dead parent's pid in its children, so a reused pid can adopt unrelated processes;
/// compare creation times if that ever shows up in practice.
pub fn tree(root: u32, pairs: &[(u32, u32)]) -> Vec<u32> {
    let mut out = vec![root];
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
