//! Mirrors broker grants into the bash file queue's slot dir (`%TEMP%\jt-slots`), so scripts that do not know
//! Testdeck still see busy slots, and counts the slot dirs those scripts hold ("foreign busy").
use crate::scheduler::Ticket;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Foreign slot dirs older than this are ignored (a crashed holder; the bash client reaps dead ones).
pub const FOREIGN_MAX_AGE: Duration = Duration::from_secs(2 * 3600);
/// A foreign `excl` dir means an old gate.sh holds the whole PC, so it counts as this many busy slots.
pub const FOREIGN_EXCL_WEIGHT: u32 = 1000;

#[derive(Debug)]
pub struct Mirror {
    dir: PathBuf,
    owned: HashMap<Ticket, PathBuf>,
}

impl Mirror {
    pub fn new(dir: PathBuf) -> Self {
        let _ = fs::create_dir_all(&dir);
        Self {
            dir,
            owned: HashMap::new(),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Creates `<dir>/excl` for a gate, else the first free `<dir>/<n>`, n in 1..=slots (mkdir is the lock, as
    /// in slots.sh), with `pid` (MSYS pid) and `winpid`. Returns None when every name is taken.
    pub fn grant(
        &mut self,
        t: Ticket,
        gate: bool,
        slots: u32,
        shell_pid: Option<u32>,
        win_pid: Option<u32>,
    ) -> Option<PathBuf> {
        let _ = fs::create_dir_all(&self.dir);
        let names: Vec<String> = if gate {
            vec!["excl".into()]
        } else {
            (1..=slots.max(1)).map(|n| n.to_string()).collect()
        };
        let line = |p: Option<u32>| p.map(|p| format!("{p}\n")).unwrap_or_default();
        for name in names {
            let d = self.dir.join(name);
            if fs::create_dir(&d).is_ok() {
                let _ = fs::write(d.join("pid"), line(shell_pid));
                let _ = fs::write(d.join("winpid"), line(win_pid));
                self.owned.insert(t, d.clone());
                return Some(d);
            }
        }
        None
    }

    pub fn release(&mut self, t: Ticket) {
        if let Some(d) = self.owned.remove(&t) {
            let _ = fs::remove_dir_all(d);
        }
    }

    /// Slot dirs the broker did not create and that are younger than `FOREIGN_MAX_AGE`.
    pub fn foreign_busy(&self) -> u32 {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return 0;
        };
        let mut busy = 0;
        for e in entries.flatten() {
            let path = e.path();
            if self.owned.values().any(|d| *d == path) || !path.is_dir() {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            let weight = match name.as_str() {
                "excl" => FOREIGN_EXCL_WEIGHT,
                n if n.parse::<u32>().is_ok() => 1,
                _ => continue,
            };
            // A holder whose Windows pid has exited holds nothing; slots.sh removes its dir on the next reap.
            let winpid = fs::read_to_string(path.join("winpid"))
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok());
            if winpid.is_some_and(|p| !crate::state::ProcCtl::is_alive(&crate::winproc::WinProc, p))
            {
                continue;
            }
            let age = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|m| SystemTime::now().duration_since(m).ok());
            if age.is_none_or(|a| a < FOREIGN_MAX_AGE) {
                busy += weight;
            }
        }
        busy
    }
}
