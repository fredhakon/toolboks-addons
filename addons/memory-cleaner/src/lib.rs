mod actions;
mod activity;
mod admin;
mod advanced;
mod audio;
mod auto_triggers;
#[cfg(test)]
mod bench;
mod clean_log;
mod clean_ui;
mod commit;
mod engine;
mod freed;
mod groups;
mod growth;
mod health;
mod health_feed;
mod home;
mod memlist;
mod pressure;
mod regions;
mod report;
mod sampler;
mod scheduler;
mod series;
mod stats;
mod style;
mod trim;
mod tutorial;
mod workbench;

use std::cell::OnceCell;
use std::collections::{HashSet, VecDeque};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use self::actions::{Cooldowns, UndoLog};
use self::activity::Activity;
pub use self::admin::prepare_admin;
use self::admin::PurgeJob;
use self::auto_triggers::{AutoTriggers, Settings as AutoSettings};
use self::clean_ui::Confirm;
use self::engine::{RunJob, RunSummary, Settling};
use self::growth::GrowthTracker;
use self::memlist::Lists;
use self::pressure::{Assessment, PressureModel, SignalSource, Verdict, WindowsSignals};
use self::sampler::WindowsSampler;
use self::scheduler::Scheduler;
use self::stats::{Sampler, Snapshot, SortKey};
use self::workbench::Selection;
use toolboks_addon_sdk::{Finding, HostRequest, NativeAddon, StatusItem, TrayItem, TutorialStep};

const REFRESH: Duration = Duration::from_secs(2);

enum RefreshAction {
    Optimize(report::Trigger),
    Trim {
        selection: HashSet<String>,
        grouped: bool,
    },
}

struct RefreshWorker {
    request: Sender<Option<RefreshAction>>,
    result: Receiver<(Option<Snapshot>, Option<RefreshAction>)>,
}

