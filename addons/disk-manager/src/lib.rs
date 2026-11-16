//! Native Disk Manager workspace, adapted from the standalone Stratum scan engine.
mod category;
mod explorer;
mod folder_picker;
mod health;
mod health_findings;
mod map;
mod optimization;
mod saved_scan;
mod scanner;
mod theme;
mod tutorial;
use egui::RichText;
use scanner::{Node, Progress, Query, State};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    time::Duration,
};
use toolboks_addon_sdk::tutorial as guide;
use toolboks_addon_sdk::NativeAddon;

pub const PROVIDER: &str = "stratum.disk-manager";
pub const PACKAGE: &str = include_str!("../disk-manager.tbaddon");
pub const ID: &str = "toolboks.disk-manager";

/// Validates and previews one administrator action for the separate elevated helper. It changes
/// nothing; the helper's own confirmation window decides.
pub fn prepare_admin(
    request: &toolboks_addon_sdk::AdminRequest,
) -> Result<Box<dyn toolboks_addon_sdk::admin::AdminChange>, String> {
    optimization::prepare_admin(request)
}
pub const PAGES: [&str; 5] = [
    "Overview",
    "Largest files",
    "File types",
    "Scan details",
    "Optimization",
];
use theme::{card, weak, MUTED};

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    path: String,
    exclude: String,
    min_bytes: u64,
    older_days: u64,
    sort: String,
    remember: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            path: if cfg!(windows) {
                "C:\\".into()
            } else {
                "/".into()
            },
            exclude: String::new(),
            min_bytes: 0,
            older_days: 0,
            sort: "size".into(),
            remember: true,
        }
    }
}
#[derive(Clone)]
struct Drive {
    path: String,
    total: u64,
    free: u64,
}
#[derive(Clone, Copy)]
enum DeleteScope {
    All,
    Drive,
    Disable,
}
pub struct DiskManager {
    settings: Settings,
    loaded: bool,
    styled: bool,
    shared: Arc<Mutex<State>>,
    cancel: Arc<AtomicBool>,
    drives: Vec<Drive>,
    page: usize,
    parent: usize,
    search: String,
    offset: usize,
    options: bool,
    message: Option<String>,
    export_kind: Option<bool>,
    export_path: String,
    export_job: Option<mpsc::Receiver<Result<String, String>>>,
    last_generation: u64,
    listing_cache: Option<(String, scanner::Listing)>,
    map_state: map::Interaction,
    map_cache: Option<(u64, usize, Arc<map::MapLayout>)>,
    picker: Option<mpsc::Receiver<Result<Option<String>, String>>>,
    tutorial_snapshot: Option<(usize, bool, usize, usize)>,
    optimization: optimization::Panel,
    reporter: health_findings::Reporter,
    cache: Option<saved_scan::Worker>,
    cache_started: bool,
    reload_cache: bool,
    saved_generation: u64,
    restore: Option<(String, Option<String>)>,
    delete_confirm: Option<DeleteScope>,
}
impl Default for DiskManager {
    fn default() -> Self {
        let optimization = optimization::Panel::default();
        Self {
            reporter: health_findings::Reporter::new(optimization.health_cache()),
            optimization,
            settings: Default::default(),
            loaded: false,
            styled: false,
            shared: Default::default(),
            cancel: Arc::new(AtomicBool::new(false)),
            drives: drives(),
            page: 0,
            parent: 0,
            search: String::new(),
            offset: 0,
            options: false,
            message: None,
            export_kind: None,
            export_path: String::new(),
            export_job: None,
            last_generation: 0,
            listing_cache: None,
            map_state: Default::default(),
            map_cache: None,
            picker: None,
            tutorial_snapshot: None,
            cache: None,
            cache_started: false,
            reload_cache: false,
            saved_generation: 0,
            restore: None,
            delete_confirm: None,
        }
    }
}
impl DiskManager {
    /// Work that is not the periodic drive-health read.
    #[cfg(test)]
    fn has_user_work(&self) -> bool {
        self.export_job.is_some()
            || self.picker.is_some()
            || self.optimization.busy()
            || self.cache.as_ref().is_some_and(|w| w.pending > 0)
    }
}
impl Drop for DiskManager {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

fn button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    theme::primary(ui, label)
}
pub fn size(bytes: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", units[unit])
    }
}
fn display_path(path: &str) -> String {
    if let Some(p) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{p}")
    } else {
        path.strip_prefix(r"\\?\").unwrap_or(path).into()
    }
}
fn path_key(path: &str) -> String {
    display_path(path.trim())
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}
fn age(time: u64) -> String {
    let seconds = scanner::now().saturating_sub(time);
    if time == 0 {
        "not recorded".into()
    } else if seconds < 60 {
        "just now".into()
    } else if seconds < 3600 {
        format!("{} min ago", seconds / 60)
    } else if seconds < 86400 {
        format!("{} h ago", seconds / 3600)
    } else {
        format!("{} days ago", seconds / 86400)
    }
}
fn elapsed(s: f64) -> String {
    if s < 60.0 {
        format!("{s:.1}s")
    } else {
        format!("{}m {}s", s as u64 / 60, s as u64 % 60)
    }
}

