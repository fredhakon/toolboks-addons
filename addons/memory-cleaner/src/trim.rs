//! Per-process working-set trim and the policy for which processes may be trimmed.
//!
//! `EmptyWorkingSet` needs no admin rights for processes of the same user. The trimmed pages
//! move to the standby list and fault back in when the process touches them again.

use std::collections::HashSet;

use windows::core::{HRESULT, PWSTR};
use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, FILETIME, HANDLE};
use windows::Win32::System::ProcessStatus::{
    EmptyWorkingSet, GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
};
use windows::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, PROCESS_ACCESS_RIGHTS,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA,
};

use protected_processes::{protected_reason, Protected};
use toolboks_shared::win::Handle;

/// System and shell processes that are never trimmed. Compared case-insensitively.
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
    "dwm.exe",
    "explorer.exe",
    // Trimming the audio engine or the font host causes audible glitches and redraw stalls.
    "audiodg.exe",
    "fontdrvhost.exe",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// Windows or the shell: trimming gains nothing and can hurt responsiveness.
    System,
    /// This app itself.
    Own,
    /// The user excluded it.
    Excluded,
    /// A game, game launcher or anti-cheat process: never opened for write access.
    Protected(Protected),
}

impl SkipReason {
    pub fn describe(self) -> &'static str {
        match self {
            SkipReason::System => "System and shell processes are never trimmed",
            SkipReason::Own => "This is toolboks itself",
            SkipReason::Excluded => "Excluded from trimming",
            SkipReason::Protected(p) => p.describe(),
        }
    }
}

/// Why this process must not be trimmed, or `None` if it may be.
/// `parent` is the parent's image name, if known. `exclusions` holds lowercase process names.
pub fn skip_reason(
    pid: u32,
    name: &str,
    parent: Option<&str>,
    own_pid: u32,
    exclusions: &HashSet<String>,
) -> Option<SkipReason> {
    let lower = name.to_lowercase();
    if pid == own_pid {
        Some(SkipReason::Own)
    } else if pid == 0 || pid == 4 || PROTECTED.contains(&lower.as_str()) {
        Some(SkipReason::System)
    } else if let Some(p) = protected_reason(&lower, parent) {
        Some(SkipReason::Protected(p))
    } else if exclusions.contains(&lower) {
        Some(SkipReason::Excluded)
    } else {
        None
    }
}

/// One process picked for a manual trim, exactly as the snapshot saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrimTarget {
    pub pid: u32,
    /// Creation time from the snapshot (see [`snapshot_created`]); `None` when it could not be
    /// tied to the snapshot, and then the process is never trimmed.
    pub created: Option<u64>,
    pub name: String,
    /// Parent image name from the same snapshot.
    pub parent: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trimmed {
    /// Working set in bytes just before and just after the trim.
    pub before: u64,
    pub after: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrimError {
    /// Protected process or one owned by another user; needs elevation.
    AccessDenied,
    /// Anything else, typically the process exited in the meantime.
    Failed,
    /// The live process is protected, or is not the one the snapshot saw (other creation time
    /// or name). No write-capable handle was opened.
    Refused,
}

fn filetime(t: FILETIME) -> u64 {
    (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)
}

/// Process creation time as a FILETIME value.
pub fn creation_time(handle: HANDLE) -> windows::core::Result<u64> {
    let (mut created, mut exited, mut kernel, mut user) = Default::default();
    // SAFETY: the handle has query access and all four outputs are valid FILETIME buffers.
    unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) }?;
    Ok(filetime(created))
}

/// The current time as a FILETIME value. Taken just before a process snapshot starts.
pub fn snapshot_start() -> u64 {
    // SAFETY: this call reads the system time and takes no arguments.
    filetime(unsafe { GetSystemTimePreciseAsFileTime() })
}

/// True when a creation time read after the snapshot belongs to the process the snapshot saw:
/// it started before the snapshot did, and in the second the snapshot reported (`start_secs`,
/// Unix seconds). A process that holds the pid now but started after the snapshot began cannot
/// be the one the snapshot saw (the pid was reused), so the sampled name and parent must not be
/// trusted for it. Pure.
pub fn born_before_snapshot(created: u64, start_secs: u64, sample_start: u64) -> bool {
    let unix_secs = (created / 10_000_000).checked_sub(11_644_473_600);
    created < sample_start && unix_secs == Some(start_secs)
}

