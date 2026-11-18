//! Windows Shell item IDs preserve Unicode and long paths without a command shell.
//!
//! Scans store canonical paths, which on Windows carry the extended-length prefix (`\\?\D:\`).
//! The Shell does not parse that prefix, so "Show in Explorer" failed for every scanned item.
//! Paths are turned back into the ordinary form first; `explorer.exe /select,` is the fallback
//! when the Shell call itself fails.
use std::path::{Path, PathBuf};
/// The ordinary Windows form of a scanned path: no `\\?\` prefix, backslashes, and a drive
/// root always ends in a backslash (`D:` alone means the current folder on D:).
pub fn shell_path(path: &str) -> String {
    let path = path.trim().replace('/', "\\");
    let path = if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        path
    };
    if is_drive_root(&path) && !path.ends_with('\\') {
        format!("{path}\\")
    } else {
        path
    }
}
/// `D:` or `D:\` (after [`shell_path`]).
pub fn is_drive_root(path: &str) -> bool {
    let b = path.as_bytes();
    b.len() >= 2
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && (b.len() == 2 || (b.len() == 3 && b[2] == b'\\'))
}
/// The single argument for `explorer.exe`. A drive root is opened; anything else is selected
/// in its folder. Windows file names cannot contain quotes, so quoting is always safe.
pub fn explorer_arg(path: &str, select: bool) -> String {
    if is_drive_root(path) {
        path.to_owned()
    } else if select {
        format!("/select,\"{path}\"")
    } else {
        format!("\"{path}\"")
    }
}
fn target(path: &Path) -> Result<(PathBuf, bool), String> {
    if path.exists() {
        return Ok((path.to_owned(), false));
    }
    if let Some(parent) = path.parent().filter(|p| p.is_dir()) {
        return Ok((parent.to_owned(), true));
    }
    Err("This item and its parent are no longer available. Run a new scan.".into())
}
pub fn show(path: &str) -> Result<String, String> {
    let (target, missing) = target(Path::new(&shell_path(path)))?;
    let target = target.to_string_lossy().into_owned();
    // A drive root is opened; selecting it would only highlight it in "This PC".
    let select = !missing && !is_drive_root(&target);
    if open(Path::new(&target), select).is_err() {
        launch(&explorer_arg(&target, select))?;
    }
    Ok(if missing {
        "This item no longer exists. Opened its parent folder instead.".into()
    } else {
        "Opened the selected item in Explorer.".into()
    })
}
#[cfg(windows)]
fn launch(arg: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    // Explorer parses its own command line, so the argument goes in exactly as built. Its exit
    // code is 1 even on success; only a failure to start counts.
    std::process::Command::new("explorer.exe")
        .raw_arg(arg)
        .spawn()
        .map(drop)
        .map_err(|e| format!("Explorer could not be started: {e}"))
}
#[cfg(not(windows))]
fn launch(_: &str) -> Result<(), String> {
    Err("Show in Explorer is available on Windows.".into())
}
#[cfg(windows)]
fn open(path: &Path, select: bool) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        core::PCWSTR,
        Win32::{
            System::Com::{
                CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED,
            },
            UI::Shell::{
                ILClone, ILFindLastID, ILFree, ILRemoveLastID, SHOpenFolderAndSelectItems,
                SHParseDisplayName,
            },
        },
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: COM is balanced when initialized here; all PIDLs remain live through the Shell call.
    unsafe {
        let initialized = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
        let result = (|| {
            let mut item = std::ptr::null_mut();
            SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut item, 0, None)
                .map_err(|e| format!("Explorer could not open this path: {e}"))?;
            let result = if select {
                let parent = ILClone(item);
                if parent.is_null() {
                    CoTaskMemFree(Some(item.cast()));
                    return Err("Explorer could not prepare this item.".into());
                }
                let child = ILFindLastID(item);
                let _ = ILRemoveLastID(Some(parent));
                let result = SHOpenFolderAndSelectItems(parent, Some(&[child]), 0);
                ILFree(Some(parent));
                result
            } else {
                SHOpenFolderAndSelectItems(item, None, 0)
            };
            CoTaskMemFree(Some(item.cast()));
            result.map_err(|e| format!("Explorer could not open this item: {e}"))
        })();
        if initialized {
            CoUninitialize();
        }
        result
    }
}
#[cfg(not(windows))]
fn open(_: &Path, _: bool) -> Result<(), String> {
    Err("Show in Explorer is available on Windows.".into())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    #[test]
    fn shell_parses_long_unicode_paths_without_launching_explorer() {
        use std::os::windows::ffi::OsStrExt;
        use windows::{
            core::PCWSTR,
            Win32::{
                System::Com::{
                    CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED,
                },
                UI::Shell::SHParseDisplayName,
            },
        };
        // Removed when the test ends, also when it fails or panics.
        let tmp = tempfile::Builder::new()
            .prefix("toolboks-shell-long-")
            .tempdir()
            .unwrap();
        let mut dir = tmp.path().to_path_buf();
        for _ in 0..5 {
            dir = dir.join("long directory with spaces abcdefghijklmnopqrstuvwxyz");
        }
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("\u{6f22}\u{5b57} \u{e6}.txt");
        std::fs::write(&path, b"test").unwrap();
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        assert!(wide.len() > 260);
        unsafe {
            let initialized = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
            let mut item = std::ptr::null_mut();
            let result = SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut item, 0, None);
            if !item.is_null() {
                CoTaskMemFree(Some(item.cast()));
            }
            if initialized {
                CoUninitialize();
            }
            result.unwrap();
        }
    }
    #[test]
    fn extended_length_paths_become_shell_paths() {
        assert_eq!(shell_path(r"\\?\D:\"), r"D:\");
        assert_eq!(shell_path(r"\\?\D:"), r"D:\");
        assert_eq!(shell_path("D:"), r"D:\");
        assert_eq!(
            shell_path(r"\\?\C:\Program Files (x86)\Steam\a, b & c.txt"),
            r"C:\Program Files (x86)\Steam\a, b & c.txt"
        );
        assert_eq!(
            shell_path(r"\\?\UNC\server\share\folder"),
            r"\\server\share\folder"
        );
        assert_eq!(shell_path("C:/Users/me/My Files"), r"C:\Users\me\My Files");
        assert_eq!(
            shell_path("\\\\?\\E:\\\u{6f22}\u{5b57} \u{e6}\u{f8}"),
            "E:\\\u{6f22}\u{5b57} \u{e6}\u{f8}"
        );
        assert!(is_drive_root(r"D:\") && is_drive_root("d:"));
        assert!(!is_drive_root(r"D:\x") && !is_drive_root(r"\\server\share"));
    }
    #[test]
    fn explorer_arguments_select_items_and_open_drive_roots() {
        assert_eq!(explorer_arg(r"D:\", true), r"D:\");
        assert_eq!(
            explorer_arg(r"D:\SteamLibrary", true),
            r#"/select,"D:\SteamLibrary""#
        );
        assert_eq!(
            explorer_arg(r"C:\My Games\a, b; c (1).txt", true),
            r#"/select,"C:\My Games\a, b; c (1).txt""#
        );
        assert_eq!(explorer_arg(r"C:\My Games", false), r#""C:\My Games""#);
    }
    #[cfg(windows)]
    #[test]
    fn canonical_scan_paths_are_found_after_cleaning() {
        let tmp = tempfile::Builder::new()
            .prefix("toolboks-explorer-canon-")
            .tempdir()
            .unwrap();
        let file = tmp.path().join("a file, with spaces.txt");
        std::fs::write(&file, b"x").unwrap();
        let canonical = std::fs::canonicalize(&file).unwrap();
        let raw = canonical.to_string_lossy();
        assert!(raw.starts_with(r"\\?\"), "scans keep this form: {raw}");
        let clean = shell_path(&raw);
        assert!(!clean.starts_with(r"\\?\"));
        assert_eq!(
            target(Path::new(&clean)).unwrap(),
            (PathBuf::from(&clean), false)
        );
    }
    #[test]
    fn missing_item_falls_back_only_to_existing_parent() {
        let tmp = tempfile::Builder::new()
            .prefix("toolboks-explorer-")
            .tempdir()
            .unwrap();
        let root = tmp.path().to_path_buf();
        let path = root.join("spaces and \u{e6}\u{f8} \u{6f22}\u{5b57}.txt");
        assert_eq!(target(&path).unwrap(), (root.clone(), true));
        std::fs::write(&path, b"test").unwrap();
        assert_eq!(target(&path).unwrap(), (path.clone(), false));
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(&root).unwrap();
        assert!(target(&root.join("missing")).is_err());
    }
}