/// Live RAM usage, a per-process table, manual per-process trimming, a pressure verdict, the
/// safe optimize run and an automatic mode that only acts under real memory pressure.
/// Only the "Clean selected regions" ticks, the automatic trigger settings and the confirm and
/// log switches are saved (as preferences): exclusions and filters reset when the app closes.
pub struct Memory {
    /// `None` if the platform refused to provide system info.
    sampler: Option<Snapshot>,
    refresh_worker: Option<RefreshWorker>,
    refresh_in_flight: usize,
    refresh_action_pending: bool,
    /// Whether the page is shown. Starts `false`: see `Memory::build`.
    active: bool,
    /// Free/modified/standby sizes; `None` if the query is unavailable.
    lists: Option<Lists>,
    /// The administrator request for the cache purge, waiting to be sent to the host.
    request: Option<HostRequest>,
    next_request: u64,
    /// A finished purge asked for fresh numbers on the next frame.
    refresh_after_purge: bool,
    /// Kept from the last frame so background polling can wake the window.
    ctx: Option<egui::Context>,
    /// Which page is shown: 0 Home, 1 Advanced.
    page: usize,
    last_refresh: Instant,
    sort_key: SortKey,
    descending: bool,
    filter: String,
    /// One table row per process name instead of per process.
    grouped: bool,
    /// Lowercase process names that are never trimmed.
    exclusions: HashSet<String>,
    /// Selected table rows (by stable key), and the inspector tab shown.
    selection: Selection,
    tab: advanced::Tab,
    /// Whether the table was grouped last frame, to clear the selection when it changes.
    was_grouped: bool,
    /// "Trim selected" running on a worker thread, and the result of the last one.
    trim_job: Option<advanced::TrimJob>,
    last_trim: Option<String>,
    /// The low-priority cache purge running on a worker thread, and its last result.
    purge: Option<PurgeJob>,
    last_purge: Option<String>,
    /// The ticked "Clean selected regions" (loaded from the preferences on the first frame), the
    /// result of the last run and the Windows version that gates regions 7 and 8. A run uses
    /// the same administrator request and `purge` job as the cache purge.
    regions: regions::Selection,
    preferences_loaded: bool,
    last_regions: Option<String>,
    /// Read on first use (`windows_version`), not at start-up.
    windows_version: OnceCell<Option<regions::WindowsVersion>>,
    /// Result of the last action, shown above the table.
    status: Option<String>,
    /// The safe optimize run on a worker thread, what the last one did, and the settle
    /// readings still to come.
    run: Option<RunJob>,
    last_run: Option<RunSummary>,
    settling: Option<Settling>,
    /// Why the last automatic run started (the scheduler's text).
    run_note: Option<String>,
    /// The reason of an automatic run that was asked for, kept until the run finds something
    /// to do and replaces `run_note`.
    pending_note: Option<String>,
    /// Memory priorities the tool lowered, and when each app was last trimmed.
    undo: UndoLog,
    cooldowns: Cooldowns,
    /// When each app was last in front, for the idle checks of the safe actions.
    activity: Activity,
    /// Pressure verdict: the model, the Windows readings and the latest assessment.
    pressure: PressureModel,
    signals: Option<Box<dyn SignalSource>>,
    /// The file name of the app in front, read without a process list while the page is
    /// hidden. Replaced in tests.
    foreground_name: Box<dyn Fn() -> Option<String> + Send>,
    assessment: Assessment,
    /// The `memory-pressure` finding for Home: what the last pressure reading said, and the
    /// reading kept for a minute at a time. In memory only.
    health: health::Tracker,
    health_source: health::Source,
    /// Automatic mode (off by default) and the advice shown instead of a run.
    scheduler: Scheduler,
    /// The memory-limit and interval triggers (off by default).
    auto: AutoTriggers,
    advice: Option<String>,
    advice_hidden: bool,
    /// Zero for the models' millisecond clock, when the pressure check last ran and when the
    /// page last drew a frame.
    epoch: Instant,
    last_eval: Instant,
    last_ui: Option<Instant>,
    /// Per-app growth, fed from the process refresh.
    growth: GrowthTracker,
    last_growth: Option<Instant>,
    /// Reads the numbers a run report compares.
    win_sampler: Option<WindowsSampler>,
    /// "Where your memory goes" on Home; rebuilt with every refresh. Not stored anywhere.
    breakdown: groups::Breakdown,
    /// Commit charge on Home; refreshed with the sampler. `None` if the query fails.
    commit: Option<commit::Commit>,
    /// The last 20 honest run reports, newest last. In memory only.
    reports: VecDeque<report::HonestReport>,
    /// Which resource the Home graph shows, how far back, and the measured history.
    selected: home::Metric,
    range: series::Range,
    history: series::History,
    /// Pages read from disk per second, from the latest pressure reading.
    hard_faults: Option<f64>,
    /// Home shows the Advanced tools instead of the overview.
    advanced_open: bool,
    /// Ask-before-trim and keep-a-log switches (saved), the prompt waiting for an answer, and
    /// the last clean for the Home line.
    options: clean_log::Options,
    pending_confirm: Option<Confirm>,
    last_result: Option<freed::RunResult>,
    /// The optional clean log, how many entries it holds, and why the last write or delete
    /// failed.
    log_store: clean_log::FileStore,
    log_count: usize,
    log_note: Option<String>,
}

impl Memory {
    pub fn new() -> Self {
        Self::build(
            memlist::query(),
            commit::query(),
            Some(Box::new(WindowsSignals::new())),
            Box::new(activity::foreground_app_name),
        )
    }

    /// A `Memory` that asks Windows nothing: no memory lists, commit or pressure readings, and
    /// no app in front. Tests add the fakes they need.
    #[cfg(test)]
    fn for_test() -> Self {
        Self::build(None, None, None, Box::new(|| None))
    }