/// Creation time of the process the snapshot saw at `pid`, or `None` when it cannot be read
/// (other user, protected) or the pid may have been reused since the snapshot began.
pub fn snapshot_created(pid: u32, start_secs: u64, sample_start: u64) -> Option<u64> {
    // SAFETY: plain FFI call; the returned handle is wrapped for closing right away.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
        .map(Handle)
        .ok()?;
    let created = creation_time(handle.0).ok()?;
    born_before_snapshot(created, start_secs, sample_start).then_some(created)
}

/// Image file name of an open process, such as `chrome.exe`.
pub fn image_name(handle: &Handle) -> Option<String> {
    let mut image = vec![0u16; 32_768];
    let mut len = image.len() as u32;
    // SAFETY: the handle has query access and the buffer holds `len` UTF-16 units.
    unsafe {
        QueryFullProcessImageNameW(
            handle.0,
            PROCESS_NAME_WIN32,
            PWSTR(image.as_mut_ptr()),
            &mut len,
        )
    }
    .ok()?;
    let image = String::from_utf16_lossy(&image[..len as usize]);
    Some(image.rsplit('\\').next().unwrap_or_default().to_string())
}

/// True when the live image name still matches the sampled one and still passes the gate,
/// so a reused pid or a protected process never gets a write-capable handle. Pure.
pub fn live_name_ok(
    pid: u32,
    sampled: &str,
    live: &str,
    parent: Option<&str>,
    own_pid: u32,
) -> bool {
    live.eq_ignore_ascii_case(sampled)
        && skip_reason(pid, live, parent, own_pid, &HashSet::new()).is_none()
}

/// True when the process behind the pinned pid is the target the snapshot saw (same creation
/// time and image name) and it passes the gate with the parent sampled for that same process.
/// Another creation time means the pid was reused, so the sampled parent says nothing about
/// the live process: refused. `live_created` and `live_name` are read through a query-only
/// handle that stays open until the write-capable handle exists. Pure.
pub fn live_target_ok(
    target: &TrimTarget,
    live_created: Option<u64>,
    live_name: Option<&str>,
    own_pid: u32,
) -> bool {
    target.created.is_some()
        && target.created == live_created
        && live_name.is_some_and(|live| {
            live_name_ok(
                target.pid,
                &target.name,
                live,
                target.parent.as_deref(),
                own_pid,
            )
        })
}

fn classify(e: &windows::core::Error) -> TrimError {
    if e.code() == HRESULT::from_win32(ERROR_ACCESS_DENIED.0) {
        TrimError::AccessDenied
    } else {
        TrimError::Failed
    }
}

fn open(pid: u32, access: PROCESS_ACCESS_RIGHTS) -> Result<Handle, TrimError> {
    // SAFETY: plain FFI call; the returned handle is wrapped for closing right away.
    unsafe { OpenProcess(access, false, pid) }
        .map(Handle)
        .map_err(|e| classify(&e))
}

fn working_set(handle: &Handle) -> Option<u64> {
    let mut counters = PROCESS_MEMORY_COUNTERS::default();
    let size = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    counters.cb = size;
    // SAFETY: `counters` is a valid, correctly sized out-parameter for the call's duration.
    unsafe { GetProcessMemoryInfo(handle.0, &mut counters, size) }.ok()?;
    Some(counters.WorkingSetSize as u64)
}

