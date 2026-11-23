//! Testdeck: a machine-wide broker that queues and shows the heavy test and build jobs on this PC, as a native
//! background add-on. The broker (HTTP on 127.0.0.1, the 1 s tick, the guard) runs on its own tokio thread for as
//! long as the add-on process lives; the pages only read and change the shared state.
pub mod broker;
pub mod cache;
pub mod config;
pub mod folders;
pub mod guard;
pub mod history;
pub mod idle;
pub mod instance;
pub mod integration;
pub mod known_red;
pub mod lanes;
pub mod mirror;
pub mod notify;
pub mod orphans;
pub mod progress;
pub mod scheduler;
pub mod state;
pub mod sysmon;
pub mod ui;
pub mod winproc;

use config::Config;
use history::History;
use state::{AppState, Deps, Shared};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use toolboks_addon_sdk::{NativeAddon, StatusItem, TrayItem};

pub const ID: &str = "toolboks.testdeck";
pub const PAGES: [&str; 8] = [
    "Overview", "History", "Agents", "DB lanes", "Orphans", "Disk", "AI setup", "Settings",
];

/// `<Toolboks data folder>/addon-data/toolboks.testdeck`.
pub fn data_dir() -> PathBuf {
    toolboks_shared::storage::data_root()
        .join("addon-data")
        .join(ID)
}

/// The files the standalone Testdeck kept in `%APPDATA%\testdeck`.
const LEGACY_FILES: [&str; 7] = [
    "config.toml",
    "history.db",
    "history.db-wal",
    "history.db-shm",
    "install-log.json",
    "cache.db",
    "known-red.txt",
];

/// One-time move from the standalone app: copies its data in when this folder has no `config.toml` yet. The old
/// folder stays as a backup. Only for the real data folder, never for a `TOOLBOKS_DATA_DIR` scratch run.
fn copy_legacy_data(dir: &Path) {
    if std::env::var_os("TOOLBOKS_DATA_DIR").is_some() || dir.join("config.toml").exists() {
        return;
    }
    let Some(old) = std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("testdeck")) else {
        return;
    };
    if !old.join("config.toml").is_file() {
        return;
    }
    if let Err(e) = std::fs::create_dir_all(dir) {
        return eprintln!("testdeck: cannot create {}: {e}", dir.display());
    }
    // config.toml last: its presence marks the copy as done.
    for name in LEGACY_FILES.iter().rev() {
        let from = old.join(name);
        if from.is_file() {
            if let Err(e) = std::fs::copy(&from, dir.join(name)) {
                eprintln!("testdeck: cannot copy {}: {e}", from.display());
            }
        }
    }
}

/// The running broker: state, history and why it is not running, if it is not.
struct Core {
    shared: Shared,
    history: Arc<History>,
    /// Set by the broker thread when the port could not be bound.
    broker_error: Arc<Mutex<Option<String>>>,
}

pub struct Testdeck {
    core: Option<Core>,
    /// Why the broker did not start (second instance, broken config, history); shown on every page.
    error: Option<String>,
    /// Page state and caches; dropped on suspend, rebuilt on resume.
    app: Option<ui::TestdeckApp>,
    page: usize,
}

impl Testdeck {
    /// Does what the standalone `main` did: one instance per data folder, config, history, the 30 s stats thread
    /// and the broker on its own tokio runtime. Never waits for the port, so the first frame comes at once; a
    /// failure becomes an in-page error and the add-on keeps running (exiting would only restart it).
    pub fn new() -> Self {
        let (core, error) = match start() {
            Ok(core) => (Some(core), None),
            Err(e) => (None, Some(format!("{e:#}"))),
        };
        if let Some(e) = &error {
            eprintln!("testdeck: {e}");
        }
        Self {
            core,
            error,
            app: None,
            page: 0,
        }
    }

    fn app(&mut self) -> Option<&mut ui::TestdeckApp> {
        let core = self.core.as_ref()?;
        Some(
            self.app.get_or_insert_with(|| {
                ui::TestdeckApp::new(core.shared.clone(), core.history.clone())
            }),
        )
    }
}

impl Default for Testdeck {
    fn default() -> Self {
        Self::new()
    }
}

