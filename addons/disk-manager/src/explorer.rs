//! Windows Shell item IDs preserve Unicode and long paths without a command shell.
use std::path::{Path, PathBuf};
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
    let (target, missing) = target(Path::new(path))?;
    open(&target, !missing)?;
    Ok(if missing {
        "This item no longer exists. Opened its parent folder instead.".into()
    } else {
        "Opened the selected item in Explorer.".into()
    })
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