impl DiskManager {
    fn poll_workers(&mut self) {
        if let Some(job) = &self.export_job {
            match job.try_recv() {
                Ok(r) => {
                    self.message = Some(r.unwrap_or_else(|e| e));
                    self.export_job = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.message = Some("The export worker stopped unexpectedly.".into());
                    self.export_job = None;
                }
                _ => {}
            }
        }
        if let Some(picker) = &self.picker {
            match picker.try_recv() {
                Ok(Ok(path)) => {
                    if let Some(path) = path {
                        self.settings.path = path;
                    }
                    self.picker = None;
                }
                Ok(Err(error)) => {
                    self.message = Some(error);
                    self.picker = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.message = Some("The folder picker closed unexpectedly.".into());
                    self.picker = None;
                }
                _ => {}
            }
        }
    }
    fn exclusions(&self) -> Vec<String> {
        scanner::exclusions(
            &self
                .settings
                .exclude
                .split(';')
                .map(str::to_owned)
                .collect::<Vec<_>>(),
        )
    }
    fn quick_available(&self, p: &Progress, nodes: &[Node]) -> bool {
        !nodes.is_empty()
            && !p.cancelled
            && p.full_scanned_at > 0
            && path_key(&p.root) == path_key(&self.settings.path)
            && p.exclusions == self.exclusions()
    }
    fn start(&mut self, quick: bool) {
        if self.shared.lock().unwrap().progress.running
            || self.cache.as_ref().is_some_and(|w| w.loading)
        {
            return;
        }
        let root = match std::fs::canonicalize(self.settings.path.trim()) {
            Ok(p) if p.is_dir() => p,
            _ => {
                self.message = Some(
                    "That folder cannot be opened. Check the path and your permissions.".into(),
                );
                return;
            }
        };
        let (previous, old) = self.progress();
        let quick = quick
            && self.quick_available(&previous, &old)
            && saved_scan::volume_id(&root).as_deref() == Some(previous.volume.as_str());
        if path_key(&previous.root) == path_key(&root.to_string_lossy()) && !old.is_empty() {
            self.restore = Some((
                old.get(self.parent)
                    .map_or_else(|| previous.root.clone(), |n| n.path.clone()),
                self.map_state
                    .selected
                    .and_then(|i| old.get(i))
                    .map(|n| n.path.clone()),
            ));
        } else {
            self.parent = 0;
            self.offset = 0;
            self.search.clear();
            self.restore = None;
        }
        let mut state = self.shared.lock().unwrap();
        let generation = state.progress.generation + 1;
        state.progress = Progress {
            running: true,
            root: root.to_string_lossy().into(),
            generation,
            ..Default::default()
        };
        state.nodes = Arc::new(vec![]);
        drop(state);
        self.last_generation = generation;
        self.map_cache = None;
        self.listing_cache = None;
        self.cancel.store(false, Ordering::Relaxed);
        let shared = self.shared.clone();
        let cancel = self.cancel.clone();
        let excludes = self.exclusions();
        match std::thread::Builder::new()
            .name("disk-scan".into())
            .spawn(move || {
                if quick {
                    scanner::rescan(old, previous, shared, cancel);
                } else {
                    drop(old);
                    scanner::scan(root, excludes, shared, cancel);
                }
            }) {
            Ok(_) => self.message = None,
            Err(error) => {
                self.shared.lock().unwrap().progress.running = false;
                self.message = Some(format!("Could not start the disk scan: {error}"));
            }
        }
    }
    fn cache_tick(&mut self, active: bool) {
        if self.tutorial_snapshot.is_some() {
            return;
        }
        if !self.cache_started {
            if !active {
                return;
            }
            self.cache_started = true;
            let mut worker = saved_scan::Worker::new();
            if self.settings.remember {
                worker.send(saved_scan::Command::Load(self.exclusions()));
            } else {
                worker.send(saved_scan::Command::Delete(None));
            }
            self.cache = Some(worker);
        }
        if active && self.reload_cache && !self.cache.as_ref().unwrap().loading {
            self.reload_cache = false;
            if self.settings.remember {
                let exclusions = self.exclusions();
                self.cache
                    .as_mut()
                    .unwrap()
                    .send(saved_scan::Command::Load(exclusions));
            }
        }
        let events: Vec<_> = self.cache.as_ref().unwrap().receiver.try_iter().collect();
        for event in events {
            let worker = self.cache.as_mut().unwrap();
            worker.pending = worker.pending.saturating_sub(1);
            match event {
                saved_scan::Event::Loaded(result)=>{
                    worker.loading=false;
                    match *result {
                        Ok(Some((p,nodes))) if active && self.settings.remember && p.exclusions==self.exclusions() && self.progress().0.generation==0 => {
                            self.settings.path=display_path(&p.root);
                            self.saved_generation=p.generation;
                            let mut state=self.shared.lock().unwrap();state.progress=p;state.nodes=nodes;
                        }
                        Err(e)=>self.message=Some(e),
                        _=>{}
                    }
                }
                saved_scan::Event::Saved(result)=>match result {
                    Ok(bytes) if bytes>256*1024*1024=>self.message=Some(format!("The saved scan uses {} on this PC. Use exclusions or a smaller root to reduce it. The limit is 384 MiB per root.",size(bytes))),
                    Err(e)=>self.message=Some(e),
                    _=>{}
                },
                saved_scan::Event::Deleted(result)=>self.message=Some(result.map(|_|"Saved scans deleted from this PC.".into()).unwrap_or_else(|e|format!("Could not delete saved scans: {e}. Try again."))),
            }
        }
        let (p, nodes) = self.progress();
        if !p.running && !nodes.is_empty() {
            if let Some((parent, selected)) = self.restore.take() {
                self.parent = nodes
                    .iter()
                    .find(|n| path_key(&n.path) == path_key(&parent))
                    .map_or(0, |n| n.id);
                self.map_state = Default::default();
                self.map_state.selected = selected.and_then(|path| {
                    nodes
                        .iter()
                        .find(|n| path_key(&n.path) == path_key(&path))
                        .map(|n| n.id)
                });
            }
            if self.settings.remember && !p.cancelled && p.generation != self.saved_generation {
                self.saved_generation = p.generation;
                self.cache
                    .as_mut()
                    .unwrap()
                    .send(saved_scan::Command::Save(Box::new(p), nodes));
            }
        }
    }
    fn privacy(&mut self, ui: &mut egui::Ui, p: &Progress) {
        ui.add_space(theme::SPACE_M);
        ui.horizontal_wrapped(|ui| {
            let toggle = toolboks_shared::ui::toggle_switch(ui, &mut self.settings.remember);
            ui.label("Remember my last scan");
            if toggle.changed() {
                if !self.settings.remember {
                    self.settings.remember = true;
                    self.delete_confirm = Some(DeleteScope::Disable);
                } else {
                    self.saved_generation = 0;
                }
            }
            if ui.add(theme::outlined("Delete saved scans")).clicked() {
                self.delete_confirm = Some(DeleteScope::All);
            }
            if ui
                .add_enabled(
                    !p.volume.is_empty(),
                    theme::outlined("Delete this drive's scans"),
                )
                .clicked()
            {
                self.delete_confirm = Some(DeleteScope::Drive);
            }
        });
        theme::caption(ui,"Saved file names stay on this PC only and are never sent anywhere. One scan per root, up to 384 MiB. Turning remembering off deletes saved scans.");
        if self.cache.as_ref().is_some_and(|w| w.loading) {
            weak(ui, "Loading your last scan...");
        } else if self.cache.as_ref().is_some_and(|w| w.pending > 0) {
            theme::caption(ui, "Updating saved scans...");
        }
        if let Some(scope) = self.delete_confirm {
            ui.horizontal_wrapped(|ui| {
                weak(
                    ui,
                    match scope {
                        DeleteScope::Drive => {
                            "Delete saved scans for this drive? Your files stay unchanged."
                        }
                        _ => "Delete all saved scans? Your files stay unchanged.",
                    },
                );
                if ui.button("Delete scans").clicked() {
                    self.delete_saved(scope, p);
                    self.delete_confirm = None;
                }
                if ui.button("Cancel").clicked() {
                    self.delete_confirm = None;
                }
            });
        }
    }
    fn delete_saved(&mut self, scope: DeleteScope, p: &Progress) {
        if matches!(scope, DeleteScope::Disable) {
            self.settings.remember = false;
        }
        self.saved_generation = p.generation;
        if let Some(worker) = &mut self.cache {
            worker.send(saved_scan::Command::Delete(
                if matches!(scope, DeleteScope::Drive) {
                    Some(p.volume.clone())
                } else {
                    None
                },
            ));
        }
    }
    fn progress(&self) -> (Progress, Arc<Vec<Node>>) {
        let s = self.shared.lock().unwrap();
        (s.progress.clone(), s.nodes.clone())
    }
    fn drill(&mut self, id: usize) {
        self.map_state = Default::default();
        self.parent = id;
        self.offset = 0;
        self.search.clear();
    }
    fn map_entries(&mut self, nodes: &[Node], p: &Progress) -> Arc<map::MapLayout> {
        if self
            .map_cache
            .as_ref()
            .is_none_or(|(g, parent, _)| *g != p.generation || *parent != self.parent)
        {
            let mut children = nodes
                .iter()
                .filter(|n| n.parent == Some(self.parent))
                .collect::<Vec<_>>();
            children.sort_by_key(|n| std::cmp::Reverse(n.bytes));
            self.map_cache = Some((
                p.generation,
                self.parent,
                Arc::new(map::MapLayout::with_tree(&children, nodes)),
            ));
        }
        self.map_cache.as_ref().unwrap().2.clone()
    }
    fn export(&mut self, nodes: Arc<Vec<Node>>, progress: Progress, json: bool) {
        let path = PathBuf::from(self.export_path.trim().trim_matches('"'));
        if path.file_name().is_none() {
            self.message = Some("Enter a full report filename.".into());
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.export_job = Some(rx);
        self.export_kind = None;
        std::thread::spawn(move || {
            let result = (|| -> Result<String, String> {
                use std::io::Write;
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .map_err(|e| {
                        format!("Could not create report: {e}. Existing files are not overwritten.")
                    })?;
                let mut out = std::io::BufWriter::new(file);
                if json {
                    #[derive(Serialize)]
                    struct Report<'a> {
                        application: &'static str,
                        summary: &'a Progress,
                        measurement: &'static str,
                        entries: &'a [Node],
                    }
                    let report=Report{application:"Disk Manager by stratum",summary:&progress,measurement:"logical bytes; hard links counted per entry; links and junctions skipped",entries:&nodes};
                    serde_json::to_writer_pretty(&mut out, &report).map_err(|e| e.to_string())?;
                } else {
                    write_csv(&mut out, &nodes, progress.cancelled).map_err(|e| e.to_string())?;
                }
                out.flush().map_err(|e| e.to_string())?;
                Ok(format!("Report saved to {}", path.display()))
            })();
            let _ = tx.send(result);
        });
    }
    fn stats(&self, ui: &mut egui::Ui, p: &Progress) {
        card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                theme::stat(ui, size(p.bytes), "analyzed, logical file sizes");
                theme::stat(ui, p.files.to_string(), "files found");
                theme::stat(ui, p.folders.to_string(), if p.changes.is_some() { "folders checked" } else { "folders read" });
                theme::stat(ui, elapsed(p.elapsed), "scan time");
                weak(
                    ui,
                    if p.running {
                        "Scanning in the background"
                    } else if p.cancelled {
                        "Partial results"
                    } else {
                        "Scan complete"
                    },
                );
            });
            if p.scanned_at>0 {
                weak(ui,format!("Scanned {}. Last full scan {}.",age(p.scanned_at),age(p.full_scanned_at)));
            }
            if let Some(c)=&p.changes {
                weak(ui,format!("Found {} new, {} removed, {} changed in {}. Read {} folders; reused {} folder lists.",c.new,c.removed,c.changed,elapsed(p.elapsed),c.folders_read,c.folders_reused));
                weak(ui,"Sizes of unchanged folders were not rechecked. Files edited in place can change size without changing their folder time. Use Full rescan to refresh every size.");
            }
        });
    }
    fn list(&mut self, ui: &mut egui::Ui, nodes: &[Node], p: &Progress, compact: bool) {
        let mut changed = false;
        let filters = ui.horizontal_wrapped(|ui| {
            changed |= ui
                .add(
                    egui::TextEdit::singleline(&mut self.search)
                        .hint_text("Search scanned paths")
                        .desired_width(210.0_f32.min(ui.available_width())),
                )
                .changed();
            egui::ComboBox::from_id_salt("disk-size")
                .selected_text(match self.settings.min_bytes {
                    0 => "Any size",
                    104857600 => "Over 100 MiB",
                    1073741824 => "Over 1 GiB",
                    _ => "Over 10 GiB",
                })
                .show_ui(ui, |ui| {
                    for (v, l) in [
                        (0, "Any size"),
                        (104857600, "Over 100 MiB"),
                        (1073741824, "Over 1 GiB"),
                        (10737418240, "Over 10 GiB"),
                    ] {
                        changed |= ui
                            .selectable_value(&mut self.settings.min_bytes, v, l)
                            .changed();
                    }
                });
            egui::ComboBox::from_id_salt("disk-age")
                .selected_text(match self.settings.older_days {
                    0 => "Any age",
                    180 => "Over 6 months",
                    365 => "Over 1 year",
                    _ => "Over 3 years",
                })
                .show_ui(ui, |ui| {
                    for (v, l) in [
                        (0, "Any age"),
                        (180, "Over 6 months"),
                        (365, "Over 1 year"),
                        (1095, "Over 3 years"),
                    ] {
                        changed |= ui
                            .selectable_value(&mut self.settings.older_days, v, l)
                            .changed();
                    }
                });
            egui::ComboBox::from_id_salt("disk-sort")
                .selected_text(match self.settings.sort.as_str() {
                    "name" => "Name A to Z",
                    "files" => "Most files",
                    "modified" => "Recently modified",
                    _ => "Largest first",
                })
                .show_ui(ui, |ui| {
                    for (v, l) in [
                        ("size", "Largest first"),
                        ("name", "Name A to Z"),
                        ("files", "Most files"),
                        ("modified", "Recently modified"),
                    ] {
                        changed |= ui
                            .selectable_value(&mut self.settings.sort, v.into(), l)
                            .changed();
                    }
                });
        });
        guide::mark(ui, "disk-filters", filters.response.rect);
        if changed {
            self.offset = 0;
        }
        let q = Query {
            parent: Some(self.parent),
            mode: Some(if self.page == 1 { "files" } else { "folders" }.into()),
            search: Some(self.search.clone()),
            sort: Some(self.settings.sort.clone()),
            offset: Some(self.offset),
            limit: Some(100),
            min_bytes: Some(self.settings.min_bytes),
            older_days: Some(self.settings.older_days),
        };
        let key = format!("{}:{}", p.generation, serde_json::to_string(&q).unwrap());
        if self
            .listing_cache
            .as_ref()
            .is_none_or(|(old, _)| old != &key)
        {
            self.listing_cache = Some((key, scanner::listing(nodes, &q)));
        }
        let listing = self.listing_cache.as_ref().unwrap().1.clone();
        ui.add_space(10.0);
        if listing.rows.is_empty() {
            weak(
                ui,
                "No entries to show. Try a different folder or change the filters.",
            );
            return;
        }
        let mut navigate = None;
        let painter = ui.painter().with_clip_rect(ui.available_rect_before_wrap());
        let mut table = egui_extras::TableBuilder::new(ui)
            .id_salt("disk-entries")
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(egui_extras::Column::remainder().at_least(60.0))
            .column(egui_extras::Column::exact(76.0))
            .min_scrolled_height(100.0)
            .max_scroll_height(340.0);
        if !compact {
            table = table.columns(egui_extras::Column::exact(65.0), 2);
        }
        table
            .header(24.0, |mut h| {
                h.col(|ui| theme::header_cell(ui, "Name"));
                let labels = if compact {
                    &["Size"][..]
                } else {
                    &["Size", "Of scan", "Files"][..]
                };
                for label in labels {
                    h.col(|ui| theme::number_cell(ui, |ui| theme::header_cell(ui, label)));
                }
            })
            .body(|body| {
                body.rows(29.0, listing.rows.len(), |mut row| {
                    let n = &listing.rows[row.index()];
                    row.col(|ui| {
                        let label = if n.dir {
                            format!("{} (folder)", n.name)
                        } else {
                            n.name.clone()
                        };
                        let r = ui.add(egui::Button::new(label).truncate().frame(false));
                        r.clone().on_hover_text(format!(
                            "{}\n{} · {} file(s) · {:.1}% of scan",
                            display_path(&n.path),
                            size(n.bytes),
                            n.files,
                            n.bytes as f64 / p.bytes.max(1) as f64 * 100.0,
                        ));
                        if n.dir && r.clicked() {
                            navigate = Some(n.id);
                        }
                        r.context_menu(|ui| {
                            if ui.button("Copy path").clicked() {
                                ui.ctx().copy_text(display_path(&n.path));
                                ui.close();
                            }
                            if ui.button("Show in Explorer").clicked() {
                                self.message = Some(explorer::show(&n.path).unwrap_or_else(|e| e));
                                ui.close();
                            }
                        });
                    });
                    row.col(|ui| {
                        theme::number_cell(ui, |ui| {
                            ui.label(size(n.bytes));
                        })
                    });
                    if !compact {
                        row.col(|ui| {
                            theme::number_cell(ui, |ui| {
                                weak(
                                    ui,
                                    format!(
                                        "{:.1}%",
                                        n.bytes as f64 / p.bytes.max(1) as f64 * 100.0
                                    ),
                                )
                            })
                        });
                        row.col(|ui| theme::number_cell(ui, |ui| weak(ui, n.files.to_string())));
                    }
                    let rect = row.response().rect;
                    painter.hline(
                        rect.x_range(),
                        rect.bottom(),
                        egui::Stroke::new(1.0, theme::HAIRLINE),
                    );
                });
            });
        if let Some(id) = navigate {
            self.drill(id);
        }
        ui.add_space(7.0);
        ui.horizontal_wrapped(|ui| {
            weak(
                ui,
                format!(
                    "{}–{} of {} entries",
                    self.offset + 1,
                    self.offset + listing.rows.len(),
                    listing.total
                ),
            );
            if listing.total <= 100 {
                return;
            }
            if ui
                .add_enabled(
                    self.offset + listing.rows.len() < listing.total,
                    theme::outlined("Next"),
                )
                .clicked()
            {
                self.offset += 100;
            }
            if ui
                .add_enabled(self.offset > 0, theme::outlined("Previous"))
                .clicked()
            {
                self.offset = self.offset.saturating_sub(100);
            }
        });
    }

    fn overview(&mut self, ui: &mut egui::Ui, nodes: &[Node], p: &Progress) {
        let mut path = Vec::new();
        let mut current = nodes.get(self.parent);
        while let Some(node) = current {
            path.push(node);
            current = node.parent.and_then(|id| nodes.get(id));
        }
        let crumbs = egui::ScrollArea::horizontal()
            .id_salt("disk-breadcrumb")
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (index, node) in path.iter().rev().enumerate() {
                        if index > 0 {
                            weak(ui, "/");
                        }
                        if node.id == self.parent {
                            ui.label(RichText::new(&node.name).strong())
                                .on_hover_text(display_path(&node.path));
                        } else if ui
                            .small_button(&node.name)
                            .on_hover_text(display_path(&node.path))
                            .clicked()
                        {
                            self.drill(node.id);
                        }
                    }
                });
            });
        guide::mark(ui, "disk-breadcrumbs", crumbs.inner_rect);
        ui.add_space(theme::SPACE_M);
        let width = ui.available_width();
        if width >= 680.0 {
            ui.scope(|ui| {
                ui.spacing_mut().item_spacing.x = theme::SPACE_M;
                ui.horizontal_top(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width(width - 278.0);
                        self.space_map(ui, nodes, p);
                    });
                    ui.vertical(|ui| {
                        ui.set_width(270.0);
                        self.folder_list(ui, nodes, p, true);
                    });
                });
            });
        } else {
            self.space_map(ui, nodes, p);
            ui.add_space(theme::SPACE_M);
            self.folder_list(ui, nodes, p, width < 360.0);
        }
    }

    fn folder_list(&mut self, ui: &mut egui::Ui, nodes: &[Node], p: &Progress, compact: bool) {
        guide::region(ui, "disk-list", |ui| {
            card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                theme::section(ui, "Folders and files");
                ui.add_space(theme::SPACE_M);
                self.list(ui, nodes, p, compact);
            });
        });
    }

    fn space_map(&mut self, ui: &mut egui::Ui, nodes: &[Node], p: &Progress) {
        let map = card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::section(ui, "Space map");
            ui.add_space(theme::SPACE_M);
            let entries = self.map_entries(nodes, p);
            let action = map::show(ui, &entries, &mut self.map_state);
            if let Some(n) = action.reveal.and_then(|id| nodes.get(id)) {
                self.message = Some(explorer::show(&n.path).unwrap_or_else(|e| e));
            }
            if let Some(n) = action.open.and_then(|id| nodes.get(id)) {
                if n.dir {
                    self.drill(n.id);
                } else {
                    self.message = Some(format!("{} · {}", display_path(&n.path), size(n.bytes)));
                }
            }
        });
        guide::mark(ui, "disk-map", map.response.rect);
    }
}