/// Trims one process as the snapshot saw it. A query-only handle pins the pid first; its
/// creation time and image name must match the snapshot, and the gate runs with the parent
/// sampled for that same process, before any write-capable handle exists.
pub fn trim_process(target: &TrimTarget) -> Result<Trimmed, TrimError> {
    let query = open(target.pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let live_created = creation_time(query.0).ok();
    let live = image_name(&query).ok_or(TrimError::Failed)?;
    if !live_target_ok(target, live_created, Some(&live), std::process::id()) {
        return Err(TrimError::Refused);
    }
    // While `query` is open the pid cannot be reused, so this is the process checked above.
    let handle = open(
        target.pid,
        PROCESS_SET_QUOTA | PROCESS_QUERY_LIMITED_INFORMATION,
    )?;
    drop(query);

    let before = working_set(&handle).ok_or(TrimError::Failed)?;
    // SAFETY: the handle is valid and was opened with PROCESS_SET_QUOTA.
    unsafe { EmptyWorkingSet(handle.0) }.map_err(|e| classify(&e))?;
    let after = working_set(&handle).ok_or(TrimError::Failed)?;
    Ok(Trimmed { before, after })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none() -> HashSet<String> {
        HashSet::new()
    }

    #[test]
    fn protects_system_and_shell_processes_case_insensitively() {
        assert_eq!(
            skip_reason(4, "System", None, 999, &none()),
            Some(SkipReason::System)
        );
        assert_eq!(
            skip_reason(500, "CSRSS.exe", None, 999, &none()),
            Some(SkipReason::System)
        );
        assert_eq!(
            skip_reason(600, "Explorer.EXE", None, 999, &none()),
            Some(SkipReason::System)
        );
        assert_eq!(
            skip_reason(0, "anything", None, 999, &none()),
            Some(SkipReason::System)
        );
    }

    #[test]
    fn own_pid_is_skipped_even_if_excluded() {
        let ex: HashSet<String> = ["toolboks.exe".to_string()].into();
        assert_eq!(
            skip_reason(77, "toolboks.exe", None, 77, &ex),
            Some(SkipReason::Own)
        );
    }

    #[test]
    fn honours_exclusions_by_lowercase_name() {
        let ex: HashSet<String> = ["chrome.exe".to_string()].into();
        assert_eq!(
            skip_reason(10, "Chrome.exe", None, 999, &ex),
            Some(SkipReason::Excluded)
        );
        assert_eq!(skip_reason(11, "firefox.exe", None, 999, &ex), None);
    }

    #[test]
    fn game_and_anti_cheat_processes_are_protected_even_when_excluded() {
        let ex: HashSet<String> = ["cs2.exe".to_string()].into();
        assert_eq!(
            skip_reason(20, "CS2.exe", None, 999, &ex),
            Some(SkipReason::Protected(Protected::KnownGame))
        );
        let reason = skip_reason(21, "EasyAntiCheat.exe", None, 999, &none());
        assert_eq!(reason, Some(SkipReason::Protected(Protected::AntiCheat)));
        assert_eq!(
            reason.unwrap().describe(),
            "Protected: anti-cheat process, Toolboks leaves it alone."
        );
        assert_eq!(skip_reason(22, "chrome.exe", None, 999, &none()), None);
    }

    #[test]
    fn manual_trim_gate_protects_children_of_launchers() {
        assert_eq!(
            skip_reason(30, "IndieGame.exe", Some("steam.exe"), 999, &none()),
            Some(SkipReason::Protected(Protected::ChildOfLauncher))
        );
        assert_eq!(
            skip_reason(31, "notepad.exe", Some("explorer.exe"), 999, &none()),
            None
        );
    }

    #[test]
    fn live_name_gate_rejects_changed_or_protected_names() {
        assert!(live_name_ok(40, "Chrome.exe", "chrome.exe", None, 999));
        // The pid now belongs to another program.
        assert!(!live_name_ok(40, "chrome.exe", "cs2.exe", None, 999));
        assert!(!live_name_ok(40, "chrome.exe", "notepad.exe", None, 999));
        // Same name, but now protected through its launcher parent.
        assert!(!live_name_ok(
            40,
            "game.exe",
            "game.exe",
            Some("EpicGamesLauncher.exe"),
            999
        ));
        assert!(!live_name_ok(40, "vgc.exe", "vgc.exe", None, 999));
    }

    fn target(pid: u32, created: Option<u64>, name: &str, parent: Option<&str>) -> TrimTarget {
        TrimTarget {
            pid,
            created,
            name: name.to_string(),
            parent: parent.map(str::to_string),
        }
    }

    #[test]
    fn live_target_gate_ties_the_check_to_the_snapshot_process() {
        // Unlisted process, unlisted parent, same creation time: allowed.
        let plain = target(50, Some(7), "notepad.exe", Some("explorer.exe"));
        assert!(live_target_ok(&plain, Some(7), Some("Notepad.exe"), 999));
        // The pid was reused by a same-named process (perhaps one a launcher started): the
        // sampled parent belongs to the old process, so the new one is refused.
        assert!(!live_target_ok(&plain, Some(8), Some("notepad.exe"), 999));
        // Creation time unknown on either side: refused.
        assert!(!live_target_ok(&plain, None, Some("notepad.exe"), 999));
        let unknown = target(50, None, "notepad.exe", None);
        assert!(!live_target_ok(&unknown, None, Some("notepad.exe"), 999));
        // No live name: refused.
        assert!(!live_target_ok(&plain, Some(7), None, 999));
        // Started by a listed launcher: refused even when nothing else changed.
        let child = target(51, Some(7), "IndieGame.exe", Some("steam.exe"));
        assert!(!live_target_ok(&child, Some(7), Some("IndieGame.exe"), 999));
        // A listed game under an unlisted parent: refused.
        let game = target(52, Some(7), "cs2.exe", Some("explorer.exe"));
        assert!(!live_target_ok(&game, Some(7), Some("cs2.exe"), 999));
    }

    #[test]
    fn snapshot_creation_time_must_predate_the_snapshot() {
        // 2026-01-01T00:00:00Z in Unix seconds, and half a second later as a FILETIME.
        let secs = 1_767_225_600u64;
        let created = (secs + 11_644_473_600) * 10_000_000 + 5_000_000;
        assert!(born_before_snapshot(created, secs, created + 1));
        // Started when or after the snapshot began: the pid was reused.
        assert!(!born_before_snapshot(created, secs, created));
        assert!(!born_before_snapshot(created, secs, created - 1));
        // The snapshot reported another second: a different process.
        assert!(!born_before_snapshot(created, secs + 1, created + 1));
        assert!(!born_before_snapshot(0, 0, 1));
    }

    /// Creation time of an own child process, read the same way the snapshot does.
    fn child_created(child: &std::process::Child) -> Option<u64> {
        open(child.id(), PROCESS_QUERY_LIMITED_INFORMATION)
            .ok()
            .and_then(|h| creation_time(h.0).ok())
    }

    #[test]
    fn snapshot_created_vouches_only_for_processes_older_than_the_snapshot() {
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "ping -n 6 127.0.0.1 > nul"])
            .spawn()
            .expect("spawn child");
        let pid = child.id();
        let created = child_created(&child);
        let after = snapshot_start();
        let result = created.map(|c| {
            let secs = c / 10_000_000 - 11_644_473_600;
            (
                snapshot_created(pid, secs, after),
                snapshot_created(pid, secs, c),
            )
        });
        let _ = child.kill();
        let _ = child.wait();
        let (later, earlier) = result.expect("creation time of child");
        assert_eq!(later, created);
        assert_eq!(earlier, None, "a snapshot older than the process");
    }

    #[test]
    fn refuses_a_child_whose_creation_time_differs() {
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "ping -n 6 127.0.0.1 > nul"])
            .spawn()
            .expect("spawn child");
        let created = child_created(&child);
        let stale = target(child.id(), created.map(|c| c - 1), "cmd.exe", None);
        let result = trim_process(&stale);
        let _ = child.kill();
        let _ = child.wait();
        assert!(created.is_some());
        assert_eq!(result, Err(TrimError::Refused));
    }

    #[test]
    fn trims_a_child_process() {
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "ping -n 6 127.0.0.1 > nul"])
            .spawn()
            .expect("spawn child");
        let own = target(child.id(), child_created(&child), "cmd.exe", None);
        let result = trim_process(&own);
        let _ = child.kill();
        let _ = child.wait();

        let trimmed = result.expect("trim own child process");
        assert!(trimmed.before > 0, "child had no working set");
        assert!(
            trimmed.after <= trimmed.before,
            "working set grew: {trimmed:?}"
        );
    }

    #[test]
    fn missing_process_fails_cleanly() {
        // Pids are multiples of 4 on Windows, so this one is never valid.
        assert!(trim_process(&target(u32::MAX - 1, Some(1), "gone.exe", None)).is_err());
    }
}
