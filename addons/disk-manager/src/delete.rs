//! "Delete permanently" from the list and the space map. Deletes with `std::fs` as the signed-in
//! user (the add-on never runs as administrator), skips the Recycle Bin, and refuses drive
//! roots, the scanned folder itself, Windows and Program Files. After a delete the scan data is
//! updated in place so the list, map, totals and saved scan agree without a rescan.
use crate::scanner::{Extension, Node, Progress};
use std::path::Path;

pub fn menu_label() -> egui::RichText {
    egui::RichText::new("Delete permanently").color(toolboks_shared::ui::RED)
}

/// The fill of the dialog's confirm button: a deep red that keeps white text above 4.5:1.
pub const DANGER_FILL: egui::Color32 = egui::Color32::from_rgb(0xb4, 0x2c, 0x25);

/// One pending or running delete, shown in the confirmation dialog.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub path: String,
    pub dir: bool,
    pub bytes: u64,
    pub files: u64,
    /// The scan the entry came from; a newer scan is rescanned instead of patched.
    pub generation: u64,
}

fn key(path: &str) -> String {
    crate::explorer::shell_path(path)
        .trim_end_matches('\\')
        .to_lowercase()
}
fn inside(path: &str, folder: &str) -> bool {
    let (path, folder) = (key(path), key(folder));
    !folder.is_empty() && (path == folder || path.starts_with(&format!("{folder}\\")))
}
/// Windows and Program Files, from the environment with the usual defaults.
fn protected_folders() -> Vec<String> {
    let system = std::env::var("SystemRoot")
        .or_else(|_| std::env::var("windir"))
        .unwrap_or_else(|_| r"C:\Windows".into());
    let drive = system.get(..2).unwrap_or("C:").to_owned();
    let mut out = vec![system];
    for (var, fallback) in [
        ("ProgramFiles", r"\Program Files"),
        ("ProgramFiles(x86)", r"\Program Files (x86)"),
        ("ProgramW6432", r"\Program Files"),
    ] {
        out.push(std::env::var(var).unwrap_or_else(|_| format!("{drive}{fallback}")));
    }
    out
}
/// Why this entry may not be deleted, in plain words; `None` when it may.
pub fn refusal(path: &str, scan_root: &str) -> Option<String> {
    refusal_with(path, scan_root, &protected_folders())
}
fn refusal_with(path: &str, scan_root: &str, protected: &[String]) -> Option<String> {
    let clean = crate::explorer::shell_path(path);
    let parent = Path::new(&clean).parent();
    if crate::explorer::is_drive_root(&clean)
        || parent.is_none()
        || (clean.starts_with(r"\\") && clean.trim_end_matches('\\').matches('\\').count() <= 3)
    {
        return Some(format!(
            "{clean} is a whole drive. Disk Manager does not delete drives."
        ));
    }
    if key(path) == key(scan_root) {
        return Some(format!(
            "{clean} is the folder you scanned. Delete what is inside it, or delete it in Explorer."
        ));
    }
    if let Some(folder) = protected.iter().find(|f| inside(path, f)) {
        return Some(format!(
            "{clean} is part of {}. Disk Manager does not delete Windows or installed programs. Use Settings > Apps to remove a program.",
            crate::explorer::shell_path(folder)
        ));
    }
    None
}

/// Deletes the file or folder for good. Runs on a worker thread; a large folder takes a while.
pub fn delete(request: &Request) -> Result<(), String> {
    let path = Path::new(&request.path);
    let shown = crate::explorer::shell_path(&request.path);
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(explain(&shown, &e, false)),
    };
    if meta.is_dir() != request.dir {
        return Err(format!(
            "{shown} changed since the scan. Nothing was deleted. Run a rescan and try again."
        ));
    }
    let result = if meta.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    match result {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(explain(&shown, &e, request.dir && path.exists())),
    }
}
fn explain(shown: &str, e: &std::io::Error, partial: bool) -> String {
    let what = match (e.kind(), e.raw_os_error()) {
        (_, Some(32 | 33)) => "is in use by another program. Close that program and try again.".to_owned(),
        (std::io::ErrorKind::PermissionDenied, _) | (_, Some(5)) => {
            "could not be deleted: access denied. It may be read-only, in use, or need administrator rights; Disk Manager never runs as administrator.".to_owned()
        }
        _ => format!("could not be deleted: {e}."),
    };
    if partial {
        format!("{shown} {what} Some files inside it may already be gone. Run a rescan to see what is left.")
    } else {
        format!("{shown} {what}")
    }
}

