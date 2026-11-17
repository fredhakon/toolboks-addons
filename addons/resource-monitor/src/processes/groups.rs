//! Apps, Background processes and Windows processes: the Task Manager style sections of the
//! Processes page, and the window scan that tells which processes are apps.

use std::collections::HashSet;

use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindow, GetWindowLongW, GetWindowThreadProcessId, IsWindowVisible, GWL_EXSTYLE,
    GW_OWNER, WS_EX_TOOLWINDOW,
};

/// The section a process belongs to, in the order the sections are shown.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(super) enum Kind {
    /// Owns a window you can switch to.
    App,
    /// Everything else that is not part of Windows.
    Background,
    /// Runs from the Windows folder, is one of the system processes, or has no readable path.
    Windows,
}

impl Kind {
    pub(super) const ALL: [Kind; 3] = [Kind::App, Kind::Background, Kind::Windows];

    pub(super) fn title(self) -> &'static str {
        match self {
            Kind::App => "Apps",
            Kind::Background => "Background processes",
            Kind::Windows => "Windows processes",
        }
    }
}

/// True when `path` is inside the folder `dir` (case-insensitive, whole folder names only).
pub(super) fn under_dir(path: &str, dir: &str) -> bool {
    let dir = dir.trim_end_matches(['\\', '/']);
    if dir.is_empty() {
        return false;
    }
    // `get` instead of slicing: a cut inside a multi-byte character is simply no match.
    let (Some(head), Some(rest)) = (path.get(..dir.len()), path.get(dir.len()..)) else {
        return false;
    };
    head.eq_ignore_ascii_case(dir) && rest.starts_with(['\\', '/'])
}

/// Sorts one process into a section. An app owns a window you can switch to, wherever it runs
/// from (Notepad and Explorer live in the Windows folder and are still apps). Otherwise it is a
/// Windows process when it runs from the Windows folder, is one of the system processes
/// (`system` is true for those), or Windows does not show its path; the rest is background.
pub(super) fn kind_of(
    exe: Option<&str>,
    has_window: bool,
    system: bool,
    windows_dir: Option<&str>,
) -> Kind {
    if has_window {
        return Kind::App;
    }
    let in_windows = match (exe, windows_dir) {
        (Some(exe), Some(dir)) => under_dir(exe, dir),
        _ => false,
    };
    if system || in_windows || exe.is_none() {
        Kind::Windows
    } else {
        Kind::Background
    }
}

/// The section of a merged row: an app if any of its processes is one, else background if any
/// is, else Windows (so all of Chrome sits under Apps, as in Task Manager).
pub(super) fn merge(a: Kind, b: Kind) -> Kind {
    a.min(b)
}

/// Processes that own a top-level window you can switch to: visible, not owned by another
/// window, not a tool window and not hidden by Windows (cloaked, like a suspended Store app's
/// frame). Only window state is read; no process is opened.
pub(super) fn window_owners() -> HashSet<u32> {
    unsafe extern "system" fn each(hwnd: HWND, data: LPARAM) -> BOOL {
        // SAFETY: `data` is the address of the set passed to EnumWindows below, which outlives
        // the enumeration, and EnumWindows calls back on this same thread.
        let owners = unsafe { &mut *(data.0 as *mut HashSet<u32>) };
        if is_switchable(hwnd) {
            let mut pid = 0u32;
            // SAFETY: plain query of a window handle Windows just handed us.
            unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
            if pid != 0 {
                owners.insert(pid);
            }
        }
        true.into()
    }
    let mut owners: HashSet<u32> = HashSet::new();
    // SAFETY: the callback only touches `owners` through the pointer, during this call.
    let _ = unsafe {
        EnumWindows(
            Some(each),
            LPARAM(&mut owners as *mut HashSet<u32> as isize),
        )
    };
    owners
}

fn is_switchable(hwnd: HWND) -> bool {
    // SAFETY: read-only queries of a window handle; a window that closed meanwhile just fails.
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() || GetWindow(hwnd, GW_OWNER).is_ok() {
            return false;
        }
        if GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0 {
            return false;
        }
        let mut cloaked = 0u32;
        let read = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            (&mut cloaked as *mut u32).cast(),
            std::mem::size_of::<u32>() as u32,
        );
        !(read.is_ok() && cloaked != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIN: Option<&str> = Some(r"C:\Windows");

    #[test]
    fn a_window_makes_an_app_wherever_it_runs_from() {
        let notepad = Some(r"C:\Windows\System32\notepad.exe");
        assert_eq!(kind_of(notepad, true, false, WIN), Kind::App);
        assert_eq!(
            kind_of(Some(r"D:\Games\x\game.exe"), true, false, WIN),
            Kind::App
        );
        // Even a process without a readable path.
        assert_eq!(kind_of(None, true, false, WIN), Kind::App);
    }

    #[test]
    fn windows_folder_system_names_and_unknown_paths_are_windows_processes() {
        let svc = Some(r"c:\windows\system32\svchost.exe");
        assert_eq!(kind_of(svc, false, false, WIN), Kind::Windows);
        assert_eq!(
            kind_of(Some(r"C:\Program Files\X\x.exe"), false, true, WIN),
            Kind::Windows
        );
        assert_eq!(kind_of(None, false, false, WIN), Kind::Windows);
    }

    #[test]
    fn the_rest_is_background() {
        let updater = Some(r"C:\Program Files\Vendor\updater.exe");
        assert_eq!(kind_of(updater, false, false, WIN), Kind::Background);
        // A folder that only starts like the Windows folder is not inside it.
        let lookalike = Some(r"C:\WindowsApps2\tool.exe");
        assert_eq!(kind_of(lookalike, false, false, WIN), Kind::Background);
        // Unknown Windows folder: only the system names and missing paths count.
        assert_eq!(kind_of(updater, false, false, None), Kind::Background);
    }

    #[test]
    fn folder_test_is_whole_names_and_case_insensitive() {
        assert!(under_dir(r"C:\WINDOWS\explorer.exe", r"C:\Windows"));
        assert!(under_dir(r"C:\Windows\a.exe", r"C:\Windows\"));
        assert!(!under_dir(r"C:\Windows", r"C:\Windows"));
        assert!(!under_dir(r"C:\Windows.old\a.exe", r"C:\Windows"));
        assert!(!under_dir(r"C:\a.exe", ""));
        // Multi-byte names never split a character.
        assert!(!under_dir("C:\\Wi\u{e9}ndows\\a.exe", r"C:\Windows"));
    }

    #[test]
    fn a_merged_row_takes_the_most_visible_section() {
        assert_eq!(merge(Kind::Background, Kind::App), Kind::App);
        assert_eq!(merge(Kind::Windows, Kind::Background), Kind::Background);
        assert_eq!(merge(Kind::Windows, Kind::Windows), Kind::Windows);
        let order: Vec<&str> = Kind::ALL.iter().map(|k| k.title()).collect();
        assert_eq!(order, ["Apps", "Background processes", "Windows processes"]);
    }

    #[test]
    fn the_window_scan_reads_without_failing() {
        // No assertion on the content: a test runner may have no desktop windows at all.
        let owners = window_owners();
        assert!(!owners.contains(&0));
    }
}