    fn build(
        lists: Option<Lists>,
        commit: Option<commit::Commit>,
        signals: Option<Box<dyn SignalSource>>,
        foreground_name: Box<dyn Fn() -> Option<String> + Send>,
    ) -> Self {
        let epoch = Instant::now();
        Self {
            sampler: None,
            refresh_worker: None,
            refresh_in_flight: 0,
            refresh_action_pending: false,
            // Hidden until the first frame is drawn. The host sends `Suspend` right after the
            // connection when the add-on was started hidden (toolboks `src/supervisor/mod.rs`,
            // "if !managed.visible { send(Suspend) }"), but not when the page is opened first,
            // and a `Resume` only follows a `Suspend`. So `ui` turns this on, and an add-on that
            // never drew already runs the hidden tick (automatic cleaning) without any
            // `suspend` call.
            active: false,
            lists,
            request: None,
            next_request: 0,
            refresh_after_purge: false,
            ctx: None,
            page: 0,
            last_refresh: Instant::now(),
            sort_key: SortKey::WorkingSet,
            descending: true,
            filter: String::new(),
            grouped: false,
            exclusions: HashSet::new(),
            selection: Selection::default(),
            tab: advanced::Tab::Selected,
            was_grouped: false,
            trim_job: None,
            last_trim: None,
            purge: None,
            last_purge: None,
            regions: regions::Selection::default(),
            preferences_loaded: false,
            last_regions: None,
            windows_version: OnceCell::new(),
            status: None,
            run: None,
            last_run: None,
            settling: None,
            run_note: None,
            pending_note: None,
            undo: UndoLog::new(),
            cooldowns: Cooldowns::new(),
            activity: Activity::new(0),
            pressure: PressureModel::new(),
            signals,
            foreground_name,
            health: {
                // "Not checked yet" until the first pressure reading arrives.
                let mut tracker = health::Tracker::new();
                tracker.refresh(health_feed::unix_now(), health::Source::NotRead);
                tracker
            },
            health_source: health::Source::NotRead,
            assessment: Assessment {
                verdict: Verdict::Healthy,
                reasons: Vec::new(),
            },
            scheduler: Scheduler::default(),
            auto: AutoTriggers::default(),
            advice: None,
            advice_hidden: false,
            epoch,
            last_eval: Instant::now(),
            last_ui: None,
            growth: GrowthTracker::new(),
            last_growth: None,
            win_sampler: None,
            breakdown: Default::default(),
            commit,
            reports: VecDeque::new(),
            selected: home::Metric::Memory,
            range: series::Range::default(),
            history: series::History::new(),
            hard_faults: None,
            advanced_open: false,
            options: clean_log::Options::default(),
            pending_confirm: None,
            last_result: None,
            log_store: clean_log::FileStore::default_location(),
            log_count: 0,
            log_note: None,
        }
    }

    fn request_refresh(&mut self, ctx: &egui::Context, action: Option<RefreshAction>) -> bool {
        // Hidden, only a run may start (the hidden tick, or the tray "Clean now", which asks
        // first when it must). A plain refresh or a selected-rows trim waits for the window.
        let run = matches!(&action, Some(RefreshAction::Optimize(_)));
        if (!self.active && !run) || (action.is_none() && self.refresh_in_flight != 0) {
            return false;
        }
        if self.refresh_worker.is_none() {
            let (request, requests) = mpsc::channel::<Option<RefreshAction>>();
            let (result, results) = mpsc::channel();
            let ctx = ctx.clone();
            let thread = std::thread::Builder::new()
                .name("memory-processes".into())
                .spawn(move || {
                    let mut sampler = None;
                    for action in requests {
                        if sampler.is_none() {
                            sampler = Sampler::new();
                        }
                        let mut snapshot = sampler.as_mut().map(Sampler::refresh);
                        // A run needs to know which apps play sound. Read here, on this
                        // thread, so the UI never waits for the audio service.
                        if let (Some(snapshot), Some(RefreshAction::Optimize(_))) =
                            (snapshot.as_mut(), action.as_ref())
                        {
                            snapshot.audio = audio::AudioState::read(&audio::WindowsSessions);
                        }
                        if result.send((snapshot, action)).is_err() {
                            break;
                        }
                        ctx.request_repaint();
                    }
                });
            if thread.is_err() {
                self.status = Some("Could not start memory sampling.".into());
                return false;
            }
            self.refresh_worker = Some(RefreshWorker {
                request,
                result: results,
            });
        }
        let worker = self.refresh_worker.as_ref().unwrap();
        let is_action = action.is_some();
        if worker.request.send(action).is_err() {
            self.refresh_worker = None;
            self.refresh_in_flight = 0;
            self.refresh_action_pending = false;
            return false;
        }
        self.refresh_in_flight += 1;
        self.refresh_action_pending |= is_action;
        true
    }

    fn refresh_now(&mut self, ctx: &egui::Context) {
        let _ = self.request_refresh(ctx, None);
    }

