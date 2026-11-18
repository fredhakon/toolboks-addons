//! Duplicate files in a scan (issue #187): which files have the same content, sorted, and the
//! extra copies deleted on request. Nothing is read until the owner asks: the scan only knows
//! names and sizes. Files of the same size get a quick hash of their first and last 64 KiB,
//! then a SHA-256 of their whole content; only equal full hashes are duplicates. Results live
//! in memory only. Deleting re-checks every file first and always leaves one copy.
use crate::delete;
use crate::scanner::Node;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    io::{Read, Seek, SeekFrom},
    path::Path,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

/// "Ignore files smaller than": 0 B, 1 MiB, 10 MiB, 100 MiB.
pub const MIN_OPTIONS: [u64; 4] = [0, 1 << 20, 10 << 20, 100 << 20];
pub const DEFAULT_MIN: u64 = 1 << 20;
/// The quick hash reads this much from each end of a file.
const EDGE: u64 = 64 * 1024;
const BUFFER: usize = 1 << 20;
/// Reparse points (OneDrive and other cloud placeholders), offline files and files that are
/// fetched from the cloud when opened or read. Reading them would download them.
const CLOUD_ATTRIBUTES: u32 = 0x400 | 0x1000 | 0x4_0000 | 0x40_0000;

/// Whether a path may be deleted: `None` when it may, else why not (drive roots, the scanned
/// folder, Windows, Program Files). Tests pass their own.
pub type Guard<'a> = &'a (dyn Fn(&str) -> Option<String> + Sync);

/// The guard of the real add-on: the refusals of the single delete.
pub fn guard(root: String) -> impl Fn(&str) -> Option<String> + Sync + Send {
    move |path: &str| delete::refusal(path, &root)
}

/// One file of a duplicate group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Copy {
    /// Its entry in the scan.
    pub node: usize,
    pub path: String,
    /// Modified, as in the scan (Unix seconds), for display.
    pub modified: u64,
    /// The full-resolution modified time read when it was hashed, checked again before a delete.
    pub stamp: u64,
    /// Why it may not be deleted (a protected folder); it can still be the kept copy.
    pub protected: Option<String>,
    /// It has more than one hard link: deleting this name frees no space, so it is never deleted.
    pub linked: bool,
}
impl Copy {
    pub fn deletable(&self) -> bool {
        self.protected.is_none() && !self.linked
    }
    pub fn name(&self) -> &str {
        file_name(&self.path)
    }
    pub fn folder(&self) -> String {
        folder_of(&self.path)
    }
}
fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}
fn folder_of(path: &str) -> String {
    Path::new(path)
        .parent()
        .map_or_else(String::new, |p| p.to_string_lossy().into_owned())
}

/// Files with the same content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    /// Unique in one search and kept through deletes; it also picks the group's map colour.
    pub id: u32,
    pub bytes: u64,
    pub hash: [u8; 32],
    /// By path, at least two.
    pub copies: Vec<Copy>,
    /// The copy that is kept: exactly one per group.
    pub keep: usize,
    /// Ticked: part of "Delete duplicates...".
    pub included: bool,
}
impl Group {
    /// Space that deleting every copy but one would free: size x (copies - 1).
    pub fn wasted(&self) -> u64 {
        self.bytes
            .saturating_mul(self.copies.len().saturating_sub(1) as u64)
    }
    /// The name of the first copy.
    pub fn name(&self) -> &str {
        self.copies.first().map_or("", |c| c.name())
    }
    pub fn folder(&self) -> String {
        self.copies.first().map_or_else(String::new, |c| c.folder())
    }
    /// Copies a delete of this group removes: every copy but the kept one that may be deleted.
    pub fn to_delete(&self) -> impl Iterator<Item = &Copy> {
        self.copies
            .iter()
            .enumerate()
            .filter(move |(i, c)| *i != self.keep && c.deletable())
            .map(|(_, c)| c)
    }
}

/// Sum of wasted space over groups.
pub fn wasted<'a>(groups: impl IntoIterator<Item = &'a Group>) -> u64 {
    groups
        .into_iter()
        .fold(0u64, |sum, g| sum.saturating_add(g.wasted()))
}

/// "Sort by".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortBy {
    #[default]
    Wasted,
    Size,
    Copies,
    Name,
    Folder,
}
impl SortBy {
    pub const ALL: [SortBy; 5] = [
        SortBy::Wasted,
        SortBy::Size,
        SortBy::Copies,
        SortBy::Name,
        SortBy::Folder,
    ];
    pub fn label(self) -> &'static str {
        match self {
            SortBy::Wasted => "Wasted space",
            SortBy::Size => "File size",
            SortBy::Copies => "Number of copies",
            SortBy::Name => "Name A to Z",
            SortBy::Folder => "Folder",
        }
    }
}
/// Sorts in place. Stable: equal groups stay in the order they were found.
pub fn sort(groups: &mut [Group], by: SortBy) {
    match by {
        SortBy::Wasted => groups.sort_by(|a, b| b.wasted().cmp(&a.wasted()).then(a.id.cmp(&b.id))),
        SortBy::Size => groups.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.id.cmp(&b.id))),
        SortBy::Copies => {
            groups.sort_by(|a, b| b.copies.len().cmp(&a.copies.len()).then(a.id.cmp(&b.id)))
        }
        SortBy::Name => groups.sort_by_cached_key(|g| (g.name().to_lowercase(), g.id)),
        SortBy::Folder => groups.sort_by_cached_key(|g| (g.folder().to_lowercase(), g.id)),
    }
}

