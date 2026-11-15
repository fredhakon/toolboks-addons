//! Versioned streaming snapshots in the host's removable add-on data area.
use crate::scanner::{Node, Progress};
use std::{
    fs::{self, File},
    io::{self, BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    sync::{mpsc, Arc},
};
const MAGIC: &[u8; 8] = b"TBDMSCAN";
const VERSION: u32 = 1;
pub const MAX_BYTES: u64 = 384 * 1024 * 1024;
const BASIS: u64 = 0xcbf29ce484222325;
fn hash(mut h: u64, bytes: &[u8]) -> u64 {
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}
fn key(volume: &str, root: &str) -> String {
    format!(
        "{:016x}-{:016x}.scan",
        hash(BASIS, volume.to_lowercase().as_bytes()),
        hash(BASIS, root.to_lowercase().as_bytes())
    )
}
fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "Saved scan is damaged or uses an unsupported format. Run a fresh scan.",
    )
}
/// Installed executables know the host's actual root, including Preview. Standalone builds use
/// shared storage. The pinned SDK does not pass a data-directory service or custom review text.
fn directory_for(exe: &Path, fallback: PathBuf) -> PathBuf {
    let installed = exe
        .parent()
        .and_then(Path::parent)
        .filter(|p| p.file_name().is_some_and(|n| n == crate::ID))
        .and_then(Path::parent)
        .filter(|p| p.file_name().is_some_and(|n| n == "addons"))
        .and_then(Path::parent);
    installed
        .map(Path::to_owned)
        .unwrap_or(fallback)
        .join("addon-data")
        .join(crate::ID)
        .join("scans")
}
pub fn directory() -> PathBuf {
    directory_for(
        &std::env::current_exe().unwrap_or_default(),
        toolboks_shared::storage::data_root(),
    )
}
#[cfg(windows)]
pub fn volume_id(root: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        core::PCWSTR,
        Win32::Storage::FileSystem::{GetVolumeNameForVolumeMountPointW, GetVolumePathNameW},
    };
    let path: Vec<u16> = root.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut mount = vec![0u16; 32768];
    let mut volume = [0u16; 128];
    unsafe {
        GetVolumePathNameW(PCWSTR(path.as_ptr()), &mut mount).ok()?;
        GetVolumeNameForVolumeMountPointW(PCWSTR(mount.as_ptr()), &mut volume).ok()?;
    }
    Some(String::from_utf16_lossy(&volume[..volume.iter().position(|&c| c == 0)?]).to_lowercase())
}
#[cfg(not(windows))]
pub fn volume_id(root: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    Some(format!("device:{}", fs::metadata(root).ok()?.dev()))
}
struct Hashed<T> {
    inner: T,
    hash: u64,
    bytes: u64,
}
impl<W: Write> Write for Hashed<W> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        if self.bytes.saturating_add(b.len() as u64) > MAX_BYTES - 8 {
            return Err(io::Error::other(
                "Saved scan exceeds the 384 MiB limit; narrow the root or add exclusions.",
            ));
        }
        let n = self.inner.write(b)?;
        self.hash = hash(self.hash, &b[..n]);
        self.bytes += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