impl NativeAddon for DiskManager {
    fn load_preferences(&mut self, prefs: &BTreeMap<String, serde_json::Value>) {
        if let Some(value) = prefs.get("settings") {
            self.settings = serde_json::from_value(value.clone()).unwrap_or_default();
        }
        self.loaded = true;
    }
    fn save_preferences(&mut self, prefs: &mut BTreeMap<String, serde_json::Value>) -> bool {
        if !self.loaded {
            return false;
        }
        let value = serde_json::to_value(&self.settings).unwrap();
        let changed = prefs.get("settings") != Some(&value);
        if changed {
            prefs.insert("settings".into(), value);
        }
        self.optimization.save_preferences(prefs) || changed
    }
    fn suspend(&mut self) {
        if self.cache_started {
            self.cache_tick(false);
        }
        // Keep export/analysis workers, but cancel the scan and drop its large results.
        // The old worker owns the old state until it observes cancellation.
        self.cancel.store(true, Ordering::Relaxed);
        self.cancel = Arc::new(AtomicBool::new(false));
        self.shared = Default::default();
        self.listing_cache = None;
        self.map_cache = None;
        self.last_generation = 0;
        self.saved_generation = 0;
        self.reload_cache = true;
        self.restore = None;
        self.parent = 0;
        self.offset = 0;
    }
    fn has_background_work(&self) -> bool {
        // Always: the runtime has no timer, so the six-hour drive-health read is started from
        // its 200 ms background tick, which only does a lock and a clock comparison.
        true
    }
    fn findings(&self) -> Vec<toolboks_addon_sdk::Finding> {
        self.reporter.findings()
    }
    fn check_now(&mut self) {
        self.reporter.check_now();
    }
    fn tick_background(&mut self) {
        self.reporter.poll();
        self.poll_workers();
        if self.cache_started {
            self.cache_tick(false);
        }
    }
    fn set_capabilities(&mut self, capabilities: &[toolboks_addon_sdk::Capability]) {
        self.optimization.capabilities(capabilities);
    }
    fn prepare_admin(
        &mut self,
        request: &toolboks_addon_sdk::AdminRequest,
    ) -> Result<Box<dyn toolboks_addon_sdk::admin::AdminChange>, String> {
        optimization::prepare_admin(request)
    }
    fn admin_result(&mut self, id: u64, outcome: toolboks_addon_sdk::AdminOutcome) {
        self.optimization.admin_result(id, outcome);
    }
    fn take_host_request(&mut self) -> Option<toolboks_addon_sdk::HostRequest> {
        self.optimization.request()
    }
    fn navigate(&mut self, route: &str) {
        let (page, drive) = parse_route(route);
        if page != self.page {
            self.export_kind = None;
            self.page = page;
            self.offset = 0;
        }
        if let Some(letter) = drive {
            self.optimization.select(&letter);
        }
    }
    fn page_index(&self) -> usize {
        self.page
    }
    fn tutorial_steps(&self) -> Vec<toolboks_addon_sdk::TutorialStep> {
        tutorial::steps()
    }
    fn tutorial_location(&self) -> String {
        format!("{}/{}", self.page, self.options)
    }
    fn tutorial_start(&mut self) {
        self.optimization.tutorial_start();
        self.tutorial_snapshot = Some((self.page, self.options, self.parent, self.offset));
        self.parent = 0;
        self.offset = 0;
        self.map_cache = None;
        self.listing_cache = None;
    }
    fn tutorial_navigate(&mut self, route: &str) {
        let mut parts = route.split('/');
        self.page = parts
            .next()
            .and_then(|p| p.parse::<usize>().ok())
            .unwrap_or(0)
            .min(4);
        self.options = matches!(parts.next(), Some("options" | "true"));
        self.offset = 0;
    }
    fn tutorial_finish(&mut self) {
        self.optimization.tutorial_finish();
        if let Some((page, options, parent, offset)) = self.tutorial_snapshot.take() {
            self.page = page;
            self.options = options;
            self.parent = parent;
            self.offset = offset;
        }
        self.map_cache = None;
        self.listing_cache = None;
    }
    fn ui(&mut self, ui: &mut egui::Ui, prefs: &mut BTreeMap<String, serde_json::Value>) -> bool {
        if !self.styled {
            toolboks_shared::ui::apply_style(ui.ctx());
            self.styled = true;
        }
        if !self.loaded {
            if let Some(v) = prefs.get("settings") {
                self.settings = serde_json::from_value(v.clone()).unwrap_or_default();
            }
            self.loaded = true;
        }
        self.reporter.poll();
        self.poll_workers();
        self.cache_tick(true);
        self.tick(ui.ctx());
        let (p, nodes) = self.progress();
        if p.generation != self.last_generation {
            self.last_generation = p.generation;
            self.map_state = Default::default();
            self.parent = 0;
            self.offset = 0;
        }
        let sample = self.tutorial_snapshot.is_some() && (p.generation == 0 || p.running);
        let (p, nodes) = if sample {
            tutorial::sample()
        } else {
            (p, nodes)
        };
        if sample {
            weak(
                ui,
                "Tutorial sample. Illustrative data, not a scan of your files.",
            );
            ui.add_space(theme::SPACE_M);
        }
        if self.page == 4 {
            return egui::ScrollArea::vertical()
                .id_salt("disk-optimization")
                .show(ui, |ui| self.optimization.ui(ui, prefs))
                .inner;
        }
        let heading = ui.horizontal_wrapped(|ui| {
            theme::title(
                ui,
                if self.page == 0 {
                    "Storage overview"
                } else {
                    PAGES[self.page]
                },
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let export = ui.add_enabled(
                    !p.running && !nodes.is_empty() && self.export_job.is_none(),
                    theme::outlined("Export report"),
                );
                guide::mark(ui, "disk-export", export.rect);
                if export.clicked() {
                    self.export_kind = Some(false);
                    self.export_path = std::env::var("USERPROFILE")
                        .map(|p| {
                            PathBuf::from(p)
                                .join("Documents/Disk Manager Report.csv")
                                .display()
                                .to_string()
                        })
                        .unwrap_or("Disk Manager Report.csv".into());
                }
            });
        });
        guide::mark(ui, "disk-workspace-title", heading.response.rect);
        ui.add_space(theme::SPACE_L);
        if let Some(message) = self.message.clone() {
            if theme::banner(ui, &message) {
                self.message = None;
            }
            ui.add_space(theme::SPACE_M);
        }
        if let Some(mut json) = self.export_kind {
            card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                theme::section(ui, "Export scan");
                weak(ui, "Save every entry in the scan. Choose a new filename.");
                ui.horizontal_wrapped(|ui| {
                    let format_changed = ui.selectable_value(&mut json, false, "CSV").changed()
                        | ui.selectable_value(&mut json, true, "JSON").changed();
                    if format_changed {
                        self.export_path = PathBuf::from(&self.export_path)
                            .with_extension(if json { "json" } else { "csv" })
                            .display()
                            .to_string();
                        self.export_kind = Some(json);
                    }
                });
                ui.add(
                    egui::TextEdit::singleline(&mut self.export_path)
                        .hint_text("Report filename")
                        .desired_width(ui.available_width()),
                );
                ui.add_space(theme::SPACE_M);
                ui.horizontal_wrapped(|ui| {
                    if button(ui, "Save report").clicked() {
                        self.export(nodes.clone(), p.clone(), json);
                    }
                    if ui.add(theme::outlined("Cancel")).clicked() {
                        self.export_kind = None;
                    }
                });
            });
            ui.add_space(theme::SPACE_M);
        }
        if self.page == 0 {
            ui.horizontal_wrapped(|ui| {
                let drive = egui::ComboBox::from_id_salt("disk-drive")
                    .width(85.0)
                    .selected_text("Drives")
                    .show_ui(ui, |ui| {
                        for d in &self.drives {
                            if ui
                                .selectable_label(
                                    false,
                                    format!(
                                        "{} · {} free of {}",
                                        d.path,
                                        size(d.free),
                                        size(d.total)
                                    ),
                                )
                                .clicked()
                            {
                                self.settings.path = d.path.clone();
                            }
                        }
                    });
                guide::mark(ui, "disk-drives", drive.response.rect);
                let response = ui.add_enabled(
                    !p.running && self.picker.is_none(),
                    egui::TextEdit::singleline(&mut self.settings.path)
                        .hint_text("Drive or folder path")
                        .desired_width((ui.available_width() - 322.0).clamp(115.0, 460.0)),
                );
                guide::mark(ui, "disk-path", response.rect);
                let enter = !guide::active(ui.ctx())
                    && response.lost_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let browse = ui.add_enabled(
                    !p.running && self.picker.is_none(),
                    theme::outlined(if self.picker.is_some() {
                        "Choosing..."
                    } else {
                        "Browse to folder"
                    }),
                );
                guide::mark(ui, "disk-browse", browse.rect);
                if browse.clicked() {
                    self.picker = Some(folder_picker::start(
                        self.settings.path.clone(),
                        ui.ctx().clone(),
                    ));
                }
                if p.running {
                    let stop = button(ui, "Stop scan");
                    guide::mark(ui, "disk-analyze", stop.rect);
                    if stop.clicked() {
                        self.cancel.store(true, Ordering::Relaxed);
                    }
                } else {
                    let quick = self.quick_available(&p, &nodes);
                    let label = if quick {
                        "Rescan (quick)"
                    } else if nodes.is_empty() {
                        "Scan"
                    } else {
                        "Full rescan"
                    };
                    let analyze = ui
                        .add_enabled_ui(
                            self.picker.is_none()
                                && self.export_kind.is_none()
                                && !self.cache.as_ref().is_some_and(|w| w.loading),
                            |ui| button(ui, label),
                        )
                        .inner;
                    guide::mark(ui, "disk-analyze", analyze.rect);
                    if analyze.clicked() || (enter && analyze.enabled()) {
                        self.start(quick);
                    }
                    if quick && ui.add(theme::outlined("Full rescan")).clicked() {
                        self.start(false);
                    }
                }
                if ui.add(theme::outlined("Options")).clicked() {
                    self.options = !self.options;
                }
            });
            self.privacy(ui, &p);
            if !nodes.is_empty() && p.exclusions != self.exclusions() {
                weak(ui, "Exclusions changed. A full rescan is required.");
            }
            if self.options {
                ui.add_space(theme::SPACE_M);
                let options = card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                theme::section(ui, "Skip folders by name");
                weak(ui, "Separate names with semicolons.");
                ui.add_enabled(
                    !p.running,
                    egui::TextEdit::singleline(&mut self.settings.exclude)
                        .hint_text("For example: node_modules; .git; target")
                        .desired_width(ui.available_width()),
                );
                weak(
                    ui,
                    "Names match exactly, ignoring case. Leave empty to scan everything you can read.",
                );
            });
                guide::mark(ui, "disk-options", options.response.rect);
            }
        }
        ui.add_space(theme::SPACE_XL);
        egui::ScrollArea::vertical()
            .id_salt(("disk-workspace", self.page))
            .show(ui, |ui| {
                if p.generation == 0 {
                    card().show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.set_min_height(120.0);
                        theme::section(ui, "No scan yet");
                        weak(
                            ui,
                            if self.page == 0 {
                                "Choose a drive or folder, then select Scan."
                            } else {
                                "Start a scan from Overview to see results here."
                            },
                        );
                        ui.add_space(theme::SPACE_M);
                        theme::caption(ui, "Scanning only reads. Your files are never changed.");
                    });
                    return;
                }
                guide::region(ui, "disk-stats", |ui| self.stats(ui, &p));
                ui.add_space(theme::SPACE_L);
                if p.running {
                    card().show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        theme::section(ui, "Reading folders...");
                        ui.add(
                            egui::Label::new(
                                RichText::new(display_path(&p.current))
                                    .size(11.0)
                                    .color(MUTED),
                            )
                            .truncate(),
                        );
                        weak(
                            ui,
                            format!(
                                "{} entries skipped. Stop at any time to explore partial results.",
                                p.skipped
                            ),
                        );
                    });
                    return;
                }
                if p.cancelled {
                    weak(
                        ui,
                        "Partial scan. Totals cover only what was found before you stopped.",
                    );
                    ui.add_space(theme::SPACE_M);
                }
                match self.page {
                    2 => {
                        let types = card().show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            weak(ui, "Share of all file bytes found, by extension.");
                            ui.add_space(theme::SPACE_M);
                            if p.extensions.is_empty() {
                                weak(ui, "No files were found in this scan.");
                            }
                            for e in &p.extensions {
                                let color = theme::DATA[3];
                                ui.horizontal(|ui| {
                                    ui.label(&e.name);
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            ui.label(size(e.bytes));
                                            weak(ui, format!("{} files · {:.1}%", e.files, e.bytes as f64 / p.bytes.max(1) as f64 * 100.0));
                                        },
                                    );
                                });
                                ui.add(
                                    egui::ProgressBar::new(e.bytes as f32 / p.bytes.max(1) as f32)
                                        .fill(color)
                                        .desired_width(ui.available_width()),
                                );
                                ui.add_space(theme::SPACE_M);
                            }
                        });
                        guide::mark(ui, "disk-types", types.response.rect);
                    }
                    3 => {
                        let details = card().show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            theme::section(ui, "Scan coverage");
                            weak(
                                ui,
                                format!(
                                    "{} entries skipped (links, junctions, exclusions or access errors).",
                                    p.skipped
                                ),
                            );
                            weak(ui, "Sizes are logical file lengths. Hard links count once per directory entry. Sparse or cloud placeholder files can differ from the space they use on disk. Files may change while scanning.");
                            ui.add_space(theme::SPACE_L);
                            theme::section(ui, "Access and read errors (first 100)");
                            if p.errors.is_empty() {
                                weak(ui, "No errors recorded.");
                            }
                            for error in &p.errors {
                                theme::caption(ui, error.as_str());
                            }
                        });
                        guide::mark(ui, "disk-details", details.response.rect);
                    }
                    0 => self.overview(ui, &nodes, &p),
                    _ => {
                        guide::region(ui, "disk-list", |ui| self.list(ui, &nodes, &p, ui.available_width() < 360.0));
                    }
                }
                ui.add_space(theme::SPACE_M);
                theme::caption(
                    ui,
                    "Logical sizes. Links and junctions are left out. Read-only analysis.",
                );
            });
        self.tick(ui.ctx());
        let value = serde_json::to_value(&self.settings).unwrap();
        if prefs.get("settings") != Some(&value) {
            prefs.insert("settings".into(), value);
            true
        } else {
            false
        }
    }
    fn tick(&mut self, ctx: &egui::Context) {
        if self.shared.lock().unwrap().progress.running
            || self.export_job.is_some()
            || self.picker.is_some()
            || self.optimization.busy()
            || self.cache.as_ref().is_some_and(|w| w.pending > 0)
        {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }
    fn cancel(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// The page and drive a route names. The sidebar sends the page number ("4"); Home sends the
/// route of a finding ("Optimization/C:"). Anything unknown opens the first page.
fn parse_route(route: &str) -> (usize, Option<String>) {
    let (head, tail) = match route.split_once('/') {
        Some((head, tail)) => (head.trim(), Some(tail)),
        None => (route.trim(), None),
    };
    let page = head
        .parse::<usize>()
        .ok()
        .or_else(|| {
            PAGES
                .iter()
                .position(|name| name.eq_ignore_ascii_case(head))
        })
        .unwrap_or(0)
        .min(PAGES.len() - 1);
    let drive = tail
        .filter(|_| PAGES[page] == "Optimization")
        .and_then(health_findings::normalize_letter);
    (page, drive)
}

fn drives() -> Vec<Drive> {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        extern "system" {
            fn GetLogicalDrives() -> u32;
            fn GetDiskFreeSpaceExW(
                path: *const u16,
                available: *mut u64,
                total: *mut u64,
                free: *mut u64,
            ) -> i32;
        }
        let mut out = vec![];
        let mask = unsafe { GetLogicalDrives() };
        for i in 0..26 {
            if mask & (1 << i) == 0 {
                continue;
            }
            let path = format!("{}:\\", (b'A' + i) as char);
            let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
            let (mut available, mut total, mut free) = (0, 0, 0);
            if unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, &mut total, &mut free) }
                != 0
            {
                out.push(Drive { path, total, free });
            }
        }
        out
    }
    #[cfg(not(windows))]
    {
        vec![]
    }
}
fn csv_cell(value: &str) -> String {
    let protected = if value.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("'{value}")
    } else {
        value.into()
    };
    format!("\"{}\"", protected.replace('"', "\"\""))
}
fn write_csv(out: &mut impl std::io::Write, nodes: &[Node], partial: bool) -> std::io::Result<()> {
    writeln!(
        out,
        "Path,Kind,Logical bytes,Files,Modified Unix seconds,Partial scan"
    )?;
    for n in nodes {
        writeln!(
            out,
            "{},{},{},{},{},{}",
            csv_cell(&display_path(&n.path)),
            if n.dir { "Folder" } else { "File" },
            n.bytes,
            n.files,
            n.modified,
            partial
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn suspending_releases_scan_state_and_keeps_export_worker_and_preferences() {
        let mut addon = DiskManager::default();
        let prefs = BTreeMap::from([(
            "settings".into(),
            serde_json::json!({
                "path":"C:\\saved", "exclude":"target", "min_bytes":1234, "older_days":9, "sort":"name", "remember":true
            }),
        )]);
        addon.load_preferences(&prefs);
        let scan_cancel = addon.cancel.clone();
        let scan_state = addon.shared.clone();
        scan_state.lock().unwrap().progress.generation = 3;
        let (tx, rx) = mpsc::channel();
        addon.export_job = Some(rx);
        addon.suspend();
        assert!(scan_cancel.load(Ordering::Relaxed));
        assert!(!Arc::ptr_eq(&scan_state, &addon.shared));
        assert_eq!(addon.progress().0.generation, 0);
        assert!(addon.has_user_work());
        tx.send(Ok("Export finished".into())).unwrap();
        addon.tick_background();
        assert_eq!(addon.message.as_deref(), Some("Export finished"));
        assert!(!addon.has_user_work());
        assert!(
            addon.has_background_work(),
            "the drive-health timer keeps ticking"
        );
        let mut saved = BTreeMap::new();
        assert!(addon.save_preferences(&mut saved));
        assert_eq!(saved["settings"], prefs["settings"]);
    }
    #[test]
    fn pending_cache_work_requests_a_frame_from_inside_ui() {
        let root = scanner::tests::temp("cache-repaint");
        let mut worker = saved_scan::Worker::at(root.clone());
        worker.pending = 1;
        worker.loading = true;
        let mut addon = DiskManager::default();
        addon.loaded = true;
        addon.cache_started = true;
        addon.cache = Some(worker);
        let ctx = egui::Context::default();
        let mut prefs = BTreeMap::new();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            addon.ui(ui, &mut prefs);
        });
        assert!(
            output.viewport_output[&egui::ViewportId::ROOT].repaint_delay
                <= Duration::from_millis(200)
        );
        output.textures_delta.clear();
    }
    #[test]
    fn scan_button_starts_from_pointer_input() {
        let root = scanner::tests::temp("scan-button");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("file"), b"data").unwrap();
        let mut addon = DiskManager::default();
        addon.loaded = true;
        addon.cache_started = true;
        addon.cache = Some(saved_scan::Worker::at(root.join("cache")));
        addon.settings.path = root.to_string_lossy().into_owned();
        addon.settings.remember = false;
        let ctx = egui::Context::default();
        let mut prefs = BTreeMap::new();
        let frame = |addon: &mut DiskManager,
                     prefs: &mut BTreeMap<String, serde_json::Value>,
                     events: Vec<egui::Event>| {
            let mut out = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1280.0, 900.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ui| {
                    guide::prepare(ui.ctx(), Some("disk-analyze"), false);
                    addon.ui(ui, prefs);
                },
            );
            out.textures_delta.clear();
        };
        for _ in 0..3 {
            frame(&mut addon, &mut prefs, vec![]);
        }
        let pos = guide::anchor(&ctx).unwrap().center();
        for pressed in [true, false] {
            frame(
                &mut addon,
                &mut prefs,
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
        }
        assert!(
            addon.progress().0.generation > 0,
            "button did not start; message={:?}",
            addon.message
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while addon.progress().0.running {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(addon.progress().0.files, 1);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn remembering_off_deletes_on_disk_and_exclusions_disable_quick_rescan() {
        let root = scanner::tests::temp("privacy");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("file"), b"data").unwrap();
        let shared = Arc::new(Mutex::new(State::default()));
        scanner::scan(
            root.clone(),
            vec![],
            shared.clone(),
            Arc::new(AtomicBool::new(false)),
        );
        let (p, nodes) = {
            let state = shared.lock().unwrap();
            (state.progress.clone(), state.nodes.clone())
        };
        let dir = root.join("cache");
        saved_scan::save(&dir, &p, &nodes).unwrap();
        let mut addon = DiskManager::default();
        addon.cache = Some(saved_scan::Worker::at(dir.clone()));
        addon.cache_started = true;
        addon.shared = shared;
        addon.settings.path = root.to_string_lossy().into_owned();
        assert!(addon.quick_available(&p, &nodes));
        addon.settings.exclude = "skip".into();
        assert!(!addon.quick_available(&p, &nodes));
        addon.delete_saved(DeleteScope::Disable, &p);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while addon.cache.as_ref().unwrap().pending > 0 {
            assert!(std::time::Instant::now() < deadline);
            addon.cache_tick(false);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!addon.settings.remember);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn rescan_restores_parent_selection_and_pagination_by_path() {
        let root = scanner::tests::temp("selection");
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::write(root.join("nested/selected"), b"data").unwrap();
        let shared = Arc::new(Mutex::new(State::default()));
        scanner::scan(
            root.clone(),
            vec![],
            shared.clone(),
            Arc::new(AtomicBool::new(false)),
        );
        let (p, nodes) = {
            let state = shared.lock().unwrap();
            (state.progress.clone(), state.nodes.clone())
        };
        let parent = nodes
            .iter()
            .find(|n| n.name == "nested")
            .unwrap()
            .path
            .clone();
        let selected = nodes
            .iter()
            .find(|n| n.name == "selected")
            .unwrap()
            .path
            .clone();
        std::fs::write(root.join("nested/new"), b"new").unwrap();
        scanner::rescan(nodes, p, shared.clone(), Arc::new(AtomicBool::new(false)));
        let mut addon = DiskManager::default();
        addon.shared = shared;
        addon.cache = Some(saved_scan::Worker::at(root.join("cache")));
        addon.cache_started = true;
        addon.restore = Some((parent.clone(), Some(selected.clone())));
        addon.offset = 100;
        addon.settings.remember = false;
        addon.cache_tick(false);
        let (_, nodes) = addon.progress();
        assert_eq!(nodes[addon.parent].path, parent);
        assert_eq!(nodes[addon.map_state.selected.unwrap()].path, selected);
        assert_eq!(addon.offset, 100);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[ignore = "700,000-node streaming/load responsiveness measurement"]
    fn large_saved_scan_loads_while_frames_continue() {
        use std::time::Instant;
        let root = scanner::tests::temp("large-snapshot");
        std::fs::create_dir_all(&root).unwrap();
        let canonical = std::fs::canonicalize(&root).unwrap();
        let root_path = canonical.to_string_lossy().into_owned();
        let mut nodes = Vec::with_capacity(700101);
        nodes.push(Node {
            id: 0,
            parent: None,
            name: "Synthetic 700k fixture".into(),
            path: root_path.clone(),
            dir: true,
            bytes: 700000,
            files: 700000,
            modified: 0,
            dir_stamp: 1,
        });
        for folder in 0..100 {
            let name = format!("folder-{folder}");
            nodes.push(Node {
                id: nodes.len(),
                parent: Some(0),
                path: canonical.join(&name).to_string_lossy().into_owned(),
                name,
                dir: true,
                bytes: 7000,
                files: 7000,
                modified: 0,
                dir_stamp: 1,
            });
        }
        for file in 0..700000 {
            let parent = 1 + file / 7000;
            let name = format!("file-{file}.bin");
            nodes.push(Node {
                id: nodes.len(),
                parent: Some(parent),
                path: PathBuf::from(&nodes[parent].path)
                    .join(&name)
                    .to_string_lossy()
                    .into_owned(),
                name,
                dir: false,
                bytes: 1,
                files: 1,
                modified: 0,
                dir_stamp: 0,
            });
        }
        let p = Progress {
            root: root_path,
            volume: saved_scan::volume_id(&canonical).unwrap(),
            generation: 1,
            scanned_at: scanner::now(),
            full_scanned_at: scanner::now(),
            files: 700000,
            folders: 101,
            bytes: 700000,
            ..Default::default()
        };
        let dir = root.join("cache");
        let bytes = saved_scan::save(&dir, &p, &nodes).unwrap();
        drop(nodes);
        let mut worker = saved_scan::Worker::at(dir);
        worker.send(saved_scan::Command::Load(vec![]));
        let mut addon = DiskManager::default();
        addon.cache = Some(worker);
        addon.cache_started = true;
        addon.loaded = true;
        let ctx = egui::Context::default();
        let mut prefs = BTreeMap::new();
        let started = Instant::now();
        let mut frames = 0;
        let mut max_frame = Duration::ZERO;
        while addon.progress().0.generation == 0 {
            assert!(started.elapsed() < Duration::from_secs(45));
            let begin = Instant::now();
            let mut out = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1280.0, 900.0),
                    )),
                    ..Default::default()
                },
                |ui| {
                    addon.ui(ui, &mut prefs);
                },
            );
            out.textures_delta.clear();
            max_frame = max_frame.max(begin.elapsed());
            frames += 1;
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(frames > 2, "loading should yield frames");
        assert_eq!(addon.progress().1.len(), 700101);
        println!("LARGE_SNAPSHOT nodes=700101 files=700000 bytes={} load={:.3}s frames={} longest_frame={:.3}s",bytes,started.elapsed().as_secs_f64(),frames,max_frame.as_secs_f64());
        drop(addon);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn csv_protects_formulas_and_quotes() {
        assert_eq!(csv_cell("=1+1"), "\"'=1+1\"");
        assert_eq!(csv_cell("a\"b"), "\"a\"\"b\"");
    }
    #[test]
    fn pretty_paths_and_sizes() {
        assert_eq!(display_path(r"\\?\C:\data"), r"C:\data");
        assert_eq!(size(1024), "1.00 KiB");
    }
    #[test]
    fn official_package_validates() {
        let p = toolboks_addon_sdk::Package::parse(PACKAGE.as_bytes(), "0.15.0").unwrap();
        assert_eq!(p.manifest.native_provider.as_deref(), Some(PROVIDER));
        // Shown in the install review; it names what the saved scans hold.
        assert_eq!(
            p.manifest.data_note.as_deref(),
            Some("Keeps your last scan of each drive on this PC (file and folder names).")
        );
    }
    #[test]
    fn routes_from_the_sidebar_and_from_home() {
        assert_eq!(parse_route("0"), (0, None));
        assert_eq!(parse_route("4"), (4, None));
        assert_eq!(parse_route("99"), (4, None));
        assert_eq!(parse_route("Optimization"), (4, None));
        assert_eq!(parse_route("optimization/C:"), (4, Some("C:".into())));
        assert_eq!(parse_route("Optimization/d"), (4, Some("D:".into())));
        assert_eq!(parse_route("Optimization/C:\\x"), (4, None));
        assert_eq!(parse_route("Largest files"), (1, None));
        assert_eq!(
            parse_route("Largest files/C:"),
            (1, None),
            "a drive only on Optimization"
        );
        assert_eq!(parse_route("nonsense"), (0, None));
        assert_eq!(parse_route(""), (0, None));
    }
    #[test]
    fn home_opens_the_drive_it_names() {
        let mut addon = DiskManager::default();
        addon.navigate("Optimization/E:");
        assert_eq!(addon.page_index(), 4);
        assert_eq!(addon.optimization.wanted_for_test().as_deref(), Some("E:"));
        addon.navigate("2");
        assert_eq!(addon.page_index(), 2);
    }
    #[test]
    fn it_reports_drive_health_to_home() {
        let addon = DiskManager::default();
        assert!(
            addon.findings().is_empty(),
            "nothing before the first listing"
        );
        let p = toolboks_addon_sdk::Package::parse(PACKAGE.as_bytes(), "0.15.0").unwrap();
        assert_eq!(p.manifest.checks, ["Drive health"]);
        assert_eq!(p.manifest.min_toolboks_version, "0.15.0");
    }
}