    fn poll_refresh(&mut self, ctx: &egui::Context) {
        while let Some(worker) = &self.refresh_worker {
            let (snapshot, action) = match worker.result.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.refresh_worker = None;
                    self.refresh_in_flight = 0;
                    self.refresh_action_pending = false;
                    self.status = Some("Memory sampling stopped. Try again.".into());
                    break;
                }
            };
            self.refresh_in_flight -= 1;
            self.last_refresh = Instant::now();
            let available = snapshot.is_some();
            if let Some(snapshot) = snapshot.filter(|_| self.active || action.is_some()) {
                if self.active {
                    self.breakdown = groups::breakdown(&snapshot.processes, snapshot.totals.used);
                }
                self.sampler = Some(snapshot);
                if self.active {
                    self.lists = memlist::query();
                    self.commit = commit::query();
                    self.feed_growth();
                }
            }
            if let Some(action) = action {
                self.refresh_action_pending = false;
                if !available {
                    self.status = Some("Could not read a fresh process list. Try again.".into());
                } else {
                    match action {
                        RefreshAction::Optimize(trigger) => {
                            self.run_from_snapshot(ctx, trigger);
                        }
                        RefreshAction::Trim { selection, grouped } => {
                            self.trim_from_snapshot(ctx, &selection, grouped);
                        }
                    }
                }
                if !self.active {
                    self.sampler = None;
                    self.refresh_worker = None;
                    self.refresh_in_flight = 0;
                }
            }
        }
    }

    /// Clicking the active column flips the order; a new column starts in its natural order.
    fn apply_sort(&mut self, key: SortKey) {
        if self.sort_key == key {
            self.descending = !self.descending;
        } else {
            self.sort_key = key;
            // Numbers read best largest-first, names and pids smallest-first.
            self.descending = matches!(key, SortKey::WorkingSet | SortKey::Private);
        }
    }

    fn toggle_exclusion(&mut self, name: String) {
        if !self.exclusions.remove(&name) {
            self.exclusions.insert(name);
        }
    }
}

impl Default for Memory {
    fn default() -> Self {
        Self::new()
    }
}

/// Page names, in order. They match `host-services.json`.
pub const PAGES: [&str; 2] = ["Home", "Advanced"];
pub const ID: &str = "toolboks.memory-cleaner";

impl Memory {
    /// Loads the saved settings once (from `load_preferences`, or the first frame).
    fn load_prefs(&mut self, preferences: &std::collections::BTreeMap<String, serde_json::Value>) {
        if !self.preferences_loaded {
            self.regions = regions::Selection::load(preferences);
            self.auto = AutoTriggers::new(AutoSettings::load(preferences));
            self.options = clean_log::Options::load(preferences);
            self.log_count = clean_log::load(&self.log_store).len();
            self.preferences_loaded = true;
        }
    }

    /// The Windows version, asked on first use (the Advanced page's regions).
    fn windows_version(&self) -> Option<regions::WindowsVersion> {
        *self.windows_version.get_or_init(regions::windows_version)
    }

    fn tab_name(&self) -> &'static str {
        match self.tab {
            advanced::Tab::Selected => "selected",
            advanced::Tab::System => "system",
            advanced::Tab::Rules => "rules",
        }
    }
}

/// Applies the Toolboks look (fonts, text sizes, spacing, colours) the first time the add-on
/// draws. The add-on runs in its own process, so the host's own set-up does not reach it.
fn style_once(ctx: &egui::Context) {
    let id = egui::Id::NULL.with("toolboks-style-applied");
    if !ctx.data(|data| data.get_temp::<bool>(id)).unwrap_or(false) {
        toolboks_shared::ui::apply_style(ctx);
        ctx.data_mut(|data| data.insert_temp(id, true));
    }
}