impl<R: Read> Read for Hashed<R> {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(b)?;
        self.hash = hash(self.hash, &b[..n]);
        self.bytes += n as u64;
        if self.bytes > MAX_BYTES {
            return Err(invalid());
        }
        Ok(n)
    }
}
fn number(r: &mut impl Read) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}
fn encode(out: &mut impl Write, p: &Progress, nodes: &[Node]) -> io::Result<()> {
    out.write_all(MAGIC)?;
    out.write_all(&VERSION.to_le_bytes())?;
    let metadata = serde_json::to_vec(p).map_err(io::Error::other)?;
    if metadata.len() > 4 * 1024 * 1024 {
        return Err(invalid());
    }
    out.write_all(&(metadata.len() as u32).to_le_bytes())?;
    out.write_all(&metadata)?;
    out.write_all(&(nodes.len() as u64).to_le_bytes())?;
    for n in nodes {
        if n.name.len() > 131072 {
            return Err(invalid());
        }
        out.write_all(&(n.parent.map_or(u64::MAX, |p| p as u64)).to_le_bytes())?;
        out.write_all(&[u8::from(n.dir)])?;
        for value in [n.bytes, n.files, n.modified, n.dir_stamp] {
            out.write_all(&value.to_le_bytes())?;
        }
        out.write_all(&(n.name.len() as u32).to_le_bytes())?;
        out.write_all(n.name.as_bytes())?;
    }
    Ok(())
}
fn decode(r: &mut impl Read, length: u64) -> io::Result<(Progress, Vec<Node>)> {
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(invalid());
    }
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    if u32::from_le_bytes(b) != VERSION {
        return Err(invalid());
    }
    r.read_exact(&mut b)?;
    let len = u32::from_le_bytes(b) as usize;
    if len > 4 * 1024 * 1024 {
        return Err(invalid());
    }
    let mut metadata = vec![0; len];
    r.read_exact(&mut metadata)?;
    let mut p: Progress = serde_json::from_slice(&metadata).map_err(|_| invalid())?;
    if p.root.is_empty()
        || p.volume.is_empty()
        || p.running
        || p.cancelled
        || !p.elapsed.is_finite()
    {
        return Err(invalid());
    }
    let count = number(r)?;
    if count == 0 || count > 4_000_000 || count > length / 45 {
        return Err(invalid());
    }
    let mut nodes: Vec<Node> = Vec::with_capacity(count as usize);
    for id in 0..count as usize {
        let parent = number(r)?;
        let parent = if parent == u64::MAX {
            None
        } else {
            Some(usize::try_from(parent).map_err(|_| invalid())?)
        };
        if (id == 0) != (parent.is_none()) || parent.is_some_and(|i| i >= id || !nodes[i].dir) {
            return Err(invalid());
        }
        let mut flag = [0u8; 1];
        r.read_exact(&mut flag)?;
        if flag[0] > 1 || (id == 0 && flag[0] != 1) {
            return Err(invalid());
        }
        let bytes = number(r)?;
        let files = number(r)?;
        let modified = number(r)?;
        let dir_stamp = number(r)?;
        r.read_exact(&mut b)?;
        let len = u32::from_le_bytes(b) as usize;
        if len == 0 || len > 131072 {
            return Err(invalid());
        }
        let mut name = vec![0; len];
        r.read_exact(&mut name)?;
        let name = String::from_utf8(name).map_err(|_| invalid())?;
        if name.contains('\0')
            || (id > 0 && (name.contains(['/', '\\', ':']) || name == "." || name == ".."))
        {
            return Err(invalid());
        }
        let path = parent.map_or_else(
            || p.root.clone(),
            |i| {
                Path::new(&nodes[i].path)
                    .join(&name)
                    .to_string_lossy()
                    .into_owned()
            },
        );
        nodes.push(Node {
            id,
            parent,
            name,
            path,
            dir: flag[0] == 1,
            bytes,
            files,
            modified,
            dir_stamp,
        });
    }
    if nodes[0].bytes != p.bytes || nodes[0].files != p.files {
        return Err(invalid());
    }
    p.generation = 1;
    Ok((p, nodes))
}
fn read(path: &Path) -> io::Result<(Progress, Arc<Vec<Node>>)> {
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    if !(32..=MAX_BYTES).contains(&len) {
        return Err(invalid());
    }
    let mut input = Hashed {
        inner: BufReader::new(file),
        hash: BASIS,
        bytes: 0,
    };
    let (p, nodes) = decode(&mut input, len)?;
    let expected = number(&mut input.inner)?;
    if expected != input.hash || input.bytes + 8 != len {
        return Err(invalid());
    }
    Ok((p, Arc::new(nodes)))
}
pub(crate) fn save(dir: &Path, p: &Progress, nodes: &[Node]) -> Result<u64, String> {
    if p.volume.is_empty() {
        return Err("This volume could not be identified. The scan was not saved.".into());
    }
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let path = dir.join(key(&p.volume, &p.root));
    let pending = path.with_extension("scan.pending");
    let result = (|| -> io::Result<u64> {
        let mut out = Hashed {
            inner: BufWriter::new(File::create(&pending)?),
            hash: BASIS,
            bytes: 0,
        };
        encode(&mut out, p, nodes)?;
        out.inner.write_all(&out.hash.to_le_bytes())?;
        out.flush()?;
        out.inner.get_ref().sync_all()?;
        let bytes = out.bytes + 8;
        drop(out);
        replace(&pending, &path)?;
        Ok(bytes)
    })();
    if result.is_err() {
        let _ = fs::remove_file(pending);
    }
    result.map_err(|e| format!("Could not save the scan: {e}"))
}
#[cfg(windows)]
fn replace(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        core::PCWSTR,
        Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        },
    };
    let a: Vec<_> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let b: Vec<_> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        MoveFileExW(
            PCWSTR(a.as_ptr()),
            PCWSTR(b.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(|e| io::Error::other(e.to_string()))
}
#[cfg(not(windows))]
fn replace(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}
fn newest(dir: &Path, exclusions: &[String]) -> LoadResult {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(format!(
                "Could not load saved scans: {e}. Run a fresh scan."
            ))
        }
    };
    let mut paths: Vec<_> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "scan"))
        .collect();
    paths.sort_by_key(|e| std::cmp::Reverse(e.metadata().and_then(|m| m.modified()).ok()));
    let mut ignored = false;
    for entry in paths {
        match read(&entry.path()) {
            Ok((p, nodes))
                if p.exclusions == exclusions
                    && volume_id(Path::new(&p.root)).as_deref() == Some(p.volume.as_str()) =>
            {
                return Ok(Some((p, nodes)))
            }
            _ => ignored = true,
        }
    }
    if ignored {
        Err("No saved scan matches this volume and exclusions, or the saved file is damaged or outdated. Run a fresh scan.".into())
    } else {
        Ok(None)
    }
}
fn delete(dir: &Path, volume: Option<&str>) -> Result<(), String> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.to_string()),
    };
    let prefix = volume.map(|v| format!("{:016x}-", hash(BASIS, v.to_lowercase().as_bytes())));
    for e in entries {
        let e = e.map_err(|e| e.to_string())?;
        let name = e.file_name();
        let name = name.to_string_lossy();
        if (name.ends_with(".scan") || name.ends_with(".scan.pending"))
            && prefix.as_ref().is_none_or(|p| name.starts_with(p))
        {
            fs::remove_file(e.path()).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}
pub type LoadResult = Result<Option<(Progress, Arc<Vec<Node>>)>, String>;
pub enum Command {
    Load(Vec<String>),
    Save(Box<Progress>, Arc<Vec<Node>>),
    Delete(Option<String>),
}
pub enum Event {
    Loaded(Box<LoadResult>),
    Saved(Result<u64, String>),
    Deleted(Result<(), String>),
}
pub struct Worker {
    sender: mpsc::Sender<Command>,
    pub receiver: mpsc::Receiver<Event>,
    pub pending: usize,
    pub loading: bool,
}
impl Worker {
    pub fn new() -> Self {
        Self::at(directory())
    }
    pub(crate) fn at(dir: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel();
        let (results, receiver) = mpsc::channel();
        // One queue orders save/delete operations: turning remembering off cannot race a save.
        std::thread::spawn(move || {
            while let Ok(command) = rx.recv() {
                let event = match command {
                    Command::Load(exclusions) => Event::Loaded(Box::new(newest(&dir, &exclusions))),
                    Command::Save(p, nodes) => Event::Saved(save(&dir, &p, &nodes)),
                    Command::Delete(volume) => Event::Deleted(delete(&dir, volume.as_deref())),
                };
                let _ = results.send(event);
            }
        });
        Self {
            sender: tx,
            receiver,
            pending: 0,
            loading: false,
        }
    }
    pub fn send(&mut self, command: Command) {
        if matches!(command, Command::Load(_)) {
            self.loading = true;
        }
        if self.sender.send(command).is_ok() {
            self.pending += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::{self, State};
    use std::sync::{atomic::AtomicBool, Mutex};
    fn fixture(tag: &str) -> (PathBuf, Progress, Arc<Vec<Node>>) {
        let root = scanner::tests::temp(tag);
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::write(root.join("nested/spaces \u{e6} \u{6f22}.txt"), b"saved").unwrap();
        let state = Arc::new(Mutex::new(State::default()));
        scanner::scan(
            root.clone(),
            vec![],
            state.clone(),
            Arc::new(AtomicBool::new(false)),
        );
        let s = state.lock().unwrap();
        (root, s.progress.clone(), s.nodes.clone())
    }
    #[test]
    fn binary_round_trip_reconstructs_paths_and_atomic_replacement() {
        let (root, p, nodes) = fixture("saved-roundtrip");
        let dir = root.join("cache");
        let bytes = save(&dir, &p, &nodes).unwrap();
        let path = dir.join(key(&p.volume, &p.root));
        let (loaded, tree) = read(&path).unwrap();
        assert_eq!(
            (loaded.bytes, loaded.files, loaded.exclusions),
            (p.bytes, p.files, p.exclusions.clone())
        );
        assert_eq!(
            serde_json::to_value(&*tree).unwrap(),
            serde_json::to_value(&*nodes).unwrap()
        );
        assert_eq!(fs::metadata(&path).unwrap().len(), bytes);
        assert!(!path.with_extension("scan.pending").exists());
        // A crash before replacement leaves the last valid committed scan in place.
        fs::write(path.with_extension("scan.pending"), b"interrupted").unwrap();
        assert!(newest(&dir, &[]).unwrap().is_some());
        save(&dir, &p, &nodes).unwrap();
        assert!(read(&path).is_ok());
        assert!(!path.with_extension("scan.pending").exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn version_damage_truncation_and_impossible_counts_are_rejected() {
        let (root, p, nodes) = fixture("saved-damage");
        let dir = root.join("cache");
        save(&dir, &p, &nodes).unwrap();
        let path = dir.join(key(&p.volume, &p.root));
        let data = fs::read(&path).unwrap();
        for len in [0, 8, 12, 20, data.len() / 2, data.len() - 1] {
            fs::write(&path, &data[..len]).unwrap();
            assert!(read(&path).is_err(), "truncation {len}");
        }
        let mut wrong = data.clone();
        wrong[8..12].copy_from_slice(&0u32.to_le_bytes());
        fs::write(&path, &wrong).unwrap();
        assert!(read(&path).is_err());
        let mut wrong = data.clone();
        let i = wrong.len() - 9;
        wrong[i] ^= 1;
        fs::write(&path, &wrong).unwrap();
        assert!(read(&path).is_err());
        let mut wrong = data.clone();
        let metadata_len = u32::from_le_bytes(wrong[12..16].try_into().unwrap()) as usize;
        wrong[16 + metadata_len..24 + metadata_len].copy_from_slice(&u64::MAX.to_le_bytes());
        fs::write(&path, &wrong).unwrap();
        assert!(read(&path).is_err());
        assert!(newest(&dir, &[]).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn identity_exclusions_and_deletion_are_scoped_to_the_addon() {
        let (root, p, nodes) = fixture("saved-scope");
        let dir = root.join("cache");
        save(&dir, &p, &nodes).unwrap();
        assert!(newest(&dir, &["skip".into()]).is_err());
        let mut other = p.clone();
        other.volume = "different-volume".into();
        save(&dir, &other, &nodes).unwrap();
        assert_ne!(key(&p.volume, &p.root), key(&other.volume, &p.root));
        assert_ne!(key(&p.volume, &p.root), key(&p.volume, "other-root"));
        delete(&dir, Some(&p.volume)).unwrap();
        assert!(!dir.join(key(&p.volume, &p.root)).exists());
        assert!(dir.join(key(&other.volume, &p.root)).exists());
        assert!(newest(&dir, &[]).is_err());
        fs::write(dir.join("unrelated.txt"), b"keep").unwrap();
        delete(&dir, None).unwrap();
        assert!(dir.join("unrelated.txt").exists());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        let installed = PathBuf::from(
            "D:/isolated/Toolboks Preview/addons/toolboks.disk-manager/0.2.0/addon.exe",
        );
        assert_eq!(
            directory_for(&installed, PathBuf::from("wrong")),
            PathBuf::from("D:/isolated/Toolboks Preview/addon-data/toolboks.disk-manager/scans")
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn queued_delete_runs_after_an_inflight_save() {
        let (root, p, nodes) = fixture("saved-queue");
        let dir = root.join("cache");
        let mut worker = Worker::at(dir.clone());
        worker.send(Command::Save(Box::new(p), nodes));
        worker.send(Command::Delete(None));
        assert!(matches!(
            worker
                .receiver
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap(),
            Event::Saved(Ok(_))
        ));
        assert!(matches!(
            worker
                .receiver
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap(),
            Event::Deleted(Ok(()))
        ));
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        drop(worker);
        fs::remove_dir_all(root).unwrap();
    }
}
