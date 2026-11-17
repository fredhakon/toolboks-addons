//! Resource Monitor: live CPU and memory graphs.

use std::time::{Duration, Instant};

mod connections;
mod detail;
mod details;
mod gpu;
mod history;
mod history_page;
mod history_settings;
mod list;
mod lower_confirm;
mod memory_breakdown;
mod nvml;
mod overview;
mod power;
mod processes;
mod recording;
mod sampler;
mod tutorial;

use history::{History, Range};
use sampler::{GpuSample, Info, Msg, Sample, Sampler};
use toolboks_addon_sdk::{NativeAddon, StatusItem, TutorialStep};

/// Space between the range row and the page below it.
const GAP_BELOW_BAR: f32 = 8.0;

/// A sample older than this means the sampler is no longer live.
const LIVE_WITHIN: Duration = Duration::from_secs(5);

/// Histories keyed by name (engine, drive mount, adapter), `N` series each.
pub(crate) struct Keyed<const N: usize> {
    items: Vec<(String, [History; N])>,
}

impl<const N: usize> Keyed<N> {
    fn new() -> Self {
        Self { items: Vec::new() }
    }

    /// Finds the series for `key`, creating empty ones when it is new.
    fn find_or_create(&mut self, key: &str) -> &mut [History; N] {
        let at = match self.items.iter().position(|(k, _)| k == key) {
            Some(i) => i,
            None => {
                let series = std::array::from_fn(|_| History::new());
                self.items.push((key.to_string(), series));
                self.items.len() - 1
            }
        };
        &mut self.items[at].1
    }

    /// Pushes one sample; known keys missing from `present` get 0.0.
    fn update(&mut self, present: &[(&str, [f32; N])]) {
        for (key, values) in present {
            let series = self.find_or_create(key);
            for (h, v) in series.iter_mut().zip(values) {
                h.push(*v);
            }
        }
        for (key, series) in &mut self.items {
            if !present.iter().any(|(k, _)| k == key) {
                for h in series.iter_mut() {
                    h.push(0.0);
                }
            }
        }
    }

    pub(crate) fn get(&self, key: &str) -> Option<&[History; N]> {
        self.items.iter().find(|(k, _)| k == key).map(|(_, s)| s)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, &[History; N])> {
        self.items.iter().map(|(k, s)| (k.as_str(), s))
    }
}

/// A row of the Overview list; the one selected decides what the detail pane shows.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub(crate) enum Resource {
    #[default]
    Cpu,
    Memory,
    Gpu,
    Disk,
    Network,
    Power,
}

impl Resource {
    pub(crate) const ALL: [Resource; 6] = [
        Resource::Cpu,
        Resource::Memory,
        Resource::Gpu,
        Resource::Disk,
        Resource::Network,
        Resource::Power,
    ];

    /// The section name the guided tour uses for this resource.
    fn key(self) -> &'static str {
        match self {
            Resource::Cpu => "cpu",
            Resource::Memory => "memory",
            Resource::Gpu => "gpu",
            Resource::Disk => "disk",
            Resource::Network => "network",
            Resource::Power => "power",
        }
    }
}

pub struct Resources {
    /// The row picked in the Overview list (in memory only).
    selected: Resource,
    /// How far back the graphs look.
    range: Range,
    sampler: Option<Sampler>,
    info: Option<Info>,
    last: Option<Sample>,
    /// When `last` arrived.
    last_at: Option<Instant>,
    cpu: History,
    mem: History,
    disk_read: History,
    disk_write: History,
    net_down: History,
    net_up: History,
    gpu_load: History,
    gpu_power: History,
    /// Rated power of the processor for the power estimate, set by the user.
    cpu_rated_w: f32,
    cores: Vec<History>,
    engines: Keyed<1>,
    drives: Keyed<2>,
    adapters: Keyed<2>,
    /// Latest sample that carried GPU data.
    gpu: Option<GpuSample>,
    /// Samples in a row that arrived without GPU data.
    gpu_missing: u32,
    processes: processes::Page,
    connections: connections::Page,
    /// Which page is shown: 0 Overview, 1 Details, 2 Processes, 3 Connections, 4 History.
    page: usize,
    history: history_page::Page,
    /// The switch, size, delete and folder group and the one-time note on the History page.
    history_settings: history_settings::Settings,
    /// Resource history recorder (on unless the preference switches it off). Kept across
    /// `suspend`, so it records while the page is closed and Toolboks sits in the tray.
    recording: recording::Control,
}