impl NativeAddon for Memory {
    fn ui(
        &mut self,
        ui: &mut egui::Ui,
        preferences: &mut std::collections::BTreeMap<String, serde_json::Value>,
    ) -> bool {
        style_once(ui.ctx());
        // The page is shown (a page opened first gets no `Resume`).
        self.active = true;
        self.load_prefs(preferences);
        self.draw(ui, self.page);
        self.confirm_ui(ui.ctx());
        let regions_changed = self.regions.store(preferences);
        let auto_changed = self.auto.settings().store(preferences);
        let options_changed = self.options.store(preferences);
        regions_changed || auto_changed || options_changed
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
    /// The host sends the saved settings before any frame, so automatic cleaning is on from
    /// the start even if the page is never opened this session.
    fn load_preferences(
        &mut self,
        preferences: &std::collections::BTreeMap<String, serde_json::Value>,
    ) {
        self.load_prefs(preferences);
    }
    fn prepare_admin(
        &mut self,
        request: &toolboks_addon_sdk::AdminRequest,
    ) -> Result<Box<dyn toolboks_addon_sdk::admin::AdminChange>, String> {
        admin::prepare_admin(request)
    }
    fn admin_result(&mut self, id: u64, outcome: toolboks_addon_sdk::AdminOutcome) {
        self.purge_finished(id, outcome);
    }
    fn take_host_request(&mut self) -> Option<HostRequest> {
        self.request.take()
    }
    fn tick(&mut self, ctx: &egui::Context) {
        self.ctx = Some(ctx.clone());
        if self.refresh_after_purge {
            self.refresh_after_purge = false;
            self.refresh_now(ctx);
        }
        self.poll_refresh(ctx);
        if self.refresh_worker.is_none() {
            self.refresh_now(ctx);
        }
        self.background(ctx);
        self.update_health(health_feed::unix_now());
    }
    fn suspend(&mut self) {
        self.active = false;
        self.sampler = None;
        if !self.refresh_action_pending {
            self.refresh_worker = None;
            self.refresh_in_flight = 0;
        }
        self.last_ui = None;
        self.breakdown = Default::default();
        self.growth = GrowthTracker::new();
        self.last_growth = None;
        self.history = series::History::new();
        self.activity = Activity::new(self.now_ms());
        self.selection = Selection::default();
        if self.run.is_none() && self.settling.is_none() {
            self.win_sampler = None;
        }
    }
    fn resume(&mut self) {
        self.active = true;
        if self.signals.is_none() {
            self.signals = Some(Box::new(WindowsSignals::new()));
        }
    }
    /// A running trim or optimize keeps going, and finishes, while the window is hidden.
    fn has_background_work(&self) -> bool {
        // Always: the memory-pressure finding and the automatic cleaning (pressure scheduler,
        // memory limit, interval) run from the background tick while the window is hidden.
        // The tick does two time comparisons unless a reading is due.
        true
    }
    fn tick_background(&mut self) {
        let ctx = self.ctx.clone().unwrap_or_default();
        if !self.active && self.refresh_action_pending {
            self.poll_refresh(&ctx);
        }
        self.poll_run(&ctx);
        self.poll_trim(&ctx);
        self.settle_tick();
        self.health_background_tick(&ctx);
        if self.settling.is_none() && self.sampler.is_none() && self.run.is_none() {
            self.win_sampler = None;
        }
    }
    fn status_items(&self) -> Vec<StatusItem> {
        match self.sampler.as_ref() {
            Some(s) if s.totals.total > 0 => vec![StatusItem {
                text: format!("RAM {:.0}%", s.totals.used_fraction() * 100.0),
                tooltip: None,
            }],
            _ => Vec::new(),
        }
    }
    fn findings(&self) -> Vec<Finding> {
        self.health_findings()
    }
    fn check_now(&mut self) {
        self.health_check_now();
    }
    fn tray_items(&self) -> Vec<TrayItem> {
        self.clean_now_tray_items()
    }
    fn tray_action(&mut self, id: &str) {
        self.clean_now_tray_action(id);
    }
    fn tutorial_steps(&self) -> Vec<TutorialStep> {
        tutorial::steps()
    }
    fn tutorial_location(&self) -> String {
        if self.page == 0 {
            "0".into()
        } else {
            format!("1/{}", self.tab_name())
        }
    }
    fn tutorial_navigate(&mut self, section: &str) {
        let mut parts = section.split('/');
        self.navigate(parts.next().unwrap_or("0"));
        if let Some(tab) = parts.next() {
            self.tab = match tab {
                "system" => advanced::Tab::System,
                "rules" => advanced::Tab::Rules,
                _ => advanced::Tab::Selected,
            };
        }
    }
}

impl Memory {
    /// Draws page `page`, or a short note while the first reading is still being collected.
    fn draw(&mut self, ui: &mut egui::Ui, page: usize) {
        if self.sampler.is_none() {
            if self.refresh_in_flight > 0 {
                ui.label("Collecting memory information…");
            } else {
                ui.label("Memory information is not available. Toolboks will try again.");
                if self.last_refresh.elapsed() >= REFRESH {
                    self.refresh_now(ui.ctx());
                }
                ui.request_repaint_after(REFRESH);
            }
            return;
        }

        self.last_ui = Some(Instant::now());
        if self.last_refresh.elapsed() >= REFRESH {
            self.refresh_now(ui.ctx());
        }
        ui.request_repaint_after(REFRESH);

        if page == 0 {
            // Home lays out its own three zones and scrolls inside them.
            self.home_ui(ui);
            return;
        }
        self.advanced_ui(ui);
    }
}