/// "Keep": which copy of each group stays.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KeepRule {
    #[default]
    Oldest,
    Newest,
    ShortestPath,
    /// The copy inside the folder shown on the map; the oldest when none or several are.
    InFolder,
}
impl KeepRule {
    pub const ALL: [KeepRule; 4] = [
        KeepRule::Oldest,
        KeepRule::Newest,
        KeepRule::ShortestPath,
        KeepRule::InFolder,
    ];
    pub fn label(self) -> &'static str {
        match self {
            KeepRule::Oldest => "Oldest modified",
            KeepRule::Newest => "Newest modified",
            KeepRule::ShortestPath => "Shortest path",
            KeepRule::InFolder => "Copy in the current folder",
        }
    }
}
fn key(path: &str) -> String {
    crate::explorer::shell_path(path)
        .trim_end_matches('\\')
        .to_lowercase()
}
fn inside(path: &str, folder: &str) -> bool {
    let (path, folder) = (key(path), key(folder));
    !folder.is_empty() && path.starts_with(&format!("{folder}\\"))
}
/// The copy `rule` keeps. Ties go to the first path, so the choice never flickers.
pub fn keep_index(copies: &[Copy], rule: KeepRule, folder: Option<&str>) -> usize {
    let oldest = |list: &mut dyn Iterator<Item = (usize, &Copy)>| {
        list.min_by(|(_, a), (_, b)| a.stamp.cmp(&b.stamp).then(a.path.cmp(&b.path)))
            .map(|(i, _)| i)
    };
    let pick = match rule {
        KeepRule::Oldest => oldest(&mut copies.iter().enumerate()),
        KeepRule::Newest => copies
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.stamp.cmp(&b.stamp).then(b.path.cmp(&a.path)))
            .map(|(i, _)| i),
        KeepRule::ShortestPath => copies
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                a.path
                    .chars()
                    .count()
                    .cmp(&b.path.chars().count())
                    .then(a.path.cmp(&b.path))
            })
            .map(|(i, _)| i),
        KeepRule::InFolder => folder
            .and_then(|f| {
                oldest(
                    &mut copies
                        .iter()
                        .enumerate()
                        .filter(|(_, c)| inside(&c.path, f)),
                )
            })
            .or_else(|| oldest(&mut copies.iter().enumerate())),
    };
    pick.unwrap_or(0)
}
pub fn apply_rule(groups: &mut [Group], rule: KeepRule, folder: Option<&str>) {
    for g in groups {
        g.keep = keep_index(&g.copies, rule, folder);
    }
}

/// Progress of a search, read by the panel while it runs.
#[derive(Default)]
pub struct Counters {
    pub total: AtomicU64,
    pub checked: AtomicU64,
    pub bytes: AtomicU64,
}

/// What a search found, and what it could not read.
#[derive(Clone, Debug, Default)]
pub struct Found {
    pub generation: u64,
    pub min_bytes: u64,
    pub groups: Vec<Group>,
    /// Files of the minimum size or more (never 0 B).
    pub considered: u64,
    /// Files that share their size with another: the ones read.
    pub candidates: u64,
    pub checked: u64,
    pub bytes_read: u64,
    /// In use, access denied or another read error.
    pub unreadable: u64,
    /// Cloud placeholders and offline files, skipped so nothing is downloaded.
    pub cloud: u64,
    /// Changed or gone since the scan.
    pub changed: u64,
    /// Extra names of a file that is already in a group (hard links): they use no extra space.
    pub links_left_out: u64,
    /// Copies for which Windows did not say whether they are hard links of each other.
    pub links_unknown: u64,
    /// Stopped before every candidate was read.
    pub stopped: bool,
}

/// Files of `min_bytes` or more (never 0 B) grouped by exact size; only sizes shared by two or
/// more files. Largest size first; each group's files by path, so one folder is read in a row.
pub fn candidates(nodes: &[Node], min_bytes: u64) -> (u64, Vec<(u64, Vec<usize>)>) {
    let min = min_bytes.max(1);
    let mut by_size: HashMap<u64, Vec<usize>> = HashMap::new();
    let mut considered = 0;
    for n in nodes {
        if !n.dir && n.bytes >= min {
            considered += 1;
            by_size.entry(n.bytes).or_default().push(n.id);
        }
    }
    let mut groups: Vec<(u64, Vec<usize>)> =
        by_size.into_iter().filter(|(_, v)| v.len() > 1).collect();
    groups.sort_by_key(|g| std::cmp::Reverse(g.0));
    for (_, ids) in &mut groups {
        ids.sort_by(|a, b| nodes[*a].path.cmp(&nodes[*b].path));
    }
    (considered, groups)
}