/// Starts the real recorder, saving to the history folder.
#[cfg(not(test))]
fn start_recorder() -> Option<recording::Recorder> {
    recording::Recorder::start_system()
}

/// Unit tests never record the real system or write a history folder.
#[cfg(test)]
fn start_recorder() -> Option<recording::Recorder> {
    recording::fake_recorder(Duration::from_secs(3600))
}

impl Resources {
    pub fn new() -> Self {
        Self {
            selected: Resource::default(),
            range: Range::default(),
            sampler: None,
            info: None,
            last: None,
            last_at: None,
            cpu: History::new(),
            mem: History::new(),
            disk_read: History::new(),
            disk_write: History::new(),
            net_down: History::new(),
            net_up: History::new(),
            gpu_load: History::new(),
            gpu_power: History::new(),
            cpu_rated_w: power::DEFAULT_RATED_W,
            cores: Vec::new(),
            engines: Keyed::new(),
            drives: Keyed::new(),
            adapters: Keyed::new(),
            gpu: None,
            gpu_missing: 0,
            processes: processes::Page::new(),
            connections: connections::Page::new(),
            page: 0,
            history: history_page::Page::new(),
            history_settings: history_settings::Settings::default(),
            recording: recording::Control::default(),
        }
    }
}

impl Default for Resources {
    fn default() -> Self {
        Self::new()
    }
}

/// Page names, in order. They match `host-services.json`.
pub const PAGES: [&str; 5] = ["Overview", "Details", "Processes", "Connections", "History"];
pub const ID: &str = "toolboks.resource-monitor";
/// Index of the History page in [`PAGES`].
const HISTORY_PAGE: usize = 4;

