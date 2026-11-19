//! Settings, saved to `config.toml` in the add-on's data folder and applied live.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The test load slider, lightest first. Each preset sets the queue and load limits from this PC's thread count.
pub const LOAD_LEVELS: [&str; 4] = ["Light", "Medium", "High", "Heavy"];

/// Logical CPU threads of this PC.
pub fn threads() -> u32 {
    std::thread::available_parallelism().map_or(1, |n| n.get() as u32)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub port: u16,
    pub slots: u32,
    /// Type checks (tsc, npm run typecheck) are single-core: they run in their own lane beside the jest slots.
    pub tsc_slots: u32,
    /// Workers the running jobs share: a job alone gets all of them, the gate always does. 0 = every logical core
    /// but two.
    pub worker_budget: u32,
    /// Free RAM each worker is expected to need; a grant gets no more workers than free RAM allows.
    pub ram_per_worker_gb: f32,
    pub workers_user_mode: u32,
    /// Percent; load-based grants need the 10 s CPU average below this.
    pub cpu_threshold: f32,
    pub ram_free_min_gb: f32,
    pub auto_user_mode: bool,
    pub idle_minutes: u32,
    pub gate_exclusive: bool,
    pub gate_first: bool,
    /// Split the cores between the running jobs (not the gate) and lock each job's process tree to its share.
    pub cpu_cap: bool,
    pub heartbeat_timeout_secs: u64,
    /// Empty means `%TEMP%\jt-slots`, the dir the bash file queue uses.
    pub mirror_dir: String,
    // Phase 2 (PROMPT.md "Phase 2"). Token savers.
    /// Result cache and dedupe of identical jest runs.
    pub cache_enabled: bool,
    pub cache_ttl_hours: u32,
    /// Suites rerun once alone when they are the only failures.
    pub flaky_suites: Vec<String>,
    // Safety and convenience.
    /// A job with no new PASS/FAIL line for this long is marked stalled.
    pub stall_minutes: u32,
    /// Below this free RAM the newest non-gate job is killed and requeued at the head.
    pub ram_kill_free_gb: f32,
    /// Toast when a gate, or any run at least this long, ends.
    pub toast_min_minutes: u32,
    /// Gate requests wait for the night window when this is on.
    pub night_window_enabled: bool,
    /// "HH:MM", local time; the window may cross midnight.
    pub night_start: String,
    pub night_end: String,
    /// In user mode, a toast with Allow / Later asks before a gate starts.
    pub gate_ask_in_user_mode: bool,
    /// The test load preset (index into `LOAD_LEVELS`) the limits match; None = set by hand. Re-applied on start, so a
    /// preset follows the thread count of the PC it runs on.
    pub load_level: Option<u8>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 47123,
            slots: 2,
            tsc_slots: 3,
            worker_budget: 0,
            ram_per_worker_gb: 0.75,
            workers_user_mode: 1,
            cpu_threshold: 85.0,
            ram_free_min_gb: 4.0,
            auto_user_mode: true,
            idle_minutes: 5,
            gate_exclusive: true,
            gate_first: true,
            cpu_cap: true,
            heartbeat_timeout_secs: 30,
            mirror_dir: String::new(),
            cache_enabled: true,
            cache_ttl_hours: 24,
            flaky_suites: vec!["rate-limit/api-v1-edge-and-parser-select".into()],
            stall_minutes: 10,
            ram_kill_free_gb: 2.0,
            toast_min_minutes: 5,
            night_window_enabled: false,
            night_start: "22:00".into(),
            night_end: "07:00".into(),
            gate_ask_in_user_mode: true,
            load_level: None,
        }
    }
}

impl Config {
    pub fn default_path() -> PathBuf {
        crate::data_dir().join("config.toml")
    }

    /// A missing file gives the defaults; a broken one is an error.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let mut cfg: Self = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(e.into()),
        };
        if let Some(level) = cfg.load_level {
            cfg.set_load(level, threads());
        }
        Ok(cfg)
    }

    /// Sets the queue and load limits of a test load preset, scaled to `threads` logical CPU threads.
    pub fn set_load(&mut self, level: u8, threads: u32) {
        let t = threads.max(1);
        let (slots, tsc, budget, cpu, ram) = match level {
            0 => (1, 1, (t / 4).max(1), 60.0, 6.0),
            1 => ((t / 16).max(2), 2, (t / 2).max(1), 80.0, 4.0),
            2 => (
                (t / 8).max(2),
                (t / 10).max(2),
                t.saturating_sub(2).max(1),
                95.0,
                3.0,
            ),
            _ => ((t / 4).max(3), (t / 8).max(3), t, 100.0, 2.0),
        };
        self.slots = slots;
        self.tsc_slots = tsc;
        self.worker_budget = budget;
        self.cpu_threshold = cpu;
        self.ram_free_min_gb = ram;
        self.load_level = Some(level.min(3));
    }

    /// The preset these limits match, if any.
    pub fn matching_load(&self, threads: u32) -> Option<u8> {
        (0..LOAD_LEVELS.len() as u8).find(|&l| {
            let mut c = self.clone();
            c.set_load(l, threads);
            (
                c.slots,
                c.tsc_slots,
                c.worker_budget,
                c.cpu_threshold,
                c.ram_free_min_gb,
            ) == (
                self.slots,
                self.tsc_slots,
                self.worker_budget,
                self.cpu_threshold,
                self.ram_free_min_gb,
            )
        })
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn mirror_path(&self) -> PathBuf {
        if self.mirror_dir.is_empty() {
            std::env::temp_dir().join("jt-slots")
        } else {
            PathBuf::from(&self.mirror_dir)
        }
    }
}