enum Skip {
    Cloud,
    Changed,
    Unreadable,
    Stopped,
}
struct Opened {
    file: fs::File,
    stamp: u64,
    /// Volume and file number: the same pair is the same file under another name.
    id: Option<(u64, u64)>,
    links: Option<u32>,
}
#[cfg(windows)]
fn attributes(meta: &fs::Metadata) -> u32 {
    use std::os::windows::fs::MetadataExt;
    meta.file_attributes()
}
#[cfg(not(windows))]
fn attributes(_: &fs::Metadata) -> u32 {
    0
}
pub(crate) fn stamp(meta: &fs::Metadata) -> u64 {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.last_write_time()
    }
    #[cfg(not(windows))]
    {
        meta.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos().min(u64::MAX as u128) as u64)
    }
}
#[cfg(windows)]
fn identity(file: &fs::File) -> (Option<(u64, u64)>, Option<u32>) {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION},
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle belongs to `file`, which outlives the call; `info` is a valid out value.
    match unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) } {
        Ok(()) => (
            Some((
                u64::from(info.dwVolumeSerialNumber),
                (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
            )),
            Some(info.nNumberOfLinks),
        ),
        Err(_) => (None, None),
    }
}
#[cfg(not(windows))]
fn identity(file: &fs::File) -> (Option<(u64, u64)>, Option<u32>) {
    use std::os::unix::fs::MetadataExt;
    match file.metadata() {
        Ok(m) => (Some((m.dev(), m.ino())), Some(m.nlink() as u32)),
        Err(_) => (None, None),
    }
}
/// Opens a file for reading if it is still the size the scan saw and is fully on this PC.
fn open(path: &str, bytes: u64) -> Result<Opened, Skip> {
    let meta = fs::symlink_metadata(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => Skip::Changed,
        _ => Skip::Unreadable,
    })?;
    if attributes(&meta) & CLOUD_ATTRIBUTES != 0 {
        return Err(Skip::Cloud);
    }
    if !meta.is_file() || meta.len() != bytes {
        return Err(Skip::Changed);
    }
    let file = fs::File::open(path).map_err(|_| Skip::Unreadable)?;
    let meta = file.metadata().map_err(|_| Skip::Unreadable)?;
    if meta.len() != bytes {
        return Err(Skip::Changed);
    }
    let (id, links) = identity(&file);
    Ok(Opened {
        file,
        stamp: stamp(&meta),
        id,
        links,
    })
}
/// SHA-256 of `len` bytes from the start of the file, or of its first and last [`EDGE`] bytes
/// when `quick` and the file is longer than both ends together. A file that is not `len`
/// bytes long any more counts as changed.
fn hash(
    file: &mut fs::File,
    len: u64,
    quick: bool,
    cancel: &AtomicBool,
    counters: &Counters,
) -> Result<[u8; 32], Skip> {
    let mut h = Sha256::new();
    let mut buf = vec![0u8; BUFFER.min(len.max(1) as usize)];
    let mut read_exact = |file: &mut fs::File, mut want: u64, h: &mut Sha256| {
        while want > 0 {
            if cancel.load(Ordering::Relaxed) {
                return Err(Skip::Stopped);
            }
            let take = want.min(buf.len() as u64) as usize;
            let got = file.read(&mut buf[..take]).map_err(|_| Skip::Unreadable)?;
            if got == 0 {
                return Err(Skip::Changed);
            }
            h.update(&buf[..got]);
            counters.bytes.fetch_add(got as u64, Ordering::Relaxed);
            want -= got as u64;
        }
        Ok(())
    };
    if quick && len > 2 * EDGE {
        h.update(len.to_le_bytes());
        read_exact(file, EDGE, &mut h)?;
        file.seek(SeekFrom::Start(len - EDGE))
            .map_err(|_| Skip::Unreadable)?;
        read_exact(file, EDGE, &mut h)?;
    } else {
        read_exact(file, len, &mut h)?;
    }
    Ok(h.finalize().into())
}
/// Whether a quick hash already covers the whole file, so it is the full hash.
fn quick_is_full(len: u64) -> bool {
    len <= 2 * EDGE
}

/// One file that passed the quick step.
struct Read1 {
    node: usize,
    stamp: u64,
    id: Option<(u64, u64)>,
    links: Option<u32>,
}

/// Finds the duplicates among `nodes`. Reads files only; never changes anything. `cancel`
/// stops it between reads; what was found so far is returned with `stopped` set.
pub fn find(
    nodes: &[Node],
    generation: u64,
    min_bytes: u64,
    protected: Guard,
    cancel: &AtomicBool,
    counters: &Counters,
) -> Found {
    let (considered, sizes) = candidates(nodes, min_bytes);
    let mut found = Found {
        generation,
        min_bytes,
        considered,
        candidates: sizes.iter().map(|s| s.1.len() as u64).sum(),
        ..Default::default()
    };
    counters.total.store(found.candidates, Ordering::Relaxed);
    let mut groups: Vec<(u64, [u8; 32], Vec<Read1>)> = vec![];
    'sizes: for (len, ids) in sizes {
        // Quick step: the first and last 64 KiB (the whole file when it is that small).
        let mut quick: HashMap<[u8; 32], Vec<Read1>> = HashMap::new();
        let mut order = vec![];
        for id in ids {
            if cancel.load(Ordering::Relaxed) {
                found.stopped = true;
                break 'sizes;
            }
            let n = &nodes[id];
            let result = open(&n.path, len)
                .and_then(|mut o| hash(&mut o.file, len, true, cancel, counters).map(|h| (h, o)));
            match result {
                Ok((h, o)) => {
                    if !quick.contains_key(&h) {
                        order.push(h);
                    }
                    quick.entry(h).or_default().push(Read1 {
                        node: id,
                        stamp: o.stamp,
                        id: o.id,
                        links: o.links,
                    });
                }
                Err(Skip::Stopped) => {
                    found.stopped = true;
                    break 'sizes;
                }
                Err(Skip::Cloud) => found.cloud += 1,
                Err(Skip::Changed) => found.changed += 1,
                Err(Skip::Unreadable) => found.unreadable += 1,
            }
            found.checked += 1;
            counters.checked.fetch_add(1, Ordering::Relaxed);
        }
        // Full step: only files whose quick hashes match another's.
        for qh in order {
            let same = quick.remove(&qh).unwrap_or_default();
            if same.len() < 2 {
                continue;
            }
            if quick_is_full(len) {
                groups.push((len, qh, same));
                continue;
            }
            let mut full: HashMap<[u8; 32], Vec<Read1>> = HashMap::new();
            let mut full_order = vec![];
            for r in same {
                let n = &nodes[r.node];
                let result = open(&n.path, len).and_then(|mut o| {
                    if o.stamp != r.stamp {
                        return Err(Skip::Changed);
                    }
                    hash(&mut o.file, len, false, cancel, counters)
                });
                match result {
                    Ok(h) => {
                        if !full.contains_key(&h) {
                            full_order.push(h);
                        }
                        full.entry(h).or_default().push(r);
                    }
                    Err(Skip::Stopped) => {
                        found.stopped = true;
                        break 'sizes;
                    }
                    Err(Skip::Cloud) => found.cloud += 1,
                    Err(Skip::Changed) => found.changed += 1,
                    Err(Skip::Unreadable) => found.unreadable += 1,
                }
            }
            for fh in full_order {
                let same = full.remove(&fh).unwrap_or_default();
                if same.len() > 1 {
                    groups.push((len, fh, same));
                }
            }
        }
    }
    found.bytes_read = counters.bytes.load(Ordering::Relaxed);
    // Hard links: two names of one file are one copy, and saves nothing.
    let mut out = vec![];
    for (len, h, reads) in groups {
        let mut seen = std::collections::HashSet::new();
        let mut copies = vec![];
        for r in reads {
            match r.id {
                Some(id) if !seen.insert(id) => {
                    found.links_left_out += 1;
                    continue;
                }
                None => found.links_unknown += 1,
                _ => {}
            }
            let n = &nodes[r.node];
            copies.push(Copy {
                node: r.node,
                path: n.path.clone(),
                modified: n.modified,
                stamp: r.stamp,
                protected: protected(&n.path),
                linked: r.links.is_some_and(|l| l > 1),
            });
        }
        if copies.len() < 2 {
            continue;
        }
        copies.sort_by(|a, b| a.path.cmp(&b.path));
        out.push(Group {
            id: 0,
            bytes: len,
            hash: h,
            keep: keep_index(&copies, KeepRule::Oldest, None),
            copies,
            included: true,
        });
    }
    out.sort_by(|a, b| {
        b.wasted()
            .cmp(&a.wasted())
            .then_with(|| a.copies[0].path.cmp(&b.copies[0].path))
    });
    for (i, g) in out.iter_mut().enumerate() {
        g.id = i as u32;
    }
    found.groups = out;
    found
}