impl Resources {
    /// The "how far back" switch above the graph pages. `with_rated` adds the processor rated
    /// power box at the right (the Details page); the Overview keeps that box in its Power view
    /// and puts the switch at the right of the row.
    fn range_bar(&mut self, ui: &mut egui::Ui, with_rated: bool) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if with_rated {
                ui.label(egui::RichText::new("Graphs show the last").weak());
                self.range_pills(ui);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    power::rated_power_input(ui, &mut self.cpu_rated_w);
                });
            } else {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    // Right-to-left, so add the last pill first.
                    for range in Range::ALL.into_iter().rev() {
                        self.range_pill(ui, range);
                    }
                    ui.label(egui::RichText::new("Graphs show the last").weak());
                });
            }
        });
        ui.add_space(GAP_BELOW_BAR);
    }

    fn range_pills(&mut self, ui: &mut egui::Ui) {
        for range in Range::ALL {
            self.range_pill(ui, range);
        }
    }

    fn range_pill(&mut self, ui: &mut egui::Ui, range: Range) {
        if toolboks_shared::ui::pill(ui, range.label(), self.range == range).clicked() {
            self.range = range;
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

impl NativeAddon for Resources {
    /// Starts or stops the history recorder from the add-on's own preferences (default on).
    fn load_preferences(
        &mut self,
        preferences: &std::collections::BTreeMap<String, serde_json::Value>,
    ) {
        self.recording
            .apply(recording::wanted(preferences), start_recorder);
    }
    fn ui(
        &mut self,
        ui: &mut egui::Ui,
        preferences: &mut std::collections::BTreeMap<String, serde_json::Value>,
    ) -> bool {
        style_once(ui.ctx());
        self.recording.poll(start_recorder);
        let mut changed = false;
        if self.page == 4 {
            changed = self.history_settings.ui(
                ui,
                preferences,
                &mut self.recording,
                &recording::disk::directory(),
                start_recorder,
            );
        }
        self.draw(ui, self.page);
        changed
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
    fn tick(&mut self, ctx: &egui::Context) {
        self.poll(ctx);
    }
    /// Stops sampling and drops the graphs, as the built-in tool does when it is not shown.
    /// The history recorder is the one thing kept: it runs on its own thread.
    fn suspend(&mut self) {
        let page = self.page;
        let recording = std::mem::take(&mut self.recording);
        *self = Self::new();
        self.page = page;
        self.recording = recording;
    }
    /// Normally false: the recorder has its own thread and needs no 200 ms background ticks,
    /// so a suspended Resource Monitor wakes nothing (and does nothing at all with recording
    /// off). True only while a restart or a delete waits for an old recorder thread to end.
    fn has_background_work(&self) -> bool {
        self.recording.pending()
    }
    fn tick_background(&mut self) {
        self.recording.poll(start_recorder);
    }
    fn status_items(&self) -> Vec<StatusItem> {
        let live = self.last_at.is_some_and(|t| t.elapsed() < LIVE_WITHIN);
        let (true, Some(sample)) = (live, &self.last) else {
            return Vec::new();
        };
        let item = |text: String| StatusItem {
            text,
            tooltip: None,
        };
        let mut items = vec![item(format!("CPU {:.0}%", sample.cpu))];
        if let Some(load) = self.gpu.as_ref().and_then(|g| g.load) {
            items.push(item(format!("GPU {load:.0}%")));
        }
        items
    }
    fn tutorial_steps(&self) -> Vec<TutorialStep> {
        tutorial::steps()
    }
    fn tutorial_location(&self) -> String {
        if self.page == 0 {
            format!("0/{}", self.selected.key())
        } else {
            self.page.to_string()
        }
    }
    fn tutorial_navigate(&mut self, section: &str) {
        let mut parts = section.split('/');
        self.navigate(parts.next().unwrap_or("0"));
        if let Some(key) = parts.next() {
            self.selected = Resource::ALL
                .into_iter()
                .find(|r| r.key() == key)
                .unwrap_or_default();
        }
    }
}

impl Resources {
    /// System totals for the Processes headers; None until a fresh reading exists.
    fn process_totals(&self) -> Option<processes::Totals> {
        let fresh = self.last_at.is_some_and(|t| t.elapsed() < LIVE_WITHIN);
        let sample = self.last.as_ref().filter(|_| fresh)?;
        Some(processes::Totals {
            cpu: sample.cpu,
            mem_used: sample.mem_used,
            mem_total: self.info.as_ref().map_or(0, |i| i.mem_total),
            disk_active: sample.disk_active,
            disk_bps: sample.disk_read_bps + sample.disk_write_bps,
            gpu: sample.gpu.as_ref().and_then(|g| g.load),
        })
    }

    /// "Open in history" on the Processes page: the History page, showing that app.
    fn open_in_history(&mut self, app: &str) {
        self.history.show_app(app);
        self.page = HISTORY_PAGE;
    }

    /// What the History page reads: the history folder, and the run recording now.
    fn history_inputs(&self) -> history_page::Inputs {
        let live = self.recording.run().zip(self.recording.store());
        let live_steps = live
            .as_ref()
            .and_then(|(_, s)| s.lock().ok().map(|s| s.fine().len()))
            .unwrap_or(0);
        history_page::Inputs {
            sources: Some(recording::query::Sources {
                dir: recording::disk::directory(),
                live,
            }),
            recording: self.recording.is_on(),
            live_steps,
        }
    }

    fn poll(&mut self, ctx: &egui::Context) {
        self.processes.tick();
        self.connections.tick();
        let sampler = self.sampler.get_or_insert_with(|| Sampler::start(ctx));
        for msg in sampler.drain() {
            match msg {
                Msg::Info(info) => self.info = Some(info),
                Msg::Sample(sample) => {
                    self.cpu.push(sample.cpu);
                    let total = self.info.as_ref().map_or(0, |i| i.mem_total);
                    self.mem.push(overview::mem_percent(sample.mem_used, total));
                    self.disk_read.push(sample.disk_read_bps as f32);
                    self.disk_write.push(sample.disk_write_bps as f32);
                    self.net_down.push(sample.net_down_bps as f32);
                    self.net_up.push(sample.net_up_bps as f32);
                    self.cores.resize_with(sample.cores.len(), History::new);
                    for (h, v) in self.cores.iter_mut().zip(&sample.cores) {
                        h.push(*v);
                    }
                    let drives: Vec<_> = sample
                        .drives
                        .iter()
                        .map(|d| (d.mount.as_str(), [d.read_bps as f32, d.write_bps as f32]))
                        .collect();
                    self.drives.update(&drives);
                    let adapters: Vec<_> = sample
                        .adapters
                        .iter()
                        .map(|a| (a.name.as_str(), [a.down_bps as f32, a.up_bps as f32]))
                        .collect();
                    self.adapters.update(&adapters);
                    let engines: Vec<_> = sample
                        .gpu
                        .iter()
                        .flat_map(|g| g.engines.iter())
                        .map(|(n, v)| (n.as_str(), [*v]))
                        .collect();
                    self.engines.update(&engines);
                    self.gpu_power.push(
                        sample
                            .gpu
                            .as_ref()
                            .and_then(|gpu| gpu.power_w)
                            .unwrap_or(f32::NAN),
                    );
                    match &sample.gpu {
                        Some(g) => {
                            self.gpu_load.push(g.load.unwrap_or(0.0));
                            self.gpu = Some(g.clone());
                            self.gpu_missing = 0;
                        }
                        None => self.gpu_missing = self.gpu_missing.saturating_add(1),
                    }
                    let gpu_pids = sample.gpu.as_ref().map(|_| sample.gpu_pids.as_slice());
                    self.processes.set_gpu(gpu_pids);
                    self.last = Some(*sample);
                    self.last_at = Some(Instant::now());
                }
            }
        }
    }

    fn draw(&mut self, ui: &mut egui::Ui, page: usize) {
        let sampler = self.sampler.get_or_insert_with(|| Sampler::start(ui.ctx()));
        sampler.ping();
        match page {
            2 => {
                self.processes.ui(ui, self.process_totals());
                if let Some(app) = self.processes.take_open_history() {
                    self.open_in_history(&app);
                }
                return;
            }
            3 => {
                self.connections.ui(ui);
                return;
            }
            4 => {
                let inputs = self.history_inputs();
                self.history.ui(ui, inputs);
                return;
            }
            _ => {}
        }
        let with_rated = page == 1;
        toolboks_addon_sdk::tutorial::region(ui, "resource-range", |ui| {
            self.range_bar(ui, with_rated)
        });
        if page == 1 {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| self.details_ui(ui));
        } else {
            // The Overview scrolls its detail pane itself; the list stays put.
            self.overview_ui(ui);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_is_on_by_default_and_survives_suspend() {
        let mut addon = Resources::new();
        let off = std::collections::BTreeMap::from([(
            recording::PREF_KEY.to_string(),
            serde_json::json!(false),
        )]);
        addon.load_preferences(&off);
        assert!(!addon.recording.is_on(), "off when switched off");
        addon.load_preferences(&Default::default());
        assert!(addon.recording.is_on(), "on by default");
        let store = addon.recording.store().unwrap();
        addon.page = 2;
        addon.suspend();
        assert_eq!(addon.page, 2);
        assert!(std::sync::Arc::ptr_eq(
            &store,
            &addon.recording.store().unwrap()
        ));
        addon.resume();
        assert!(addon.recording.is_on());
        // Switched off in the preferences: the recorder goes, and nothing waits to start.
        addon.load_preferences(&off);
        assert!(!addon.recording.is_on());
        assert!(!addon.has_background_work());
    }

    /// The real SDK runtime loop: the host shows the page, then switches to another add-on
    /// (or hides the window to the tray), which arrives as `Suspend`. Recording keeps adding
    /// steps while suspended, and switching it off in the preferences ends the thread.
    #[test]
    fn recording_keeps_sampling_while_suspended_and_stops_when_switched_off() {
        use std::time::{Duration, Instant};
        use toolboks_addon_sdk::runtime::proto::memory::duplex;
        use toolboks_addon_sdk::runtime::proto::{
            self, AddonMessage, Hello, HostMessage, InputBatch, Theme,
        };
        let wait_until = |what: &str, mut done: Box<dyn FnMut() -> bool>| {
            let until = Instant::now() + Duration::from_secs(20);
            while !done() {
                assert!(Instant::now() < until, "timed out: {what}");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let mut addon = Resources::new();
        addon
            .recording
            .apply(true, || recording::fake_recorder(Duration::from_millis(10)));
        let store = addon.recording.store().unwrap();
        let (host, addon_side) = duplex();
        let runtime = std::thread::spawn(move || {
            toolboks_addon_sdk::runtime::run_addon_on(addon_side, addon, ID)
        });
        let (mut reader, mut writer) = host.split();
        proto::host_handshake(&mut reader, &mut writer, &Hello::new(ID, "test")).unwrap();
        std::thread::spawn(move || while reader.read::<AddonMessage>().is_ok() {});
        let on = serde_json::json!({ recording::PREF_KEY: true }).to_string();
        writer
            .write(&HostMessage::LoadPreferences { json: on })
            .unwrap();
        writer
            .write(&HostMessage::Input(InputBatch {
                seq: 1,
                size: [800.0, 600.0],
                pixels_per_point: 1.0,
                theme: Theme::Dark,
                window_focused: true,
                events: vec![],
            }))
            .unwrap();
        // Another add-on is opened, or the window goes to the tray.
        writer.write(&HostMessage::Suspend).unwrap();
        let steps = || store.lock().unwrap().fine().len();
        let at_suspend = steps();
        wait_until(
            "steps while suspended",
            Box::new(|| steps() >= at_suspend + 5),
        );
        // Switched off: the recorder thread ends and releases its history.
        let off = serde_json::json!({ recording::PREF_KEY: false }).to_string();
        writer
            .write(&HostMessage::LoadPreferences { json: off })
            .unwrap();
        wait_until(
            "recorder thread ended",
            Box::new(|| std::sync::Arc::strong_count(&store) == 1),
        );
        let after_off = steps();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(steps(), after_off, "nothing is recorded once off");
        writer.write(&HostMessage::Quit).unwrap();
        runtime.join().unwrap().unwrap();
    }

    #[test]
    fn open_in_history_switches_to_the_history_page() {
        assert_eq!(PAGES[HISTORY_PAGE], "History");
        let mut addon = Resources::new();
        addon.navigate("2");
        addon.open_in_history("chrome.exe");
        assert_eq!(addon.page_index(), HISTORY_PAGE);
    }

    #[test]
    fn find_or_create_reuses_series() {
        let mut k = Keyed::<2>::new();
        k.find_or_create("a")[0].push(1.0);
        k.find_or_create("b");
        k.find_or_create("a")[0].push(2.0);
        assert_eq!(k.iter().count(), 2);
        assert_eq!(k.get("a").unwrap()[0].view(Range::Minute), &[1.0, 2.0]);
        assert!(k.get("c").is_none());
    }

    #[test]
    fn update_pushes_zero_for_absent_keys() {
        let mut k = Keyed::<1>::new();
        k.update(&[("a", [5.0]), ("b", [6.0])]);
        k.update(&[("a", [7.0])]);
        assert_eq!(k.get("a").unwrap()[0].view(Range::Minute), &[5.0, 7.0]);
        assert_eq!(k.get("b").unwrap()[0].view(Range::Minute), &[6.0, 0.0]);
    }
}
