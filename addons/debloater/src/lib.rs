//! The Debloater tool: turn off Windows ads, suggestions and tracking toggles. The page always
//! reads the real state from Windows. The only thing written to disk is the undo list
//! (`saved_undo`), so changes can be undone after Toolboks closes.

// model::Group::Apps is not constructed anywhere yet; drop this once the curated list is used.

use std::collections::HashSet;
use std::sync::mpsc::{Receiver, Sender};

use toolboks_addon_sdk::{AdminOutcome, AdminRequest, HostRequest, NativeAddon, TutorialStep};

pub use admin::prepare_admin;

// The UI-free engine lives in its own crate; re-exported so `debloat::<module>` paths keep working.
pub use toolboks_debloat_engine::{
    apps, backend, catalog, model, plan, restore, services, startup, tasks, ChangeError,
};

mod admin;
mod page;
mod saved_undo;
mod style;
mod tutorial;

use saved_undo::{RegStep, ServiceStep, StartupStep, TaskStep};

/// What an administrator request that is waiting for its answer was for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AdminKind {
    RegistryApply,
    RegistryUndo,
    ServiceSet,
    ServiceUndo,
    TaskSet,
    TaskUndo,
    StartupSet,
    StartupUndo,
}

/// State of the Apps page. Package calls are slow, so they run on worker threads and report
/// back over these channels.
#[derive(Default)]
struct AppsState {
    rows: Vec<apps::AppRow>,
    /// A load was started and not yet invalidated.
    started: bool,
    loading: Option<Receiver<Result<Vec<apps::AppRow>, String>>>,
    removing: Option<Receiver<apps::AppRun>>,
    error: Option<String>,
    /// Ticked curated app ids.
    selected: HashSet<&'static str>,
    focus: Option<&'static str>,
    confirming: bool,
    last_run: Option<apps::AppRun>,
    message: Option<String>,
}

/// State of the Startup page. Reading versions of every startup file is slow, so the list loads
/// on a worker thread; a single switch change runs on the UI thread.
#[derive(Default)]
struct StartupState {
    entries: Vec<startup::Entry>,
    /// A load was started and not yet invalidated.
    started: bool,
    loading: Option<Receiver<StartupLoad>>,
    error: Option<String>,
    /// What Windows measured for slow startup items (the boot performance log), read once per
    /// visit and again on Refresh. `None` until read; see `boot_perf` for the source.
    cost: Option<StartupCost>,
    /// The row shown in the inspector, as (source, name).
    focus: Option<(startup::Source, String)>,
    /// A Careful entry waiting for the inline "Switch off?" confirm.
    pending: Option<(startup::Source, String)>,
    /// Changes that can be undone, oldest first (saved, so they survive a restart).
    steps: Vec<StartupStep>,
    message: Option<String>,
}

/// One finished load of the Startup page: the entries, and the boot log when it was read too.
struct StartupLoad {
    entries: Result<Vec<startup::Entry>, String>,
    cost: Option<StartupCost>,
}

/// The measured startup cost: whether the log could be read, and what Windows named as slow.
struct StartupCost {
    access: boot_perf::Access,
    /// The log read stopped early or hit its limit, so a missing name proves nothing.
    incomplete: bool,
    slow: Vec<boot_perf::SlowSummary>,
}