/// After entries left the scan: copies get their new ids, deleted copies leave their group and
/// groups left with one copy disappear. The kept copy stays the same file where it can.
pub fn remap(groups: &mut Vec<Group>, map: &[Option<usize>]) {
    for g in groups.iter_mut() {
        let kept = g.copies.get(g.keep).map(|c| c.path.clone());
        g.copies
            .retain_mut(|c| match map.get(c.node).copied().flatten() {
                Some(new) => {
                    c.node = new;
                    true
                }
                None => false,
            });
        g.keep = kept
            .and_then(|k| g.copies.iter().position(|c| c.path == k))
            .unwrap_or_else(|| keep_index(&g.copies, KeepRule::Oldest, None));
    }
    groups.retain(|g| g.copies.len() > 1);
}

/// One group of a delete: the copy kept and the copies deleted, as they were when hashed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanGroup {
    pub group: u32,
    pub bytes: u64,
    pub keep: Copy,
    pub delete: Vec<Copy>,
}
/// What a confirmed delete removes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub generation: u64,
    pub groups: Vec<PlanGroup>,
    /// How the kept copies were chosen, for the dialog.
    pub rule: String,
    /// Copies left alone because they are protected or hard links.
    pub left_alone: u64,
}
impl Plan {
    pub fn files(&self) -> u64 {
        self.groups.iter().map(|g| g.delete.len() as u64).sum()
    }
    pub fn bytes(&self) -> u64 {
        self.groups.iter().fold(0u64, |s, g| {
            s.saturating_add(g.bytes.saturating_mul(g.delete.len() as u64))
        })
    }
}
/// The delete of the groups `ids` (all ticked groups when `None`) of at least `min_bytes`.
pub fn plan(
    groups: &[Group],
    ids: Option<&[u32]>,
    min_bytes: u64,
    generation: u64,
    rule: String,
) -> Result<Plan, String> {
    let mut out = Plan {
        generation,
        groups: vec![],
        rule,
        left_alone: 0,
    };
    for g in groups {
        let wanted = match ids {
            Some(ids) => ids.contains(&g.id),
            None => g.included && g.bytes >= min_bytes,
        };
        if !wanted {
            continue;
        }
        let keep = g
            .copies
            .get(g.keep)
            .ok_or("A group has no copy chosen to keep. Nothing was deleted.")?
            .clone();
        let delete: Vec<Copy> = g.to_delete().cloned().collect();
        out.left_alone += (g.copies.len() - 1 - delete.len()) as u64;
        let pg = PlanGroup {
            group: g.id,
            bytes: g.bytes,
            keep,
            delete,
        };
        if pg.delete.is_empty() {
            continue;
        }
        check(&pg)?;
        out.groups.push(pg);
    }
    if out.groups.is_empty() {
        return Err(if out.left_alone > 0 {
            "Nothing to delete: the other copies are protected or hard links.".into()
        } else {
            "Nothing to delete.".into()
        });
    }
    Ok(out)
}
/// Refuses a group that would not keep exactly one copy.
pub fn check(g: &PlanGroup) -> Result<(), String> {
    let keep = key(&g.keep.path);
    let mut seen = std::collections::HashSet::new();
    for d in &g.delete {
        let k = key(&d.path);
        if k == keep {
            return Err(format!(
                "{} was both kept and deleted. Nothing in its group was deleted.",
                crate::display_path(&d.path)
            ));
        }
        if !seen.insert(k) {
            return Err(format!(
                "{} was listed twice. Nothing in its group was deleted.",
                crate::display_path(&d.path)
            ));
        }
    }
    Ok(())
}

