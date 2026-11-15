//! Per-process working-set trim and the policy for which processes may be trimmed.
//!
//! `EmptyWorkingSet` needs no admin rights for processes of the same user. The trimmed pages
//! move to the standby list and fault back in when the process touches them again.

use std::collections::HashSet;

use windows::core::{HRESULT, PWSTR};
use windows::Win32::Foundation::ERROR_ACCESS_DENIED;
use windows::Win32::System::ProcessStatus::{
    EmptyWorkingSet, GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_ACCESS_RIGHTS, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA,
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
    /// The live process is protected or no longer has the sampled name. No write-capable
    /// handle was opened.
    Refused,
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

/// Trims one process. `name` and `parent` are what the caller sampled; the live image name is
/// checked with a query-only handle before any write-capable handle exists.
pub fn trim_process(pid: u32, name: &str, parent: Option<&str>) -> Result<Trimmed, TrimError> {
    let query = open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let live = image_name(&query).ok_or(TrimError::Failed)?;
    if !live_name_ok(pid, name, &live, parent, std::process::id()) {
        return Err(TrimError::Refused);
    }
    // While `query` is open the pid cannot be reused, so this is the process checked above.
    let handle = open(pid, PROCESS_SET_QUOTA | PROCESS_QUERY_LIMITED_INFORMATION)?;
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

    #[test]
    fn trims_a_child_process() {
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "ping -n 6 127.0.0.1 > nul"])
            .spawn()
            .expect("spawn child");
        let result = trim_process(child.id(), "cmd.exe", None);
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
        assert!(trim_process(u32::MAX - 1, "gone.exe", None).is_err());
    }
}