/// The "Create a restore point first" switch, shared by Privacy and ads and Tasks. Memory only.
#[derive(Default)]
struct RestoreState {
    /// The switch (off by default each session).
    on: bool,
    /// Tasks page: Windows confirmed a new or recent restore point this session.
    tasks_done: bool,
    /// Services page: same, once per session.
    services_done: bool,
    /// Privacy and ads: creating before an apply, with the ids to apply afterwards.
    creating: Option<Receiver<(restore::RestoreStatus, Vec<&'static str>)>>,
    /// Last result line.
    status: Option<String>,
}

/// Result of a short worker run on the Tasks page.
enum TaskJob {
    /// One task was switched: (title, outcome, restore point result if one was made first).
    Set(
        String,
        Result<tasks::UndoStep, ChangeError<tasks::UndoStep>>,
        Option<restore::RestoreStatus>,
    ),
    /// Undo ran; the steps that failed come back with the counts and the refusal reasons.
    Undo {
        remaining: Vec<TaskStep>,
        done: usize,
        failed: usize,
        refused: Vec<String>,
    },
}

/// State of the Tasks page. Task Scheduler (COM) calls run on worker threads only.
#[derive(Default)]
struct TasksState {
    statuses: Vec<tasks::TaskStatus>,
    /// A load was started and not yet invalidated.
    started: bool,
    loading: Option<Receiver<Vec<tasks::TaskStatus>>>,
    /// A change or undo is running.
    working: Option<Receiver<TaskJob>>,
    /// The task shown in the inspector.
    focus: Option<&'static str>,
    /// A Careful task waiting for the inline "Switch off?" confirm.
    pending: Option<&'static str>,
    /// Changes that can be undone, oldest first (saved, so they survive a restart).
    steps: Vec<TaskStep>,
    message: Option<String>,
}

/// Result of a short worker run on the Services page.
enum ServiceJob {
    /// One service was switched off: (title, outcome, restore point result if one was made first).
    Set(
        String,
        Result<services::UndoStep, ChangeError<services::UndoStep>>,
        Option<restore::RestoreStatus>,
    ),
    /// Undo ran; the steps that failed come back with the counts and the refusal reasons.
    Undo {
        remaining: Vec<ServiceStep>,
        done: usize,
        failed: usize,
        refused: Vec<String>,
    },
}

/// State of the Services page. Service Control Manager calls run on worker threads only.
#[derive(Default)]
struct ServicesState {
    statuses: Vec<services::ServiceStatus>,
    /// A load was started and not yet invalidated.
    started: bool,
    loading: Option<Receiver<Vec<services::ServiceStatus>>>,
    /// A change or undo is running.
    working: Option<Receiver<ServiceJob>>,
    /// The service shown in the inspector.
    focus: Option<&'static str>,
    /// A service waiting for the inline "Switch off?" confirm.
    pending: Option<&'static str>,
    /// Changes that can be undone, oldest first (saved, so they survive a restart).
    steps: Vec<ServiceStep>,
    message: Option<String>,
}

pub struct Debloater {
    env: model::Env,
    can_change_user_profile: bool,
    statuses: Vec<model::Status>,
    /// Ticked item ids.
    selected: HashSet<&'static str>,
    /// The item shown in "About this item".
    focus: Option<&'static str>,
    /// Registry undo steps grouped by apply run, oldest first.
    undo_runs: Vec<Vec<RegStep>>,
    message: Option<String>,
    /// Plain-words lines for items of the last run that did not fully work.
    run_notes: Vec<String>,
    /// The inline "Apply N changes?" confirm block is open.
    confirming: bool,
    /// The state has been read from Windows at least once.
    loaded: bool,
    apps: AppsState,
    startup: StartupState,
    tasks: TasksState,
    services: ServicesState,
    restore: RestoreState,
    /// Which page is shown: 0 Privacy and ads, 1 Apps, 2 Startup, 3 Tasks, 4 Services.
    page: usize,
    /// The administrator request waiting to be sent to the host.
    request: Option<HostRequest>,
    next_request: u64,
    /// The request that has been sent and not answered yet, and what it was for.
    admin_pending: Option<(u64, AdminKind)>,
    /// Where the answer to a service or task request goes, so the page's existing job handling
    /// shows it exactly as it showed the answer of a worker thread.
    service_tx: Option<Sender<ServiceJob>>,
    task_tx: Option<Sender<TaskJob>>,
    /// The title of the item a pending set request is about.
    pending_title: String,
    /// An undo that is waiting for the helper: the steps it was asked to undo (newest first) and the
    /// steps that were left alone (oldest first).
    service_batch: Vec<ServiceStep>,
    service_rest: Vec<ServiceStep>,
    task_batch: Vec<TaskStep>,
    task_rest: Vec<TaskStep>,
    startup_batch: Vec<StartupStep>,
    startup_rest: Vec<StartupStep>,
    /// The title of the startup item a pending set request is about, and what was asked.
    pending_startup: Option<(String, bool)>,
    /// The saved undo list on disk.
    saved: SavedState,
}

/// The saved undo list: where it lives and what happened to it.
struct SavedState {
    store: saved_undo::UndoStore,
    /// The file was read (once, on the first draw).
    loaded: bool,
    /// Writing is paused: the file on disk is from a newer Debloater or could not be read, and is
    /// never overwritten. "Delete saved undo" ends the pause.
    paused: bool,
    /// Plain words about what was found on disk (damaged, newer, unreadable entries).
    note: Option<String>,
    /// The last save failed; undo still works until Toolboks closes.
    error: Option<String>,
    /// Catalog ids of apps Debloater removed (they come back from the Microsoft Store).
    removed_apps: Vec<&'static str>,
    /// Saved entries this version could not read; written back unchanged, never used.
    unreadable: Vec<serde_json::Value>,
    /// The inline "Delete saved undo?" confirm is open.
    confirming: bool,
}

impl SavedState {
    fn new(dir: std::path::PathBuf) -> Self {
        Self {
            store: saved_undo::UndoStore::new(dir),
            loaded: false,
            paused: false,
            note: None,
            error: None,
            removed_apps: Vec::new(),
            unreadable: Vec::new(),
            confirming: false,
        }
    }
}

impl Debloater {
    pub fn new() -> Self {
        Self {
            env: backend::detect_env(),
            can_change_user_profile: toolboks_shared::elevation::can_change_user_profile(),
            statuses: Vec::new(),
            selected: HashSet::new(),
            focus: None,
            undo_runs: Vec::new(),
            message: None,
            run_notes: Vec::new(),
            confirming: false,
            loaded: false,
            apps: AppsState::default(),
            startup: StartupState::default(),
            tasks: TasksState::default(),
            services: ServicesState::default(),
            restore: RestoreState::default(),
            page: 0,
            request: None,
            next_request: 0,
            admin_pending: None,
            service_tx: None,
            task_tx: None,
            pending_title: String::new(),
            service_batch: Vec::new(),
            service_rest: Vec::new(),
            task_batch: Vec::new(),
            task_rest: Vec::new(),
            startup_batch: Vec::new(),
            startup_rest: Vec::new(),
            pending_startup: None,
            saved: SavedState::new(saved_undo::UndoStore::default_dir()),
        }
    }
}

impl Default for Debloater {
    fn default() -> Self {
        Self::new()
    }
}

/// Page names, in order. They match `host-services.json`.
pub const PAGES: [&str; 5] = ["Privacy and ads", "Apps", "Startup", "Tasks", "Services"];
pub const ID: &str = "toolboks.debloater";

/// Applies the Toolboks look (fonts, text sizes, spacing, colours) the first time the add-on
/// draws. The add-on runs in its own process, so the host's own set-up does not reach it.
fn style_once(ctx: &egui::Context) {
    let id = egui::Id::NULL.with("toolboks-style-applied");
    if !ctx.data(|data| data.get_temp::<bool>(id)).unwrap_or(false) {
        toolboks_shared::ui::apply_style(ctx);
        ctx.data_mut(|data| data.insert_temp(id, true));
    }
}

impl NativeAddon for Debloater {
    fn ui(
        &mut self,
        ui: &mut egui::Ui,
        _preferences: &mut std::collections::BTreeMap<String, serde_json::Value>,
    ) -> bool {
        style_once(ui.ctx());
        self.page_ui(ui, self.page);
        false
    }
    fn navigate(&mut self, page: &str) {
        self.page = page.parse::<usize>().unwrap_or(0).min(PAGES.len() - 1);
    }
    fn page_index(&self) -> usize {
        self.page
    }
    fn prepare_admin(
        &mut self,
        request: &AdminRequest,
    ) -> Result<Box<dyn toolboks_addon_sdk::admin::AdminChange>, String> {
        admin::prepare_admin(request)
    }
    fn admin_result(&mut self, id: u64, outcome: AdminOutcome) {
        self.admin_finished(id, outcome);
    }
    fn take_host_request(&mut self) -> Option<HostRequest> {
        self.request.take()
    }
    fn has_background_work(&self) -> bool {
        self.admin_pending.is_some()
    }
    /// Drops what was read from Windows. Results and undo steps stay (the undo list is also saved).
    fn suspend(&mut self) {
        self.statuses = Vec::new();
        self.loaded = false;
        self.confirming = false;
        self.selected = HashSet::new();
        self.apps.rows = Vec::new();
        self.apps.loading = None;
        self.apps.started = false;
        self.apps.confirming = false;
        self.apps.selected = HashSet::new();
        self.startup.entries = Vec::new();
        self.startup.cost = None;
        self.startup.loading = None;
        self.startup.started = false;
        self.startup.pending = None;
        self.tasks.statuses = Vec::new();
        self.tasks.loading = None;
        self.tasks.started = false;
        self.tasks.pending = None;
        self.services.statuses = Vec::new();
        self.services.loading = None;
        self.services.started = false;
        self.services.pending = None;
    }
    fn tutorial_steps(&self) -> Vec<TutorialStep> {
        tutorial::steps()
    }
    fn tutorial_location(&self) -> String {
        self.page.to_string()
    }
    fn tutorial_navigate(&mut self, page: &str) {
        self.navigate(page);
    }
}