/// Progress of a delete.
#[derive(Default)]
pub struct DeleteCounters {
    pub done: AtomicU64,
    pub freed: AtomicU64,
}
/// What a delete did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub generation: u64,
    pub deleted: Vec<String>,
    pub freed: u64,
    /// Left alone because they (or the kept copy) changed since the check.
    pub changed: u64,
    /// Left alone because they are in a protected folder now.
    pub protected: u64,
    pub failed: Vec<String>,
    pub stopped: bool,
}
impl Report {
    /// "Deleted 212 files, freed 9.4 GiB. 3 skipped (changed), 1 failed (in use)."
    pub fn summary(&self) -> String {
        let n = self.deleted.len();
        let mut s = format!(
            "Deleted {} file{}, freed {}.",
            super::count(n as u64),
            if n == 1 { "" } else { "s" },
            crate::size(self.freed)
        );
        let mut more = vec![];
        if self.changed > 0 {
            more.push(format!("{} skipped (changed)", super::count(self.changed)));
        }
        if self.protected > 0 {
            more.push(format!(
                "{} skipped (protected)",
                super::count(self.protected)
            ));
        }
        if !self.failed.is_empty() {
            let in_use = self.failed.iter().all(|e| e.contains("in use"));
            more.push(format!(
                "{} failed{}",
                super::count(self.failed.len() as u64),
                if in_use { " (in use)" } else { "" }
            ));
        }
        if !more.is_empty() {
            s.push(' ');
            s.push_str(&more.join(", "));
            s.push('.');
        }
        if self.stopped {
            s.push_str(" Stopped before the rest.");
        }
        s
    }
}
/// Whether the file at `path` is still the one that was hashed: same size and modified time.
fn unchanged(path: &str, bytes: u64, stamp_then: u64) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|m| m.is_file() && m.len() == bytes && stamp(&m) == stamp_then)
}
/// Deletes what `plan` lists, one file at a time. Before each file it checks again that the
/// kept copy is still there unchanged and that the file is the one that was hashed; anything
/// else is left alone. `cancel` stops it before the next file.
pub fn run(
    plan: &Plan,
    protected: Guard,
    cancel: &AtomicBool,
    counters: &DeleteCounters,
) -> Report {
    let mut r = Report {
        generation: plan.generation,
        ..Default::default()
    };
    'groups: for g in &plan.groups {
        if let Err(e) = check(g) {
            r.failed.push(e);
            continue;
        }
        for (i, d) in g.delete.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                r.stopped = true;
                break 'groups;
            }
            // Never the last copy: the kept one must still be there, as it was.
            if !unchanged(&g.keep.path, g.bytes, g.keep.stamp) {
                r.changed += (g.delete.len() - i) as u64;
                continue 'groups;
            }
            if protected(&d.path).is_some() {
                r.protected += 1;
                continue;
            }
            if !unchanged(&d.path, g.bytes, d.stamp) {
                r.changed += 1;
                continue;
            }
            let request = delete::Request {
                path: d.path.clone(),
                dir: false,
                bytes: g.bytes,
                files: 1,
                generation: plan.generation,
            };
            match delete::delete(&request) {
                Ok(()) => {
                    r.deleted.push(d.path.clone());
                    r.freed = r.freed.saturating_add(g.bytes);
                    counters.freed.fetch_add(g.bytes, Ordering::Relaxed);
                }
                Err(e) => r.failed.push(e),
            }
            counters.done.fetch_add(1, Ordering::Relaxed);
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::{Progress, State};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    fn tmp(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("toolboks-dupes-{name}-"))
            .tempdir()
            .unwrap()
    }
    fn write(root: &Path, rel: &str, data: &[u8]) -> PathBuf {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, data).unwrap();
        p
    }
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
    fn none(_: &str) -> Option<String> {
        None
    }
    fn search(nodes: &[Node], min: u64) -> Found {
        find(
            nodes,
            1,
            min,
            &none,
            &AtomicBool::new(false),
            &Counters::default(),
        )
    }
    fn names(g: &Group) -> Vec<&str> {
        g.copies.iter().map(|c| c.name()).collect()
    }
    /// Data that is the same at both ends and differs only in the middle.
    fn big(middle: u8) -> Vec<u8> {
        let mut v = vec![7u8; 3 * EDGE as usize];
        v[EDGE as usize + 10] = middle;
        v
    }

    #[test]
    fn size_groups_skip_unique_sizes_empty_files_and_small_files() {
        let mut nodes = vec![super::super::model::tests::node(0, None, "root", true, 0)];
        for (name, bytes) in [
            ("a", 10u64),
            ("b", 10),
            ("c", 11),
            ("e1", 0),
            ("e2", 0),
            ("d", 2048),
            ("f", 2048),
            ("g", 2048),
        ] {
            let id = nodes.len();
            nodes.push(super::super::model::tests::node(
                id,
                Some(0),
                name,
                false,
                bytes,
            ));
        }
        let (considered, groups) = candidates(&nodes, 0);
        assert_eq!(considered, 6, "0 B files are never considered");
        assert_eq!(groups, vec![(2048, vec![6, 7, 8]), (10, vec![1, 2])]);
        let (considered, groups) = candidates(&nodes, 1024);
        assert_eq!((considered, groups.len()), (3, 1));
        assert_eq!(groups[0].0, 2048);
    }

    #[test]
    fn quick_and_full_hashes_agree_on_small_files_and_full_decides_on_big_ones() {
        let t = tmp("hash");
        let small = write(t.path(), "small.bin", &[3u8; 1000]);
        let a = write(t.path(), "a.bin", &big(1));
        let b = write(t.path(), "b.bin", &big(2));
        let c = Counters::default();
        let stop = AtomicBool::new(false);
        let h = |p: &Path, len: u64, quick: bool| {
            let mut f = fs::File::open(p).unwrap();
            hash(&mut f, len, quick, &stop, &c).ok().unwrap()
        };
        assert!(quick_is_full(1000));
        assert_eq!(h(&small, 1000, true), h(&small, 1000, false));
        assert_eq!(
            h(&small, 1000, false),
            <[u8; 32]>::from(Sha256::digest([3u8; 1000]))
        );
        let len = 3 * EDGE;
        assert!(!quick_is_full(len));
        assert_eq!(h(&a, len, true), h(&b, len, true), "same ends");
        assert_ne!(h(&a, len, false), h(&b, len, false), "different middle");
        assert_eq!(
            h(&a, len, false),
            <[u8; 32]>::from(Sha256::digest(big(1))),
            "the full hash is a plain SHA-256 of the content"
        );
    }

    #[test]
    fn identical_files_in_different_folders_are_found_and_lookalikes_are_not() {
        let t = tmp("find");
        let root = t.path();
        let photo = vec![9u8; 5000];
        write(root, "Photos/beach.jpg", &photo);
        write(root, "Backup/old/beach copy.jpg", &photo);
        write(root, "Desktop/beach.jpg", &photo);
        // Same size, different content: not duplicates.
        write(root, "x/one.dat", &[1u8; 5000 + 1]);
        write(root, "y/two.dat", &[2u8; 5000 + 1]);
        // Same ends, different middle.
        write(root, "big/a.iso", &big(1));
        write(root, "big/b.iso", &big(2));
        write(root, "big2/a again.iso", &big(1));
        // Empty files are never duplicates.
        write(root, "e/1.txt", b"");
        write(root, "e/2.txt", b"");
        let (_, nodes) = scan(root);
        let found = search(&nodes, 0);
        assert!(!found.stopped);
        assert_eq!(found.groups.len(), 2, "{:?}", found.groups);
        let iso = &found.groups[0];
        assert_eq!(iso.bytes, 3 * EDGE);
        let mut iso_names = names(iso);
        iso_names.sort();
        assert_eq!(iso_names, ["a again.iso", "a.iso"]);
        let jpg = &found.groups[1];
        assert_eq!(jpg.copies.len(), 3);
        assert_eq!(jpg.wasted(), 10_000);
        assert_eq!(found.considered, 8, "0 B files are not considered");
        assert_eq!(found.candidates, 8);
        assert_eq!(found.checked, 8);
        assert_eq!((found.unreadable, found.changed, found.cloud), (0, 0, 0));
        assert!(found.bytes_read > 0);
        // Ids are unique and follow the found order.
        assert_eq!(
            found.groups.iter().map(|g| g.id).collect::<Vec<_>>(),
            [0, 1]
        );
    }

    #[test]
    fn the_minimum_size_leaves_smaller_files_out() {
        let t = tmp("min");
        write(t.path(), "a/small1", &[1u8; 100]);
        write(t.path(), "b/small2", &[1u8; 100]);
        write(t.path(), "a/big1", &[2u8; 3000]);
        write(t.path(), "b/big2", &[2u8; 3000]);
        let (_, nodes) = scan(t.path());
        assert_eq!(search(&nodes, 0).groups.len(), 2);
        let found = search(&nodes, 1000);
        assert_eq!(found.groups.len(), 1);
        assert_eq!(found.groups[0].bytes, 3000);
        assert_eq!(found.considered, 2);
        assert!(MIN_OPTIONS.contains(&DEFAULT_MIN) && DEFAULT_MIN == 1 << 20);
    }

    #[test]
    fn a_hard_link_is_not_a_duplicate() {
        let t = tmp("links");
        let a = write(t.path(), "a/file.bin", &[5u8; 4000]);
        fs::create_dir_all(t.path().join("b")).unwrap();
        if fs::hard_link(&a, t.path().join("b/file.bin")).is_err() {
            return; // The file system has no hard links.
        }
        let (_, nodes) = scan(t.path());
        let found = search(&nodes, 0);
        assert!(found.groups.is_empty(), "{:?}", found.groups);
        assert_eq!(found.links_left_out, 1);
        // A real copy next to the two names: one group of two, and the linked name is never
        // deleted (it would free nothing).
        write(t.path(), "c/file.bin", &[5u8; 4000]);
        let (_, nodes) = scan(t.path());
        let found = search(&nodes, 0);
        assert_eq!(found.groups.len(), 1);
        let g = &found.groups[0];
        assert_eq!(g.copies.len(), 2);
        assert!(g.copies[0].linked && !g.copies[0].deletable());
        assert!(!g.copies[1].linked);
    }

    fn copy(path: &str, stamp: u64) -> Copy {
        Copy {
            node: 0,
            path: path.into(),
            modified: stamp,
            stamp,
            protected: None,
            linked: false,
        }
    }
    fn group(id: u32, bytes: u64, paths: &[&str]) -> Group {
        Group {
            id,
            bytes,
            hash: [0; 32],
            copies: paths
                .iter()
                .enumerate()
                .map(|(i, p)| copy(p, 100 + i as u64))
                .collect(),
            keep: 0,
            included: true,
        }
    }

    #[test]
    fn wasted_space_is_size_times_extra_copies() {
        let groups = [
            group(0, 1000, &[r"D:\a\x", r"D:\b\x"]),
            group(1, 300, &[r"D:\a\y", r"D:\b\y", r"D:\c\y", r"D:\d\y"]),
        ];
        assert_eq!(groups[0].wasted(), 1000);
        assert_eq!(groups[1].wasted(), 900);
        assert_eq!(wasted(&groups), 1900);
        let huge = group(2, u64::MAX, &[r"D:\a", r"D:\b", r"D:\c"]);
        assert_eq!(huge.wasted(), u64::MAX, "no overflow");
    }

    #[test]
    fn every_sort_order_is_stable() {
        let base = vec![
            group(0, 100, &[r"D:\m\b.txt", r"D:\z\b.txt", r"D:\y\b.txt"]), // wasted 200
            group(1, 500, &[r"D:\c\A.txt", r"D:\d\a.txt"]),                // wasted 500
            group(2, 200, &[r"D:\a\c.txt", r"D:\b\c.txt"]),                // wasted 200
            group(
                3,
                50,
                &[r"D:\e\d.txt", r"D:\f\d.txt", r"D:\g\d.txt", r"D:\h\d.txt"],
            ), // 150
        ];
        let order = |by: SortBy| {
            let mut g = base.clone();
            sort(&mut g, by);
            g.iter().map(|g| g.id).collect::<Vec<_>>()
        };
        assert_eq!(order(SortBy::Wasted), [1, 0, 2, 3], "ties keep found order");
        assert_eq!(order(SortBy::Size), [1, 2, 0, 3]);
        assert_eq!(order(SortBy::Copies), [3, 0, 1, 2]);
        assert_eq!(order(SortBy::Name), [1, 0, 2, 3], "case does not matter");
        assert_eq!(order(SortBy::Folder), [2, 1, 3, 0]);
        // Sorting a sorted list again changes nothing.
        for by in SortBy::ALL {
            let mut g = base.clone();
            sort(&mut g, by);
            let once = g.clone();
            sort(&mut g, by);
            assert_eq!(g, once, "{by:?}");
        }
    }

    #[test]
    fn keep_rules_pick_one_copy() {
        let mut copies = vec![
            copy(r"D:\Photos\2024\long folder name\x.jpg", 300),
            copy(r"D:\Backup\x.jpg", 100),
            copy(r"D:\Desktop\x.jpg", 900),
            copy(r"D:\Photos\x.jpg", 100),
        ];
        assert_eq!(
            keep_index(&copies, KeepRule::Oldest, None),
            1,
            "a tie goes to the first path"
        );
        assert_eq!(keep_index(&copies, KeepRule::Newest, None), 2);
        assert_eq!(keep_index(&copies, KeepRule::ShortestPath, None), 1);
        assert_eq!(
            keep_index(&copies, KeepRule::InFolder, Some(r"D:\Photos")),
            3,
            "the oldest copy in the folder, also in a subfolder"
        );
        assert_eq!(
            keep_index(&copies, KeepRule::InFolder, Some(r"\\?\D:\photos\2024")),
            0
        );
        assert_eq!(
            keep_index(&copies, KeepRule::InFolder, Some(r"D:\Music")),
            1,
            "no copy there: the oldest"
        );
        assert_eq!(
            keep_index(&copies, KeepRule::InFolder, Some(r"D:\Photo")),
            1,
            "a name prefix is not a folder"
        );
        copies.truncate(1);
        assert_eq!(keep_index(&copies, KeepRule::Newest, None), 0);
        let mut groups = vec![group(0, 10, &[r"D:\a\x", r"D:\bb\x"])];
        apply_rule(&mut groups, KeepRule::Newest, None);
        assert_eq!(groups[0].keep, 1);
        for rule in KeepRule::ALL {
            assert!(!rule.label().is_empty());
        }
    }

    #[test]
    fn plans_never_delete_the_kept_copy_or_protected_ones() {
        let mut g = group(0, 10, &[r"D:\a\x", r"D:\b\x", r"C:\Windows\x"]);
        g.copies[2].protected = Some("protected".into());
        g.keep = 1;
        let p = plan(&[g.clone()], None, 0, 1, "Oldest modified".into()).unwrap();
        assert_eq!(p.files(), 1);
        assert_eq!(p.groups[0].keep.path, r"D:\b\x");
        assert_eq!(p.groups[0].delete[0].path, r"D:\a\x");
        assert_eq!((p.bytes(), p.left_alone), (10, 1));
        // Keeping the protected copy deletes both others.
        g.keep = 2;
        let p = plan(&[g.clone()], Some(&[0]), 0, 1, String::new()).unwrap();
        assert_eq!(p.files(), 2);
        // A group whose only other copy is protected has nothing to delete.
        let mut only = group(1, 10, &[r"D:\a\y", r"C:\Windows\y"]);
        only.copies[1].protected = Some("protected".into());
        assert!(plan(&[only], None, 0, 1, String::new()).is_err());
        // Unticked groups and groups under the minimum size are left out of the bulk delete.
        let mut off = group(2, 10, &[r"D:\a\z", r"D:\b\z"]);
        off.included = false;
        let small = group(3, 5, &[r"D:\a\w", r"D:\b\w"]);
        let bulk = plan(&[g.clone(), off, small], None, 8, 1, String::new()).unwrap();
        assert_eq!(bulk.groups.len(), 1);
        // A kept copy that is not there is refused.
        let mut lost = g.clone();
        lost.keep = 9;
        assert!(plan(&[lost], None, 0, 1, String::new()).is_err());
        // A request that also deletes the kept copy is refused, whatever its spelling.
        let bad = PlanGroup {
            group: 0,
            bytes: 10,
            keep: copy(r"\\?\D:\a\x", 1),
            delete: vec![copy(r"d:\A\x", 1)],
        };
        assert!(check(&bad).is_err());
        let p = Plan {
            generation: 1,
            groups: vec![bad],
            rule: String::new(),
            left_alone: 0,
        };
        let r = run(
            &p,
            &none,
            &AtomicBool::new(false),
            &DeleteCounters::default(),
        );
        assert!(r.deleted.is_empty() && r.failed.len() == 1);
    }

    fn delete_all(found: &Found, guard: Guard) -> Report {
        let p = plan(&found.groups, None, 0, found.generation, String::new()).unwrap();
        run(
            &p,
            guard,
            &AtomicBool::new(false),
            &DeleteCounters::default(),
        )
    }

    #[test]
    fn delete_keeps_one_copy_and_the_scan_and_groups_are_patched() {
        let t = tmp("delete");
        let root = t.path();
        let a = write(root, "Photos/beach.jpg", &[9u8; 5000]);
        let b = write(root, "Backup/beach.jpg", &[9u8; 5000]);
        let c = write(root, "Desktop/beach.jpg", &[9u8; 5000]);
        let d = write(root, "x/song.mp3", &[4u8; 3000]);
        let e = write(root, "y/song.mp3", &[4u8; 3000]);
        write(root, "keep.txt", b"unique");
        let (mut p, nodes) = scan(root);
        let mut found = search(&nodes, 0);
        assert_eq!(found.groups.len(), 2);
        // Keep the Photos copy; leave the song group out of the bulk delete.
        let photos = found.groups.iter_mut().find(|g| g.bytes == 5000).unwrap();
        photos.keep = photos
            .copies
            .iter()
            .position(|c| c.path.contains("Photos"))
            .unwrap();
        found
            .groups
            .iter_mut()
            .find(|g| g.bytes == 3000)
            .unwrap()
            .included = false;
        let report = delete_all(&found, &none);
        assert_eq!(report.deleted.len(), 2);
        assert_eq!(report.freed, 10_000);
        assert_eq!(report.summary(), "Deleted 2 files, freed 9.77 KiB.");
        assert!(a.exists() && !b.exists() && !c.exists());
        assert!(d.exists() && e.exists());
        // The scan without them, with every total updated.
        let before = (p.bytes, p.files);
        let (nodes, map) = delete::remove_all(&mut p, &nodes, &report.deleted).unwrap();
        assert_eq!((p.bytes, p.files), (before.0 - 10_000, before.1 - 2));
        assert_eq!(nodes[0].bytes, p.bytes);
        for (i, n) in nodes.iter().enumerate() {
            assert_eq!(n.id, i);
        }
        remap(&mut found.groups, &map);
        assert_eq!(
            found.groups.len(),
            1,
            "a group with one copy left disappears"
        );
        let songs = &found.groups[0];
        for c in &songs.copies {
            assert_eq!(nodes[c.node].path, c.path, "ids follow the patched scan");
        }
        // The patched scan still saves.
        crate::saved_scan::save(&root.join("saved"), &p, &nodes).unwrap();
    }

    #[test]
    fn protected_copies_are_never_deleted() {
        let t = tmp("protected");
        let root = t.path();
        let a = write(root, "Program Files/app/data.bin", &[1u8; 4000]);
        let b = write(root, "Users/data.bin", &[1u8; 4000]);
        let c = write(root, "Users/old/data.bin", &[1u8; 4000]);
        let (_, nodes) = scan(root);
        let shield = root.join("Program Files").to_string_lossy().into_owned();
        let guard = move |path: &str| {
            crate::delete::refusal(path, "")
                .or_else(|| inside(path, &shield).then(|| "protected".to_owned()))
        };
        let mut found = find(
            &nodes,
            1,
            0,
            &guard,
            &AtomicBool::new(false),
            &Counters::default(),
        );
        let g = &mut found.groups[0];
        let pf = g
            .copies
            .iter()
            .position(|c| c.path.contains("Program Files"))
            .unwrap();
        assert!(g.copies[pf].protected.is_some());
        // Kept: Users\data.bin. The protected copy stays even in a bulk delete.
        g.keep = g
            .copies
            .iter()
            .position(|c| c.path.ends_with(r"Users\data.bin") || c.path.ends_with("Users/data.bin"))
            .unwrap();
        let report = delete_all(&found, &guard);
        assert_eq!(report.deleted.len(), 1);
        assert!(a.exists() && b.exists() && !c.exists());
        // A folder that became protected after the check is caught by the delete itself.
        let mut g = found.groups[0].clone();
        g.copies[pf].protected = None;
        let p = plan(&[g], None, 0, 1, String::new()).unwrap();
        let r = run(
            &p,
            &guard,
            &AtomicBool::new(false),
            &DeleteCounters::default(),
        );
        assert_eq!(r.protected, 1);
        assert!(a.exists());
    }

    #[test]
    fn files_that_changed_since_the_check_are_left_alone() {
        let t = tmp("changed");
        let root = t.path();
        let a = write(root, "a/f.bin", &[1u8; 4000]);
        let b = write(root, "b/f.bin", &[1u8; 4000]);
        let c = write(root, "c/f.bin", &[1u8; 4000]);
        let (_, nodes) = scan(root);
        let mut found = search(&nodes, 0);
        let g = &mut found.groups[0];
        g.keep = 0; // a
                    // b changes size after the check.
        fs::write(&b, [1u8; 4001]).unwrap();
        let report = delete_all(&found, &none);
        assert_eq!(report.changed, 1);
        assert_eq!(report.deleted.len(), 1);
        assert!(a.exists() && b.exists() && !c.exists());
        assert!(report.summary().contains("1 skipped (changed)"));

        // When the kept copy is gone, nothing in its group is deleted.
        let t = tmp("kept-gone");
        let root = t.path();
        let a = write(root, "a/f.bin", &[1u8; 4000]);
        let b = write(root, "b/f.bin", &[1u8; 4000]);
        let (_, nodes) = scan(root);
        let mut found = search(&nodes, 0);
        found.groups[0].keep = 0;
        fs::remove_file(&a).unwrap();
        let report = delete_all(&found, &none);
        assert!(report.deleted.is_empty());
        assert_eq!(report.changed, 1);
        assert!(b.exists(), "the last copy is never deleted");
    }

    #[test]
    fn stop_ends_a_search_and_a_delete_cleanly() {
        let t = tmp("stop");
        for i in 0..6 {
            write(t.path(), &format!("d{i}/f.bin"), &[3u8; 4000]);
        }
        let (_, nodes) = scan(t.path());
        let stop = AtomicBool::new(true);
        let found = find(&nodes, 1, 0, &none, &stop, &Counters::default());
        assert!(found.stopped);
        assert!(found.groups.is_empty());
        assert_eq!(found.checked, 0);
        // A delete stopped before it starts deletes nothing.
        let found = search(&nodes, 0);
        let p = plan(&found.groups, None, 0, 1, String::new()).unwrap();
        assert_eq!(p.files(), 5);
        let r = run(&p, &none, &stop, &DeleteCounters::default());
        assert!(r.stopped && r.deleted.is_empty());
        for i in 0..6 {
            assert!(t.path().join(format!("d{i}/f.bin")).exists());
        }
        // A search stopped from another thread mid-way returns what it has.
        let stop = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(Counters::default());
        let (s, c, n) = (stop.clone(), counters.clone(), nodes.clone());
        let worker = std::thread::spawn(move || find(&n, 1, 0, &none, &s, &c));
        stop.store(true, Ordering::Relaxed);
        let found = worker.join().unwrap();
        assert!(found.checked <= found.candidates);
    }

    #[test]
    fn unreadable_files_are_counted_and_skipped() {
        let t = tmp("unreadable");
        write(t.path(), "a/f.bin", &[3u8; 4000]);
        write(t.path(), "b/f.bin", &[3u8; 4000]);
        let gone = write(t.path(), "c/f.bin", &[3u8; 4000]);
        let (_, nodes) = scan(t.path());
        fs::remove_file(gone).unwrap();
        let found = search(&nodes, 0);
        assert_eq!(found.changed, 1, "gone since the scan");
        assert_eq!(found.groups[0].copies.len(), 2);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            let busy = t.path().join("a/f.bin");
            // No sharing at all: nobody else can open it while this handle is open.
            let handle = fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(&busy)
                .unwrap();
            let found = search(&nodes, 0);
            assert_eq!(found.unreadable, 1);
            assert!(found.groups.is_empty());
            drop(handle);
        }
    }
}
