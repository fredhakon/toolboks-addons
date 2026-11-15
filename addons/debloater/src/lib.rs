//! The Debloater tool: turn off Windows ads, suggestions and tracking toggles. State lives in
//! memory only; the page always reads the real state from Windows.

// model::Group::Apps is not constructed anywhere yet; drop this once the curated list is used.

use std::collections::HashSet;
use std::sync::mpsc::Receiver;

use toolboks_addon_sdk::{NativeAddon, TutorialStep};

// The UI-free engine lives in its own crate; re-exported so `debloat::<module>` paths keep working.
pub use toolboks_debloat_engine::{
    apps, backend, catalog, model, plan, restore, services, startup, tasks, ChangeError,
};

mod page;
mod style;
mod tutorial;

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
    loading: Option<Receiver<Result<Vec<startup::Entry>, String>>>,
    error: Option<String>,
    /// The row shown in the inspector, as (source, name).
    focus: Option<(startup::Source, String)>,
    /// A Careful entry waiting for the inline "Switch off?" confirm.
    pending: Option<(startup::Source, String)>,
    /// Changes of this session, oldest first.
    steps: Vec<startup::UndoStep>,
    message: Option<String>,
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
    /// Undo ran; the steps that failed come back with the counts.
    Undo {
        remaining: Vec<tasks::UndoStep>,
        done: usize,
        failed: usize,
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
    /// Changes of this session, oldest first.
    steps: Vec<tasks::UndoStep>,
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
    /// Undo ran; the steps that failed come back with the counts.
    Undo {
        remaining: Vec<services::UndoStep>,
        done: usize,
        failed: usize,
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
    /// Changes of this session, oldest first.
    steps: Vec<services::UndoStep>,
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
    undo_runs: Vec<Vec<plan::UndoOp>>,
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
    /// Drops what was read from Windows. Results and undo steps of this session stay in memory.
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