/// The scan without the entries at `paths` and everything under them (one entry after a
/// single delete, many after a duplicates delete), in one pass. Totals of every parent, the
/// scan totals and the file-type list shrink to match. Returns the new nodes and a map from
/// old ids to new ids, or `None` when none of the paths is in the scan (the root never is).
pub fn remove_all(
    progress: &mut Progress,
    nodes: &[Node],
    paths: &[String],
) -> Option<(Vec<Node>, Vec<Option<usize>>)> {
    let wanted: std::collections::HashSet<&str> = paths.iter().map(String::as_str).collect();
    let mut target = vec![false; nodes.len()];
    let mut first = None;
    for n in nodes.iter().skip(1) {
        if n.id < nodes.len() && wanted.contains(n.path.as_str()) && !target[n.id] {
            target[n.id] = true;
            first.get_or_insert(n.id);
        }
    }
    let first = first?;
    // Parents always come before their children, so one pass marks every subtree. A target
    // inside another target is gone with it and is not counted twice.
    let mut gone = vec![false; nodes.len()];
    let mut tops = vec![];
    let mut ext: std::collections::HashMap<String, (u64, u64)> = Default::default();
    let mut folders = 0u64;
    for n in &nodes[first..] {
        let under = n.parent.is_some_and(|p| gone[p]);
        if target[n.id] && !under {
            tops.push(n.id);
        }
        if target[n.id] || under {
            gone[n.id] = true;
            if n.dir {
                folders += 1;
            } else {
                let name = Path::new(&n.name)
                    .extension()
                    .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
                    .unwrap_or("No extension".into());
                let e = ext.entry(name).or_default();
                e.0 = e.0.saturating_add(n.bytes);
                e.1 += 1;
            }
        }
    }
    let mut map = vec![None; nodes.len()];
    let mut out = Vec::with_capacity(nodes.len());
    for n in nodes.iter().filter(|n| !gone[n.id]) {
        map[n.id] = Some(out.len());
        let mut n = n.clone();
        n.id = out.len();
        n.parent = n.parent.and_then(|p| map[p]);
        out.push(n);
    }
    let (mut bytes, mut files) = (0u64, 0u64);
    for &target in &tops {
        let (b, f) = (nodes[target].bytes, nodes[target].files);
        bytes = bytes.saturating_add(b);
        files = files.saturating_add(f);
        let mut ancestor = nodes[target].parent;
        while let Some(id) = ancestor {
            let n = &mut out[map[id]?];
            n.bytes = n.bytes.saturating_sub(b);
            n.files = n.files.saturating_sub(f);
            ancestor = nodes[id].parent;
        }
    }
    progress.bytes = progress.bytes.saturating_sub(bytes);
    progress.files = progress.files.saturating_sub(files);
    progress.folders = progress.folders.saturating_sub(folders);
    progress.extensions = std::mem::take(&mut progress.extensions)
        .into_iter()
        .filter_map(|e| {
            let (b, f) = ext.get(&e.name).copied().unwrap_or_default();
            let left = Extension {
                bytes: e.bytes.saturating_sub(b),
                files: e.files.saturating_sub(f),
                name: e.name,
            };
            (left.files > 0).then_some(left)
        })
        .collect();
    progress
        .extensions
        .sort_by_key(|e| std::cmp::Reverse(e.bytes));
    Some((out, map))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::State;
    use std::sync::{atomic::AtomicBool, Arc, Mutex};

    fn scan(root: &Path) -> (Progress, Vec<Node>) {
        let state = Arc::new(Mutex::new(State::default()));
        crate::scanner::scan(
            root.to_path_buf(),
            vec![],
            state.clone(),
            Arc::new(AtomicBool::new(false)),
        );
        let s = state.lock().unwrap();
        (s.progress.clone(), s.nodes.to_vec())
    }
    fn remove(
        progress: &mut Progress,
        nodes: &[Node],
        path: &str,
    ) -> Option<(Vec<Node>, Vec<Option<usize>>)> {
        remove_all(progress, nodes, &[path.to_owned()])
    }
    fn request(n: &Node, generation: u64) -> Request {
        Request {
            path: n.path.clone(),
            dir: n.dir,
            bytes: n.bytes,
            files: n.files,
            generation,
        }
    }

    #[test]
    fn refuses_drives_the_scan_root_windows_and_program_files() {
        let protected = [
            r"C:\Windows".to_owned(),
            r"C:\Program Files".to_owned(),
            r"C:\Program Files (x86)".to_owned(),
        ];
        let refused = |path: &str| refusal_with(path, r"\\?\D:\Scans", &protected);
        for path in [
            r"\\?\D:\",
            r"D:",
            r"\\?\UNC\server\share",
            r"\\?\D:\Scans",
            r"d:\scans\",
            r"\\?\C:\Windows",
            r"C:\windows\System32\drivers",
            r"\\?\C:\Program Files",
            r"C:\Program Files (x86)\Steam\steam.exe",
        ] {
            assert!(refused(path).is_some(), "{path} must be refused");
        }
        for path in [
            r"\\?\D:\Scans\old game",
            r"C:\Windows.old",
            r"C:\Program Files Backup\x",
            r"\\?\UNC\server\share\folder",
        ] {
            assert_eq!(refused(path), None, "{path} may be deleted");
        }
        assert!(refused(r"\\?\D:\").unwrap().contains("whole drive"));
        assert!(refused(r"C:\Windows\x").unwrap().contains(r"C:\Windows"));
    }

    #[test]
    fn deletes_files_and_folders_and_updates_every_total() {
        let tmp = tempfile::Builder::new()
            .prefix("toolboks-delete-")
            .tempdir()
            .unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("big folder/inner, deep")).unwrap();
        std::fs::write(root.join("big folder/a.bin"), [0; 40]).unwrap();
        std::fs::write(root.join("big folder/inner, deep/b.txt"), [0; 20]).unwrap();
        std::fs::write(root.join("keep.txt"), [0; 7]).unwrap();
        std::fs::write(root.join("old file.bin"), [0; 5]).unwrap();
        let (mut p, nodes) = scan(root);
        assert_eq!((p.bytes, p.files), (72, 4));

        let folder = nodes.iter().find(|n| n.name == "big folder").unwrap();
        assert!(refusal(&folder.path, &p.root).is_none());
        delete(&request(folder, p.generation)).unwrap();
        assert!(!root.join("big folder").exists());
        let (nodes, map) = remove(&mut p, &nodes, &folder.path).unwrap();
        assert_eq!(map[folder.id], None);
        assert_eq!((p.bytes, p.files, p.folders), (12, 2, 1));
        assert_eq!((nodes[0].bytes, nodes[0].files), (12, 2));
        assert_eq!(nodes.len(), 3);
        for (i, n) in nodes.iter().enumerate() {
            assert_eq!(n.id, i);
            assert!(n.parent.is_none_or(|parent| parent < i));
        }
        let ext = |p: &Progress, name: &str| {
            p.extensions
                .iter()
                .find(|e| e.name == name)
                .map(|e| (e.bytes, e.files))
        };
        assert_eq!(ext(&p, ".bin"), Some((5, 1)));
        assert_eq!(ext(&p, ".txt"), Some((7, 1)));

        let file = nodes
            .iter()
            .find(|n| n.name == "old file.bin")
            .unwrap()
            .clone();
        delete(&request(&file, p.generation)).unwrap();
        assert!(!root.join("old file.bin").exists());
        let (nodes, _) = remove(&mut p, &nodes, &file.path).unwrap();
        assert_eq!((p.bytes, p.files), (7, 1));
        assert_eq!(ext(&p, ".bin"), None);
        assert!(root.join("keep.txt").exists());

        // The patched scan still saves and loads: saved scans stay consistent.
        let saved = tmp.path().join("saved");
        crate::saved_scan::save(&saved, &p, &nodes).unwrap();
        // Deleting something that is already gone is not an error.
        delete(&request(&file, p.generation)).unwrap();
        assert!(
            remove(&mut p, &nodes, &nodes[0].path).is_none(),
            "never the root"
        );
    }

    #[test]
    fn several_entries_leave_in_one_pass_and_nested_ones_count_once() {
        let n = |id: usize, parent: Option<usize>, name: &str, dir: bool, bytes, files| Node {
            id,
            parent,
            name: name.into(),
            path: format!(r"D:\{name}"),
            dir,
            bytes,
            files,
            modified: 0,
            dir_stamp: 0,
        };
        let nodes = vec![
            n(0, None, "root", true, 111, 4),
            n(1, Some(0), "a", true, 101, 3),
            n(2, Some(1), "a1.bin", false, 1, 1),
            n(3, Some(1), "a2.bin", false, 100, 1),
            n(4, Some(1), "a3.txt", false, 0, 1),
            n(5, Some(0), "b.txt", false, 10, 1),
        ];
        let mut p = Progress {
            bytes: 111,
            files: 4,
            folders: 1,
            extensions: vec![
                Extension {
                    name: ".bin".into(),
                    bytes: 101,
                    files: 2,
                },
                Extension {
                    name: ".txt".into(),
                    bytes: 10,
                    files: 2,
                },
            ],
            ..Default::default()
        };
        let paths = [r"D:\a2.bin", r"D:\b.txt", r"D:\not there"].map(String::from);
        let (out, map) = remove_all(&mut p, &nodes, &paths).unwrap();
        assert_eq!(map, [Some(0), Some(1), Some(2), None, Some(3), None]);
        assert_eq!((out[0].bytes, out[0].files), (1, 2));
        assert_eq!((out[1].bytes, out[1].files), (1, 2));
        assert_eq!((p.bytes, p.files, p.folders), (1, 2, 1));
        assert_eq!(p.extensions.len(), 2);
        // A folder and a file inside it: the file is not taken off twice.
        let mut p2 = Progress {
            bytes: 111,
            files: 4,
            folders: 1,
            ..Default::default()
        };
        let paths = [r"D:\a2.bin", r"D:\a"].map(String::from);
        let (out, _) = remove_all(&mut p2, &nodes, &paths).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].bytes, out[0].files), (10, 1));
        assert_eq!((p2.bytes, p2.files, p2.folders), (10, 1, 0));
        assert!(remove_all(&mut p2, &nodes, &[r"D:\root".into()]).is_none());
    }

    #[test]
    fn a_type_change_since_the_scan_deletes_nothing() {
        let tmp = tempfile::Builder::new()
            .prefix("toolboks-delete-type-")
            .tempdir()
            .unwrap();
        let path = tmp.path().join("was a folder");
        std::fs::write(&path, b"x").unwrap();
        let error = delete(&Request {
            path: path.to_string_lossy().into_owned(),
            dir: true,
            bytes: 0,
            files: 0,
            generation: 1,
        })
        .unwrap_err();
        assert!(error.contains("changed since the scan"));
        assert!(path.exists());
    }

    #[cfg(windows)]
    #[test]
    fn a_file_in_use_reports_a_plain_error() {
        use std::os::windows::fs::OpenOptionsExt;
        let tmp = tempfile::Builder::new()
            .prefix("toolboks-delete-busy-")
            .tempdir()
            .unwrap();
        let path = tmp.path().join("busy.log");
        std::fs::write(&path, b"x").unwrap();
        // No FILE_SHARE_DELETE: Windows refuses to delete while this handle is open.
        let handle = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        let error = delete(&Request {
            path: path.to_string_lossy().into_owned(),
            dir: false,
            bytes: 1,
            files: 1,
            generation: 1,
        })
        .unwrap_err();
        assert!(
            error.contains("in use") || error.contains("access denied"),
            "{error}"
        );
        assert!(path.exists());
        drop(handle);
    }

    #[test]
    fn the_confirm_button_text_is_readable() {
        let contrast = |a: egui::Color32, b: egui::Color32| {
            let l = |c: egui::Color32| {
                let ch = |v: u8| {
                    let x = v as f64 / 255.0;
                    if x <= 0.04045 {
                        x / 12.92
                    } else {
                        ((x + 0.055) / 1.055).powf(2.4)
                    }
                };
                0.2126 * ch(c.r()) + 0.7152 * ch(c.g()) + 0.0722 * ch(c.b())
            };
            let (x, y) = (l(a), l(b));
            (x.max(y) + 0.05) / (x.min(y) + 0.05)
        };
        assert!(contrast(DANGER_FILL, egui::Color32::WHITE) >= 4.5);
    }
}