fn start() -> anyhow::Result<Core> {
    let dir = data_dir();
    copy_legacy_data(&dir);
    if !instance::acquire(&dir) {
        anyhow::bail!(
            "Testdeck is already running for this data folder (the standalone Testdeck app, or Toolboks in another \
             session). Quit it and restart this add-on."
        );
    }
    let path = Config::default_path();
    let config = Config::load(&path)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e:#}", path.display()))?;
    let history = Arc::new(History::open(&History::path_beside(&path))?);
    let deps = Deps {
        proc_ctl: Arc::new(winproc::WinProc),
        sink: history.clone(),
        idle: Arc::new(idle::WinIdle),
        load: Box::new(sysmon::SysMon::default()),
    };
    let shared = AppState::new(config, Some(path), deps);
    // Every 30 s: the per-command averages the queue ETA reads, so it does not need a page open.
    let (s, h) = (shared.clone(), history.clone());
    std::thread::spawn(move || loop {
        let stats = h.job_averages();
        s.lock().job_stats = stats;
        std::thread::sleep(std::time::Duration::from_secs(30));
    });
    // Every 30 minutes: the automatic jest folder cleanup, when the config turns it on (off by default).
    let (s, h) = (shared.clone(), history.clone());
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(30 * 60));
        if let Some(o) = folders::auto_clean(&s, h.job_paths()) {
            if !o.removed.is_empty() || !o.failed.is_empty() {
                eprintln!(
                    "testdeck: disk cleanup freed {} MB in {} folders, {} kept or failed",
                    o.freed >> 20,
                    o.removed.len(),
                    o.failed.len()
                );
            }
        }
    });
    let broker_error = Arc::new(Mutex::new(None));
    let (s, err) = (shared.clone(), broker_error.clone());
    std::thread::spawn(move || {
        let fail = |e: String| {
            eprintln!("testdeck: {e}");
            *err.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
        };
        // Two workers serve a loopback broker; one per logical core (32 on the dev PC) only costs idle memory.
        let rt = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => return fail(format!("cannot start the broker runtime: {e}")),
        };
        rt.block_on(async move {
            match broker::start(s).await {
                Ok(port) => {
                    eprintln!("testdeck: listening on 127.0.0.1:{port}");
                    std::future::pending::<()>().await
                }
                Err(e) => fail(format!(
                    "the broker cannot listen: {e:#}. Agents use the file queue until it can."
                )),
            }
        });
    });
    Ok(Core {
        shared,
        history,
        broker_error,
    })
}

impl NativeAddon for Testdeck {
    fn ui(
        &mut self,
        ui: &mut egui::Ui,
        _preferences: &mut std::collections::BTreeMap<String, serde_json::Value>,
    ) -> bool {
        ui::style_once(ui.ctx());
        if let Some(e) = &self.error {
            ui::error_card(ui, e);
            return false;
        }
        let broker_error = self
            .core
            .as_ref()
            .and_then(|c| c.broker_error.lock().ok().and_then(|e| e.clone()));
        let page = self.page;
        if let Some(app) = self.app() {
            app.show(ui, page, broker_error.as_deref());
            if let Some(p) = app.take_page() {
                self.page = p;
            }
        }
        false
    }

    fn navigate(&mut self, page: &str) {
        self.page = page
            .split('/')
            .next()
            .and_then(|p| p.parse::<usize>().ok())
            .unwrap_or(0)
            .min(PAGES.len() - 1);
    }

    fn page_index(&self) -> usize {
        self.page
    }

    /// Drops the page caches (the lanes poller, the history query, the jest log tracker); the broker keeps running.
    fn suspend(&mut self) {
        self.app = None;
    }

    /// The broker runs on its own threads, but status and tray items must follow it while no page is shown.
    fn has_background_work(&self) -> bool {
        true
    }

    fn status_items(&self) -> Vec<StatusItem> {
        let Some(core) = &self.core else {
            return vec![StatusItem {
                text: "Testdeck stopped".into(),
                tooltip: self.error.clone(),
            }];
        };
        let asking = core.shared.gate_asking().is_some();
        let c = core.shared.lock();
        let mut items = vec![StatusItem {
            text: format!(
                "Testdeck {} running \u{B7} {} queued",
                c.sched.running.len(),
                c.sched.queue.len()
            ),
            tooltip: None,
        }];
        if c.sched.paused {
            items.push(StatusItem {
                text: "Queue paused".into(),
                tooltip: None,
            });
        }
        if c.user_mode() {
            items.push(StatusItem {
                text: "User mode".into(),
                tooltip: Some("One slot, low priority".into()),
            });
        }
        if asking {
            items.push(StatusItem {
                text: "Gate waiting".into(),
                tooltip: Some("Allow or hold it from the tray".into()),
            });
        }
        items
    }

    fn tray_items(&self) -> Vec<TrayItem> {
        let Some(core) = &self.core else {
            return Vec::new();
        };
        let item = |id: &str, label: &str| TrayItem {
            id: id.into(),
            label: label.into(),
            enabled: true,
        };
        let (paused, user) = {
            let c = core.shared.lock();
            (c.sched.paused, c.user_mode_manual)
        };
        let mut items = vec![
            item(
                "pause",
                if paused {
                    "Resume queue"
                } else {
                    "Pause queue"
                },
            ),
            item(
                "user",
                if user {
                    "User mode: on"
                } else {
                    "User mode: off"
                },
            ),
        ];
        if core.shared.gate_asking().is_some() {
            items.push(item("allow", "Allow waiting gate"));
            items.push(item("later", "Gate later (30 min)"));
        }
        items
    }

    fn tray_action(&mut self, id: &str) {
        let Some(core) = &self.core else { return };
        let s = &core.shared;
        match id {
            "pause" => {
                let paused = !s.lock().sched.paused;
                s.set_paused(paused);
            }
            "user" => {
                let on = !s.lock().user_mode_manual;
                s.set_user_mode(on);
            }
            "allow" | "later" => {
                if let Some((t, _)) = s.gate_asking() {
                    if id == "allow" {
                        s.allow_gate(t);
                    } else {
                        s.snooze_gate(t);
                    }
                }
            }
            _ => {}
        }
    }
}
