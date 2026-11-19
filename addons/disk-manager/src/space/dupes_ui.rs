//! The Duplicates panel of the Space explorer (issue #187): the right panel and the map colours
//! while "Duplicates" is on, the search and delete workers, and the delete confirmation.
use super::dupes::{self, Counters, DeleteCounters, Found, KeepRule, Plan, Report, SortBy};
use super::layout::{Layout, TileKind};
use super::model::Model;
use super::{
    aside_card, count, date, type_color, Command, Explorer, Sel, ACCENT_TEXT, DANGER_BORDER,
    DANGER_TEXT, SELECTED_BG,
};
use crate::category::Category;
use crate::scanner::Kind;
use crate::theme::{self, ACCENT, HAIRLINE, MUTED, TEXT};
use egui::{Color32, RichText, Sense, Vec2};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
};

/// Tiles without a copy elsewhere: a dark neutral, so the coloured groups stand out.
pub(super) const UNIQUE: Color32 = Color32::from_rgb(0x2a, 0x2c, 0x31);
/// Group colours on the map, cycling: the file type colours, ordered so neighbours differ.
const PALETTE: [Category; 10] = [
    Category::Video,
    Category::Images,
    Category::Apps,
    Category::Documents,
    Category::Games,
    Category::Audio,
    Category::Archives,
    Category::Code,
    Category::System,
    Category::Other,
];
pub(super) fn group_color(id: u32) -> Color32 {
    type_color(PALETTE[id as usize % PALETTE.len()])
}
/// Groups drawn at once; "Show more" adds this many.
const PAGE: usize = 100;

struct SearchJob {
    cancel: Arc<AtomicBool>,
    counters: Arc<Counters>,
    rx: mpsc::Receiver<Found>,
    generation: u64,
    min_bytes: u64,
}
impl Drop for SearchJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}
struct DeleteJob {
    cancel: Arc<AtomicBool>,
    counters: Arc<DeleteCounters>,
    rx: mpsc::Receiver<Report>,
    files: u64,
}
impl Drop for DeleteJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Everything the Duplicates panel holds. Results are in memory only.
pub(super) struct Dupes {
    pub(super) min_bytes: u64,
    /// Threads a search reads with: `None` is Auto (picked from the drive kind).
    pub(super) threads: Option<usize>,
    /// The kind of the scanned drive, looked up once per scanned folder.
    drive: std::cell::RefCell<Option<(String, Kind)>>,
    sort: SortBy,
    rule: KeepRule,
    /// The folder "Copy in the current folder" used, for the dialog.
    rule_folder: Option<String>,
    /// Groups whose kept copy was picked by hand since the rule was applied.
    manual: HashSet<u32>,
    search: Option<SearchJob>,
    pub(super) found: Option<Found>,
    open: Option<u32>,
    scroll_to_open: bool,
    shown: usize,
    pub(super) confirm: Option<Plan>,
    confirm_focus: bool,
    deleting: Option<DeleteJob>,
    report: Option<Report>,
    /// Paths a finished delete removed, for the add-on to take out of the scan.
    finished: Option<(u64, Vec<String>)>,
    error: Option<String>,
    /// Scan entry id to group id, for the map colours.
    group_of: HashMap<u32, u32>,
}
impl Default for Dupes {
    fn default() -> Self {
        Self {
            min_bytes: dupes::DEFAULT_MIN,
            threads: None,
            drive: std::cell::RefCell::new(None),
            sort: SortBy::default(),
            rule: KeepRule::default(),
            rule_folder: None,
            manual: HashSet::new(),
            search: None,
            found: None,
            open: None,
            scroll_to_open: false,
            shown: PAGE,
            confirm: None,
            confirm_focus: false,
            deleting: None,
            report: None,
            finished: None,
            error: None,
            group_of: HashMap::new(),
        }
    }
}
impl Dupes {
    /// What kind of drive `root` is on (cached: asking Windows opens the drive).
    fn drive_kind(&self, root: &str) -> Kind {
        let mut cached = self.drive.borrow_mut();
        match &*cached {
            Some((r, k)) if r == root => *k,
            _ => {
                let k = crate::scanner::volume_kind(std::path::Path::new(root));
                *cached = Some((root.to_owned(), k));
                k
            }
        }
    }
    /// The threads a search of `root` reads with now.
    fn threads_for(&self, root: &str) -> usize {
        let cores = dupes::cores();
        match self.threads {
            Some(n) => n.clamp(1, cores),
            None => dupes::auto_threads(self.drive_kind(root), cores),
        }
    }
    pub(super) fn busy(&self) -> bool {
        self.search.is_some() || self.deleting.is_some()
    }
    pub(super) fn deleting(&self) -> bool {
        self.deleting.is_some()
    }
    pub(super) fn group_of(&self, node: u32) -> Option<u32> {
        self.group_of.get(&node).copied()
    }
    pub(super) fn color(&self, node: u32) -> Option<Color32> {
        self.group_of(node).map(group_color)
    }
    /// The copies of the group `node` belongs to.
    pub(super) fn copies_of(&self, node: u32) -> Vec<u32> {
        let Some(id) = self.group_of(node) else {
            return vec![];
        };
        self.found
            .iter()
            .flat_map(|f| f.groups.iter().filter(|g| g.id == id))
            .flat_map(|g| g.copies.iter().map(|c| c.node as u32))
            .collect()
    }
    fn index(&mut self) {
        self.group_of.clear();
        if let Some(f) = &self.found {
            for g in &f.groups {
                for c in &g.copies {
                    self.group_of.insert(c.node as u32, g.id);
                }
            }
            if self
                .open
                .is_some_and(|o| !f.groups.iter().any(|g| g.id == o))
            {
                self.open = None;
            }
        }
    }
    fn clear_results(&mut self) {
        self.found = None;
        self.open = None;
        self.confirm = None;
        self.manual.clear();
        self.group_of.clear();
        self.shown = PAGE;
    }
    /// Results and a running search belong to one scan: a new scan clears them.
    pub(super) fn sync(&mut self, generation: u64) {
        if self
            .found
            .as_ref()
            .is_some_and(|f| f.generation != generation)
        {
            self.clear_results();
        }
        if self
            .search
            .as_ref()
            .is_some_and(|s| s.generation != generation)
        {
            self.search = None;
        }
    }
    /// After entries left the scan (a delete): results follow them to their new ids.
    pub(super) fn remap(&mut self, map: &[Option<usize>], old: u64, new: u64) {
        if self.search.as_ref().is_some_and(|s| s.generation == old) {
            self.search = None;
            self.error = Some("The scan changed during the search. Find duplicates again.".into());
        }
        if let Some(f) = self.found.as_mut().filter(|f| f.generation == old) {
            dupes::remap(&mut f.groups, map);
            f.generation = new;
            self.manual
                .retain(|id| f.groups.iter().any(|g| g.id == *id));
            self.confirm = None;
            self.index();
        }
    }
    pub(super) fn poll_search(&mut self) {
        let Some(job) = &self.search else { return };
        match job.rx.try_recv() {
            Ok(mut found) => {
                self.search = None;
                dupes::sort(&mut found.groups, self.sort);
                self.clear_results();
                // The chosen rule carries over; the folder one is chosen again for a new folder.
                if self.rule == KeepRule::InFolder {
                    self.rule = KeepRule::Oldest;
                    self.rule_folder = None;
                } else {
                    dupes::apply_rule(&mut found.groups, self.rule, None);
                }
                self.found = Some(found);
                self.index();
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.search = None;
                self.error = Some("The search stopped unexpectedly. Try again.".into());
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }
    /// A finished delete: its report, and the paths to take out of the scan.
    pub(super) fn take_deleted(&mut self) -> Option<(u64, Vec<String>)> {
        if let Some(job) = &self.deleting {
            match job.rx.try_recv() {
                Ok(report) => {
                    self.deleting = None;
                    self.finished = Some((report.generation, report.deleted.clone()));
                    self.report = Some(report);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.deleting = None;
                    self.error = Some(
                        "The delete stopped unexpectedly. Run a rescan to see what is left.".into(),
                    );
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        self.finished.take()
    }
    pub(super) fn start_search(
        &mut self,
        ctx: &egui::Context,
        nodes: Arc<Vec<crate::scanner::Node>>,
        generation: u64,
        root: String,
    ) {
        let cancel = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(Counters::default());
        let (tx, rx) = mpsc::channel();
        let (c, n, ctx) = (cancel.clone(), counters.clone(), ctx.clone());
        let min = self.min_bytes;
        let threads = self.threads_for(&root);
        let spawned = std::thread::Builder::new()
            .name("disk-duplicates".into())
            .spawn(move || {
                let guard = dupes::guard(root);
                let _ = tx.send(dupes::find(
                    &nodes, generation, min, &guard, &c, &n, threads,
                ));
                ctx.request_repaint();
            });
        match spawned {
            Ok(_) => {
                self.error = None;
                self.report = None;
                self.search = Some(SearchJob {
                    cancel,
                    counters,
                    rx,
                    generation,
                    min_bytes: min,
                });
            }
            Err(e) => self.error = Some(format!("Could not start the search: {e}")),
        }
    }
    pub(super) fn start_delete(&mut self, ctx: &egui::Context, plan: Plan, root: String) {
        let cancel = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(DeleteCounters::default());
        let (tx, rx) = mpsc::channel();
        let (c, n, ctx) = (cancel.clone(), counters.clone(), ctx.clone());
        let files = plan.files();
        let spawned = std::thread::Builder::new()
            .name("disk-duplicates-delete".into())
            .spawn(move || {
                let guard = dupes::guard(root);
                let _ = tx.send(dupes::run(&plan, &guard, &c, &n));
                ctx.request_repaint();
            });
        match spawned {
            Ok(_) => {
                self.error = None;
                self.report = None;
                self.deleting = Some(DeleteJob {
                    cancel,
                    counters,
                    rx,
                    files,
                });
            }
            Err(e) => self.error = Some(format!("Could not start the delete: {e}")),
        }
    }
    fn rule_text(&self) -> String {
        let mut s = match (&self.rule, &self.rule_folder) {
            (KeepRule::InFolder, Some(f)) => format!("{} ({f})", self.rule.label()),
            _ => self.rule.label().to_owned(),
        };
        if !self.manual.is_empty() {
            let n = self.manual.len() as u64;
            s.push_str(&format!(
                ", and your own choice in {} group{}",
                count(n),
                if n == 1 { "" } else { "s" }
            ));
        }
        s
    }
}

/// What the panel asks for, applied after it is drawn.
enum Do {
    Find,
    Stop,
    Toggle(u32),
    Include(u32, bool),
    Keep(u32, usize),
    Select(u32),
    Reveal(String),
    Copy(String),
    DeleteGroup(u32),
    DeleteAll,
    Sort(SortBy),
    Rule(KeepRule),
    More,
    ShowUpTo(usize),
    Scrolled,
    Min(u64),
    Threads(Option<usize>),
    Dismiss,
}

/// The line under the thread selector: what was detected and what Auto does with it.
fn drive_note(kind: Kind, auto: usize, chosen: Option<usize>) -> String {
    let threads = plural(auto as u64, "thread", "threads");
    match kind {
        Kind::Hdd if chosen.is_some_and(|n| n > 1) => {
            "Hard disk detected: more than one thread usually makes it slower.".into()
        }
        Kind::Hdd => format!(
            "Hard disk detected: Auto reads with {threads}, because several make it seek back and forth."
        ),
        Kind::Network => format!("Network drive: Auto reads with {threads}."),
        Kind::Fast => format!("Not a hard disk: Auto reads with {threads}."),
    }
}
fn size_label(v: u64) -> String {
    if v == 0 {
        "0 B".into()
    } else {
        format!("{} MiB", v >> 20)
    }
}
fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{} {}", count(n), if n == 1 { one } else { many })
}
fn danger_button(text: &str, width: f32) -> egui::Button<'_> {
    egui::Button::new(RichText::new(text).color(DANGER_TEXT))
        .fill(Color32::TRANSPARENT)
        .stroke(egui::Stroke::new(1.0, DANGER_BORDER))
        .corner_radius(6)
        .min_size(Vec2::new(width, 28.0))
}
fn focus_ring(ui: &egui::Ui, r: &egui::Response, rect: egui::Rect) {
    if r.has_focus() {
        ui.painter().rect_stroke(
            rect,
            6,
            egui::Stroke::new(1.5, ACCENT),
            egui::StrokeKind::Inside,
        );
    }
}
/// "Coloured = has a copy elsewhere. Dark = unique", with a swatch for each.
fn legend(ui: &mut egui::Ui) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let (r, _) = ui.allocate_exact_size(Vec2::new(30.0, 14.0), Sense::hover());
        for i in 0..3 {
            let s = egui::Rect::from_min_size(
                r.min + Vec2::new(i as f32 * 10.0, 2.0),
                Vec2::splat(9.0),
            );
            ui.painter().rect_filled(s, 2, group_color(i));
        }
        theme::caption(ui, "Coloured = has a copy elsewhere.");
        let (r, _) = ui.allocate_exact_size(Vec2::new(12.0, 14.0), Sense::hover());
        let s = egui::Rect::from_min_size(r.min + Vec2::new(0.0, 2.0), Vec2::splat(9.0));
        ui.painter().rect_filled(s, 2, UNIQUE);
        ui.painter().rect_stroke(
            s,
            2,
            egui::Stroke::new(1.0, HAIRLINE),
            egui::StrokeKind::Outside,
        );
        theme::caption(ui, "Dark = unique");
    });
}

impl Explorer {
    /// The map colour of a tile while Duplicates is on.
    pub(super) fn dupes_fill(&self, kind: TileKind) -> Color32 {
        match kind {
            TileKind::File(id) => self.dupes.color(id).unwrap_or(UNIQUE),
            TileKind::Group { .. } => UNIQUE,
            TileKind::Hidden { .. } => SELECTED_BG,
        }
    }
    /// A map click picked one copy: its group opens in the panel.
    pub(super) fn dupes_follow_selection(&mut self) {
        if let [Sel::Node(id)] = self.view().selected.as_slice() {
            if let Some(g) = self.dupes.group_of(*id) {
                if self.dupes.open != Some(g) {
                    self.dupes.open = Some(g);
                    self.dupes.scroll_to_open = true;
                }
            }
        }
    }

    /// The right panel while Duplicates is on.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn dupes_panel(
        &mut self,
        ui: &mut egui::Ui,
        model: &Arc<Model>,
        layout: &Layout,
        root: &str,
        scanning: bool,
        modal: bool,
        out: &mut Vec<Command>,
    ) {
        let width = ui.available_width();
        let mut todo: Vec<Do> = vec![];
        let d = &self.dupes;
        // A delete running, or the report of the last one.
        if let Some(job) = &d.deleting {
            aside_card(ui, width, |ui| {
                theme::section(ui, "Deleting duplicates...");
                let done = job.counters.done.load(Ordering::Relaxed);
                let freed = job.counters.freed.load(Ordering::Relaxed);
                theme::bar(ui, done as f32 / job.files.max(1) as f32, ACCENT, 6.0);
                theme::caption(
                    ui,
                    format!(
                        "{} of {} files · {} freed",
                        count(done),
                        count(job.files),
                        crate::size(freed)
                    ),
                );
                if ui
                    .add(theme::outlined("Stop").min_size(Vec2::new(ui.available_width(), 28.0)))
                    .on_hover_text(
                        "Stops before the next file. Files already deleted stay deleted.",
                    )
                    .clicked()
                {
                    todo.push(Do::Stop);
                }
            });
            ui.add_space(12.0);
        } else if let Some(r) = &d.report {
            aside_card(ui, width, |ui| {
                theme::section(ui, "Delete finished");
                ui.add(egui::Label::new(RichText::new(r.summary()).strong()).wrap());
                if r.changed > 0 {
                    theme::caption(
                        ui,
                        format!(
                            "{} changed since the check and {} left alone.",
                            plural(r.changed, "file", "files"),
                            if r.changed == 1 { "was" } else { "were" }
                        ),
                    );
                }
                for e in r.failed.iter().take(20) {
                    ui.add(egui::Label::new(RichText::new(e).size(11.0).color(DANGER_TEXT)).wrap());
                }
                if r.failed.len() > 20 {
                    theme::caption(
                        ui,
                        format!("and {} more", count(r.failed.len() as u64 - 20)),
                    );
                }
                theme::caption(ui, "Deleted files did not go to the Recycle Bin.");
                if ui.add(theme::outlined("Dismiss")).clicked() {
                    todo.push(Do::Dismiss);
                }
            });
            ui.add_space(12.0);
        }
        // Search: the filter, the one primary button, progress and what was found.
        aside_card(ui, width, |ui| {
            theme::section(ui, "Duplicates");
            legend(ui);
            theme::caption(
                ui,
                "Small files drawn as one tile stay dark: zoom in to see them.",
            );
            ui.add_space(6.0);
            let mut min = d.min_bytes;
            ui.horizontal(|ui| {
                ui.label(RichText::new("Ignore files smaller than").size(12.0));
                let r = egui::ComboBox::from_id_salt("space-dupes-min")
                    .selected_text(size_label(min))
                    .show_ui(ui, |ui| {
                        for v in dupes::MIN_OPTIONS {
                            ui.selectable_value(&mut min, v, size_label(v));
                        }
                    });
                r.response.widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::Role::ComboBox,
                        true,
                        format!("Ignore files smaller than {}", size_label(min)),
                    )
                });
            });
            if min != d.min_bytes {
                todo.push(Do::Min(min));
            }
            ui.add_space(4.0);
            let cores = dupes::cores();
            let kind = d.drive_kind(root);
            let auto = dupes::auto_threads(kind, cores);
            let mut threads = d.threads;
            let shown = match threads {
                None => format!("Auto ({auto} of {cores})"),
                Some(n) => format!("{} of {cores}", n.clamp(1, cores)),
            };
            ui.horizontal(|ui| {
                ui.label(RichText::new("Threads to use").size(12.0));
                let r = egui::ComboBox::from_id_salt("space-dupes-threads")
                    .selected_text(shown.clone())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut threads,
                            None,
                            format!("Auto ({auto} of {cores})"),
                        );
                        for n in 1..=cores {
                            ui.selectable_value(&mut threads, Some(n), format!("{n} of {cores}"));
                        }
                    });
                r.response.widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::Role::ComboBox,
                        true,
                        format!("Threads to use: {shown}"),
                    )
                });
            });
            if threads != d.threads {
                todo.push(Do::Threads(threads));
            }
            theme::caption(ui, drive_note(kind, auto, d.threads));
            ui.add_space(6.0);
            if let Some(job) = &d.search {
                let total = job.counters.total.load(Ordering::Relaxed);
                let done = job.counters.checked.load(Ordering::Relaxed);
                let read = job.counters.bytes.load(Ordering::Relaxed);
                theme::weak(ui, "Looking for files with the same content...");
                theme::bar(ui, done as f32 / total.max(1) as f32, ACCENT, 6.0);
                theme::caption(
                    ui,
                    format!(
                        "Checked {} of {} candidate files · {} read",
                        count(done),
                        count(total),
                        crate::size(read)
                    ),
                );
                if ui
                    .add(theme::outlined("Stop").min_size(Vec2::new(ui.available_width(), 28.0)))
                    .clicked()
                {
                    todo.push(Do::Stop);
                }
                if job.min_bytes != min {
                    theme::caption(ui, "The new size applies to the next search.");
                }
            } else {
                match &d.found {
                    None => {
                        theme::weak(
                            ui,
                            "Finds files with exactly the same content. Files that share a size are read and compared; nothing is changed.",
                        );
                        theme::caption(
                            ui,
                            "Cloud-only files (OneDrive and similar) are skipped, so nothing is downloaded. Reading a large drive takes a while.",
                        );
                    }
                    Some(f) => {
                        let shown: Vec<&dupes::Group> =
                            f.groups.iter().filter(|g| g.bytes >= min).collect();
                        let copies: u64 = shown.iter().map(|g| g.copies.len() as u64).sum();
                        let wasted = dupes::wasted(shown.iter().copied());
                        if shown.is_empty() {
                            let of = if f.min_bytes.max(min) > 0 {
                                format!(" of {} or more", size_label(f.min_bytes.max(min)))
                            } else {
                                String::new()
                            };
                            let what = if f.stopped {
                                format!(
                                    "No duplicates found among the {} files checked before Stop.",
                                    count(f.checked)
                                )
                            } else {
                                format!(
                                    "No duplicates found among {} files{of}.",
                                    count(f.considered)
                                )
                            };
                            ui.add(egui::Label::new(RichText::new(what).strong()).wrap());
                        } else {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(format!(
                                        "{} · {} · {} could be freed",
                                        plural(shown.len() as u64, "group", "groups"),
                                        plural(copies, "copy", "copies"),
                                        crate::size(wasted)
                                    ))
                                    .strong(),
                                )
                                .wrap(),
                            );
                        }
                        let mut notes = vec![];
                        if f.stopped {
                            notes.push(format!(
                                "Stopped early: these are the duplicates among the {} files checked.",
                                count(f.checked)
                            ));
                        }
                        if f.unreadable > 0 {
                            notes.push(format!(
                                "{} could not be read (in use or access denied) and {} skipped.",
                                plural(f.unreadable, "file", "files"),
                                if f.unreadable == 1 { "was" } else { "were" }
                            ));
                        }
                        if f.cloud > 0 {
                            notes.push(format!(
                                "{} cloud-only {} skipped so nothing was downloaded.",
                                count(f.cloud),
                                if f.cloud == 1 {
                                    "file was"
                                } else {
                                    "files were"
                                }
                            ));
                        }
                        if f.changed > 0 {
                            notes.push(format!(
                                "{} changed since the scan and {} skipped. Rescan to include them.",
                                plural(f.changed, "file", "files"),
                                if f.changed == 1 { "was" } else { "were" }
                            ));
                        }
                        if f.links_left_out > 0 {
                            notes.push(format!(
                                "{} to files already listed {} left out: they use no extra space.",
                                plural(f.links_left_out, "hard link", "hard links"),
                                if f.links_left_out == 1 { "was" } else { "were" }
                            ));
                        }
                        if f.links_unknown > 0 {
                            notes.push(format!(
                                "Windows did not say whether {} are hard links of each other, so they are listed as separate copies.",
                                plural(f.links_unknown, "copy", "copies")
                            ));
                        }
                        if min < f.min_bytes {
                            notes.push(format!(
                                "This search left out files under {}. Find duplicates again to include them.",
                                size_label(f.min_bytes)
                            ));
                        }
                        for n in notes {
                            theme::caption(ui, n);
                        }
                    }
                }
                if let Some(e) = &d.error {
                    ui.add(egui::Label::new(RichText::new(e).size(11.0).color(DANGER_TEXT)).wrap());
                }
                ui.add_space(4.0);
                let can = !scanning && d.deleting.is_none();
                let find = ui
                    .add_enabled(
                        can,
                        egui::Button::new(RichText::new("Find duplicates").color(TEXT).strong())
                            .fill(toolboks_shared::ui::HOST_ACCENT_FILL)
                            .corner_radius(6)
                            .min_size(Vec2::new(ui.available_width(), 28.0)),
                    )
                    .on_disabled_hover_text(if scanning {
                        "Wait for the scan to finish."
                    } else {
                        "Wait for the delete to finish."
                    });
                if find.clicked() {
                    todo.push(Do::Find);
                }
            }
        });
        // The groups.
        let d = &self.dupes;
        if let Some(f) = d.found.as_ref().filter(|f| !f.groups.is_empty()) {
            let min = d.min_bytes;
            let shown: Vec<&dupes::Group> = f.groups.iter().filter(|g| g.bytes >= min).collect();
            if !shown.is_empty() {
                ui.add_space(12.0);
                let can_delete = !scanning && !modal && !d.busy();
                let open_at = d.open.and_then(|o| shown.iter().position(|g| g.id == o));
                let scroll = d.scroll_to_open;
                if let Some(at) = open_at.filter(|&at| scroll && at >= d.shown) {
                    todo.push(Do::ShowUpTo(at + 1));
                } else if scroll {
                    todo.push(Do::Scrolled);
                }
                let bulk = dupes::plan(&f.groups, None, min, f.generation, String::new()).ok();
                aside_card(ui, width, |ui| {
                    let mut sort = d.sort;
                    let mut rule = d.rule;
                    egui::Grid::new("space-dupes-controls")
                        .num_columns(2)
                        .spacing([8.0, 6.0])
                        .show(ui, |ui| {
                            ui.label(RichText::new("Sort by").size(12.0));
                            let r = egui::ComboBox::from_id_salt("space-dupes-sort")
                                .selected_text(sort.label())
                                .width(150.0)
                                .show_ui(ui, |ui| {
                                    for s in SortBy::ALL {
                                        ui.selectable_value(&mut sort, s, s.label());
                                    }
                                });
                            r.response.widget_info(|| {
                                egui::WidgetInfo::labeled(
                                    egui::Role::ComboBox,
                                    true,
                                    format!("Sort by {}", sort.label()),
                                )
                            });
                            ui.end_row();
                            ui.label(RichText::new("Keep").size(12.0));
                            let r = egui::ComboBox::from_id_salt("space-dupes-rule")
                                .selected_text(rule.label())
                                .width(150.0)
                                .show_ui(ui, |ui| {
                                    for k in KeepRule::ALL {
                                        ui.selectable_value(&mut rule, k, k.label());
                                    }
                                });
                            r.response.widget_info(|| {
                                egui::WidgetInfo::labeled(
                                    egui::Role::ComboBox,
                                    true,
                                    format!("Keep {}", rule.label()),
                                )
                            });
                            ui.end_row();
                        });
                    if sort != d.sort {
                        todo.push(Do::Sort(sort));
                    }
                    if rule != d.rule {
                        todo.push(Do::Rule(rule));
                    }
                    if let (KeepRule::InFolder, Some(folder)) = (d.rule, &d.rule_folder) {
                        theme::caption(
                            ui,
                            format!(
                                "Keeps the copy in {folder} where there is one, else the oldest."
                            ),
                        );
                    }
                    ui.add_space(4.0);
                    let full = ui.available_width();
                    let (files, bytes) = bulk.as_ref().map_or((0, 0), |p| (p.files(), p.bytes()));
                    let r = ui
                        .add_enabled(
                            can_delete && files > 0,
                            danger_button("Delete duplicates...", full),
                        )
                        .on_disabled_hover_text(if scanning {
                            "Wait for the scan to finish."
                        } else if files == 0 {
                            "Nothing to delete in the ticked groups."
                        } else {
                            "Wait for the other delete to finish."
                        });
                    if r.clicked() {
                        todo.push(Do::DeleteAll);
                    }
                    theme::caption(
                        ui,
                        format!(
                            "{} · {} from the ticked groups. One copy of each is kept.",
                            plural(files, "file", "files"),
                            crate::size(bytes)
                        ),
                    );
                    ui.add_space(6.0);
                    for (i, g) in shown.iter().enumerate().take(d.shown) {
                        if i > 0 {
                            let y = ui.cursor().top();
                            let x = ui.max_rect().x_range();
                            ui.painter().hline(x, y, egui::Stroke::new(1.0, HAIRLINE));
                            ui.add_space(2.0);
                        }
                        group_row(ui, g, d.open == Some(g.id), scroll, &mut todo);
                        if d.open == Some(g.id) {
                            group_body(ui, g, can_delete, &mut todo);
                        }
                    }
                    if shown.len() > d.shown {
                        let more = (shown.len() - d.shown).min(PAGE) as u64;
                        if ui
                            .add(
                                theme::outlined(&format!("Show {} more", count(more)))
                                    .min_size(Vec2::new(full, 28.0)),
                            )
                            .clicked()
                        {
                            todo.push(Do::More);
                        }
                    }
                });
            }
        }
        self.dupes_apply(todo, model, layout, root, ui.ctx(), out);
    }

    fn dupes_apply(
        &mut self,
        todo: Vec<Do>,
        model: &Arc<Model>,
        layout: &Layout,
        root: &str,
        ctx: &egui::Context,
        out: &mut Vec<Command>,
    ) {
        for t in todo {
            match t {
                Do::Find => {
                    self.dupes.start_search(
                        ctx,
                        model.nodes.clone(),
                        model.generation,
                        root.to_owned(),
                    );
                }
                Do::Stop => {
                    if let Some(j) = &self.dupes.search {
                        j.cancel.store(true, Ordering::Relaxed);
                    }
                    if let Some(j) = &self.dupes.deleting {
                        j.cancel.store(true, Ordering::Relaxed);
                    }
                }
                Do::Toggle(id) => {
                    if self.dupes.open == Some(id) {
                        self.dupes.open = None;
                    } else {
                        self.dupes.open = Some(id);
                        let copies = self.group_copies(id);
                        self.view_mut().selected = copies.into_iter().map(Sel::Node).collect();
                    }
                }
                Do::Include(id, on) => {
                    if let Some(g) = self.group_mut(id) {
                        g.included = on;
                    }
                }
                Do::Keep(id, i) => {
                    if let Some(g) = self.group_mut(id) {
                        if i < g.copies.len() {
                            g.keep = i;
                        }
                    }
                    self.dupes.manual.insert(id);
                }
                Do::Select(node) => {
                    if layout.find(TileKind::File(node)).is_none() {
                        // Not drawn here: show the folder it is in.
                        if let Some(p) = model.node(node as usize).and_then(|n| n.parent) {
                            self.zoom_to(p);
                        }
                    }
                    self.view_mut().selected = vec![Sel::Node(node)];
                }
                Do::Reveal(path) => out.push(Command::Reveal(path)),
                Do::Copy(path) => ctx.copy_text(crate::display_path(&path)),
                Do::DeleteGroup(id) => self.dupes_ask(Some(id)),
                Do::DeleteAll => self.dupes_ask(None),
                Do::Sort(s) => {
                    self.dupes.sort = s;
                    if let Some(f) = self.dupes.found.as_mut() {
                        dupes::sort(&mut f.groups, s);
                    }
                    self.dupes.scroll_to_open = self.dupes.open.is_some();
                }
                Do::Rule(r) => {
                    let folder = (r == KeepRule::InFolder)
                        .then(|| model.node(self.view_folder()).map(|n| n.path.clone()))
                        .flatten();
                    self.dupes.rule = r;
                    self.dupes.rule_folder = folder.as_deref().map(crate::display_path);
                    self.dupes.manual.clear();
                    if let Some(f) = self.dupes.found.as_mut() {
                        dupes::apply_rule(&mut f.groups, r, folder.as_deref());
                    }
                }
                Do::More => self.dupes.shown += PAGE,
                Do::ShowUpTo(n) => self.dupes.shown = self.dupes.shown.max(n.div_ceil(PAGE) * PAGE),
                Do::Scrolled => self.dupes.scroll_to_open = false,
                Do::Min(v) => self.dupes.min_bytes = v,
                Do::Threads(v) => self.dupes.threads = v,
                Do::Dismiss => self.dupes.report = None,
            }
        }
    }
    fn group_mut(&mut self, id: u32) -> Option<&mut dupes::Group> {
        self.dupes
            .found
            .as_mut()?
            .groups
            .iter_mut()
            .find(|g| g.id == id)
    }
    fn group_copies(&self, id: u32) -> Vec<u32> {
        self.dupes
            .found
            .iter()
            .flat_map(|f| f.groups.iter().filter(|g| g.id == id))
            .flat_map(|g| g.copies.iter().map(|c| c.node as u32))
            .collect()
    }
    /// Opens the confirmation for one group, or for every ticked group.
    fn dupes_ask(&mut self, group: Option<u32>) {
        let Some(f) = &self.dupes.found else { return };
        let ids = group.map(|g| vec![g]);
        let rule = match group {
            Some(_) => "The copy you chose".to_owned(),
            None => self.dupes.rule_text(),
        };
        let min = if group.is_some() {
            0
        } else {
            self.dupes.min_bytes
        };
        match dupes::plan(&f.groups, ids.as_deref(), min, f.generation, rule) {
            Ok(plan) => {
                self.dupes.confirm = Some(plan);
                self.dupes.confirm_focus = true;
            }
            Err(e) => self.dupes.error = Some(e),
        }
    }
    /// The "Delete N duplicate files permanently?" dialog. `blocked` says why it may not start.
    pub(super) fn dupes_confirm(&mut self, ctx: &egui::Context, root: &str, blocked: Option<&str>) {
        let Some(plan) = &self.dupes.confirm else {
            return;
        };
        let files = plan.files();
        let mut cancel = false;
        let mut confirm = false;
        let modal = egui::Modal::new(egui::Id::unique("disk-dupes-confirm")).show(ctx, |ui| {
            ui.set_max_width(480.0);
            theme::section(
                ui,
                &format!(
                    "Delete {} permanently?",
                    plural(files, "duplicate file", "duplicate files")
                ),
            );
            ui.add_space(theme::SPACE_M);
            ui.label(
                RichText::new(format!(
                    "Frees {} from {}.",
                    crate::size(plan.bytes()),
                    plural(plan.groups.len() as u64, "group", "groups")
                ))
                .strong(),
            );
            theme::caption(ui, format!("Keep: {}", plan.rule));
            if plan.left_alone > 0 {
                theme::caption(
                    ui,
                    format!(
                        "{} in protected folders or hard-linked {} kept as well.",
                        plural(plan.left_alone, "copy", "copies"),
                        if plan.left_alone == 1 { "is" } else { "are" }
                    ),
                );
            }
            ui.add_space(theme::SPACE_M);
            theme::weak(
                ui,
                "This cannot be undone. The files are deleted now and do not go to the Recycle Bin. One copy of each file is kept, and each file is checked again just before it is deleted: anything that changed is left alone.",
            );
            ui.add_space(theme::SPACE_M);
            egui::CollapsingHeader::new("What is deleted and what is kept")
                .id_salt("disk-dupes-list")
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(220.0)
                        .show(ui, |ui| {
                            for g in plan.groups.iter().take(200) {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(format!(
                                            "Keep  {}",
                                            crate::display_path(&g.keep.path)
                                        ))
                                        .size(11.0)
                                        .color(TEXT),
                                    )
                                    .wrap(),
                                );
                                for c in &g.delete {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(format!(
                                                "Delete  {}",
                                                crate::display_path(&c.path)
                                            ))
                                            .size(11.0)
                                            .color(DANGER_TEXT),
                                        )
                                        .wrap(),
                                    );
                                }
                                ui.add_space(4.0);
                            }
                            if plan.groups.len() > 200 {
                                theme::caption(
                                    ui,
                                    format!(
                                        "and {} more",
                                        plural(plan.groups.len() as u64 - 200, "group", "groups")
                                    ),
                                );
                            }
                        });
                });
            if let Some(why) = blocked {
                theme::caption(ui, why);
            }
            ui.add_space(theme::SPACE_L);
            ui.horizontal(|ui| {
                let keep = ui.add(theme::outlined("Cancel"));
                if std::mem::take(&mut self.dupes.confirm_focus) {
                    keep.request_focus();
                }
                let delete = ui.add_enabled(
                    blocked.is_none(),
                    egui::Button::new(
                        RichText::new(format!("Delete {}", plural(files, "file", "files")))
                            .color(Color32::WHITE)
                            .strong(),
                    )
                    .fill(crate::delete::DANGER_FILL)
                    .corner_radius(6)
                    .min_size(egui::vec2(0.0, 28.0)),
                );
                cancel = keep.clicked();
                confirm = delete.clicked();
            });
        });
        if cancel || modal.should_close() {
            self.dupes.confirm = None;
        } else if confirm && blocked.is_none() {
            if let Some(plan) = self.dupes.confirm.take() {
                self.dupes.start_delete(ctx, plan, root.to_owned());
            }
        }
    }
}

/// One group: tick box, then name, size and copies, and the space it wastes. A click opens it.
fn group_row(ui: &mut egui::Ui, g: &dupes::Group, open: bool, scroll: bool, todo: &mut Vec<Do>) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let mut on = g.included;
        let c = ui.add(egui::Checkbox::without_text(&mut on));
        c.widget_info(|| {
            egui::WidgetInfo::selected(
                egui::Role::CheckBox,
                true,
                on,
                format!("Include {} in Delete duplicates", g.name()),
            )
        });
        if on != g.included {
            todo.push(Do::Include(g.id, on));
        }
        let (rect, r) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 38.0), Sense::click());
        if open || r.hovered() || r.has_focus() {
            ui.painter().rect_filled(rect, 6, SELECTED_BG);
        }
        focus_ring(ui, &r, rect);
        let p = ui.painter();
        // Chevron: right when closed, down when open.
        let c = egui::pos2(rect.left() + 8.0, rect.top() + 12.0);
        let pts = if open {
            vec![
                c + Vec2::new(-4.0, -2.0),
                c + Vec2::new(0.0, 2.0),
                c + Vec2::new(4.0, -2.0),
            ]
        } else {
            vec![
                c + Vec2::new(-2.0, -4.0),
                c + Vec2::new(2.0, 0.0),
                c + Vec2::new(-2.0, 4.0),
            ]
        };
        p.line(pts, egui::Stroke::new(1.5, TEXT));
        let dot = egui::Rect::from_center_size(
            egui::pos2(rect.left() + 20.0, rect.top() + 12.0),
            Vec2::splat(8.0),
        );
        p.rect_filled(dot, 2, group_color(g.id));
        let wasted = crate::size(g.wasted());
        let font = egui::FontId::proportional(12.0);
        let wg = p.layout_no_wrap(wasted.clone(), font.clone(), TEXT);
        let right = rect.right() - 6.0;
        p.galley(
            egui::pos2(right - wg.size().x, rect.top() + 12.0 - wg.size().y / 2.0),
            wg.clone(),
            TEXT,
        );
        let room = (rect.width() - 30.0 - wg.size().x - 14.0).max(10.0);
        let mut job = super::fit_job(g.name(), room, TEXT);
        job.sections[0].format.font_id = font;
        let ng = p.layout_job(job);
        p.galley(
            egui::pos2(rect.left() + 28.0, rect.top() + 12.0 - ng.size().y / 2.0),
            ng,
            TEXT,
        );
        let line2 = format!(
            "{} · {}",
            crate::size(g.bytes),
            plural(g.copies.len() as u64, "copy", "copies")
        );
        let lg = p.layout_job(super::fit_job(&line2, rect.width() - 90.0, MUTED));
        p.galley(egui::pos2(rect.left() + 28.0, rect.top() + 21.0), lg, MUTED);
        let tag = p.layout_no_wrap("wasted".into(), egui::FontId::proportional(11.0), MUTED);
        p.galley(
            egui::pos2(right - tag.size().x, rect.top() + 21.0),
            tag,
            MUTED,
        );
        if r.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let label = format!(
            "{}, {}, {}, {} wasted, {}",
            g.name(),
            crate::size(g.bytes),
            plural(g.copies.len() as u64, "copy", "copies"),
            wasted,
            if open { "open" } else { "closed" }
        );
        r.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, true, open, &label));
        if open && scroll {
            r.scroll_to_me(Some(egui::Align::Center));
        }
        if r.clicked() {
            todo.push(Do::Toggle(g.id));
        }
    });
}

/// An open group: every copy with its folder, date, the Keep choice and its actions.
fn group_body(ui: &mut egui::Ui, g: &dupes::Group, can_delete: bool, todo: &mut Vec<Do>) {
    ui.indent(("space-dupes-group", g.id), |ui| {
        for (i, c) in g.copies.iter().enumerate() {
            ui.add_space(4.0);
            let kept = i == g.keep;
            ui.horizontal(|ui| {
                let r = ui.radio(kept, "Keep");
                r.widget_info(|| {
                    egui::WidgetInfo::selected(
                        egui::Role::RadioButton,
                        true,
                        kept,
                        format!("Keep the copy in {}", crate::display_path(&c.folder())),
                    )
                });
                if r.clicked() && !kept {
                    todo.push(Do::Keep(g.id, i));
                }
                let tag = if c.protected.is_some() {
                    Some("Protected")
                } else if c.linked {
                    Some("Hard link")
                } else {
                    None
                };
                if let Some(tag) = tag {
                    let why = c.protected.clone().unwrap_or_else(|| {
                        "This file has more than one name (a hard link). Deleting one name frees no space, so it is never deleted.".into()
                    });
                    ui.label(RichText::new(tag).size(11.0).color(ACCENT_TEXT))
                        .on_hover_text(why);
                }
                theme::caption(ui, format!("modified {}", date(c.modified)));
            });
            let folder = crate::display_path(&c.folder());
            let r = ui
                .add(
                    egui::Label::new(RichText::new(&folder).size(11.0).color(TEXT))
                        .wrap()
                        .sense(Sense::click()),
                )
                .on_hover_text("Select it on the map");
            focus_ring(ui, &r, r.rect.expand(2.0));
            r.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::Role::Link,
                    true,
                    format!("Select {} in {folder} on the map", c.name()),
                )
            });
            if r.clicked() {
                todo.push(Do::Select(c.node as u32));
            }
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                if ui.add(theme::outlined("Show in Explorer")).clicked() {
                    todo.push(Do::Reveal(c.path.clone()));
                }
                if ui.add(theme::outlined("Copy path")).clicked() {
                    todo.push(Do::Copy(c.path.clone()));
                }
            });
        }
        ui.add_space(6.0);
        let n = g.to_delete().count() as u64;
        let r = ui
            .add_enabled(
                can_delete && n > 0,
                danger_button("Delete other copies", ui.available_width()),
            )
            .on_disabled_hover_text(if n == 0 {
                "The other copies are protected or hard links."
            } else {
                "Not now: a scan or another delete is running."
            });
        if r.clicked() {
            todo.push(Do::DeleteGroup(g.id));
        }
        ui.add_space(6.0);
    });
}

#[cfg(test)]
mod tests {
    use super::super::Act;
    use super::*;
    use crate::scanner::{Node, Progress, State};
    use egui::{Pos2, Rect};
    use std::sync::Mutex;

    fn group(id: u32, nodes: &[usize]) -> dupes::Group {
        dupes::Group {
            id,
            bytes: 10,
            hash: [0; 32],
            copies: nodes
                .iter()
                .map(|&n| dupes::Copy {
                    node: n,
                    path: format!(r"D:\f{n}\x.bin"),
                    modified: 0,
                    stamp: n as u64,
                    protected: None,
                    linked: false,
                })
                .collect(),
            keep: 0,
            included: true,
        }
    }

    #[test]
    fn results_follow_a_delete_and_clear_when_the_scan_changes() {
        let mut d = Dupes {
            found: Some(Found {
                generation: 1,
                groups: vec![group(0, &[3, 5]), group(1, &[7, 8, 9])],
                ..Default::default()
            }),
            ..Default::default()
        };
        d.index();
        d.open = Some(0);
        assert_eq!(d.group_of(8), Some(1));
        assert_eq!(d.copies_of(3), [3, 5]);
        d.sync(1);
        assert!(d.found.is_some(), "same scan, same results");
        // A delete removed entry 5 and shifted the rest down by one.
        let map: Vec<Option<usize>> = (0..10)
            .map(|i| match i {
                5 => None,
                i if i > 5 => Some(i - 1),
                i => Some(i),
            })
            .collect();
        d.remap(&map, 1, 2);
        let f = d.found.as_ref().unwrap();
        assert_eq!(f.generation, 2);
        assert_eq!(f.groups.len(), 1, "a group with one copy left disappears");
        assert_eq!(d.open, None);
        assert_eq!(d.group_of(7), Some(1), "8 is now 7");
        assert_eq!(d.group_of(3), None);
        // A new scan clears everything, and stops a search of the old one.
        let cancel = Arc::new(AtomicBool::new(false));
        let (_tx, rx) = mpsc::channel();
        d.search = Some(SearchJob {
            cancel: cancel.clone(),
            counters: Default::default(),
            rx,
            generation: 2,
            min_bytes: 0,
        });
        d.sync(3);
        assert!(d.found.is_none() && d.group_of(7).is_none());
        assert!(d.search.is_none() && cancel.load(Ordering::Relaxed));
        assert_eq!(d.min_bytes, dupes::DEFAULT_MIN, "1 MiB by default");
    }

    #[test]
    fn colours_are_distinct_per_group_and_unique_tiles_are_dark() {
        let mut seen: Vec<_> = (0..10).map(|i| group_color(i).to_array()).collect();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 10);
        assert_eq!(group_color(10), group_color(0), "the palette cycles");
        for i in 0..10 {
            let c = group_color(i);
            let max = c.r().max(c.g()).max(c.b()) as f32;
            let min = c.r().min(c.g()).min(c.b()) as f32;
            assert!((max - min) / max >= 0.3, "colourful");
        }
        let max = UNIQUE.r().max(UNIQUE.g()).max(UNIQUE.b());
        assert!(max < 0x40, "unique tiles are a dark neutral");
        assert_ne!(UNIQUE, SELECTED_BG, "not the hidden strip");
        assert_ne!(
            UNIQUE,
            toolboks_shared::ui::WINDOW_BG,
            "not the map background"
        );
    }

    fn frame(
        ctx: &egui::Context,
        ex: &mut Explorer,
        p: &Progress,
        nodes: &Arc<Vec<Node>>,
    ) -> Vec<Command> {
        let mut commands = vec![];
        let mut out = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1400.0, 900.0))),
                ..Default::default()
            },
            |ui| commands = ex.show(ui, p, nodes, (0, 0), false),
        );
        out.textures_delta.clear();
        commands
    }
    fn until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !done() {
            assert!(
                std::time::Instant::now() < deadline,
                "{what} never happened"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    #[test]
    fn the_panel_finds_shows_and_deletes_duplicates() {
        let tmp = tempfile::Builder::new()
            .prefix("toolboks-dupes-ui-")
            .tempdir()
            .unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        for (rel, byte, len) in [
            ("Photos/beach.jpg", 9u8, 6000usize),
            ("Backup/beach.jpg", 9, 6000),
            ("Desktop/beach.jpg", 9, 6000),
            ("Music/song.mp3", 4, 3000),
            ("Old/song.mp3", 4, 3000),
            ("unique.bin", 1, 9000),
        ] {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, vec![byte; len]).unwrap();
        }
        let state = Arc::new(Mutex::new(State::default()));
        crate::scanner::scan(root.clone(), vec![], state.clone(), Default::default());
        let (mut p, nodes) = {
            let s = state.lock().unwrap();
            (s.progress.clone(), s.nodes.clone())
        };
        p.generation = 1;
        let ctx = egui::Context::default();
        let mut ex = Explorer::default();
        ex.open_at(0, 1, false);
        until("the map", || {
            frame(&ctx, &mut ex, &p, &nodes);
            ex.model.is_some()
        });
        // Duplicates on: the panel before the first search, then a search with 0 B as minimum.
        ex.view_mut().dupes = true;
        ex.dupes.min_bytes = 0;
        frame(&ctx, &mut ex, &p, &nodes);
        assert!(
            ex.dupes.found.is_none() && !ex.busy(),
            "nothing is read until asked"
        );
        let model = ex.model.clone().unwrap();
        ex.dupes
            .start_search(&ctx, model.nodes.clone(), 1, p.root.clone());
        until("the search", || {
            frame(&ctx, &mut ex, &p, &nodes);
            ex.dupes.found.is_some()
        });
        let f = ex.dupes.found.clone().unwrap();
        assert_eq!(f.groups.len(), 2);
        assert_eq!(f.groups[0].copies.len(), 3, "sorted by wasted space");
        let id_of = |name: &str| {
            nodes
                .iter()
                .find(|n| n.path.replace('/', "\\").ends_with(name))
                .unwrap()
                .id as u32
        };
        let photo = id_of(r"Photos\beach.jpg");
        let unique = id_of("unique.bin");
        assert_eq!(
            ex.dupes_fill(TileKind::File(photo)),
            group_color(f.groups[0].id)
        );
        assert_eq!(ex.dupes_fill(TileKind::File(unique)), UNIQUE);
        // Open the photo group: its copies are selected on the map.
        let layout = Layout::default();
        ex.dupes_apply(
            vec![Do::Toggle(f.groups[0].id)],
            &model,
            &layout,
            &p.root,
            &ctx,
            &mut vec![],
        );
        assert_eq!(ex.view().selected.len(), 3);
        frame(&ctx, &mut ex, &p, &nodes);
        // "Select its other copies" from the menu.
        ex.act(
            Act::OtherCopies,
            &[Sel::Node(photo)],
            &model,
            &ctx,
            &mut vec![],
        );
        assert_eq!(ex.view().selected.len(), 2);
        assert!(!ex.view().selected.contains(&Sel::Node(photo)));
        // A map click on one copy opens its group.
        ex.dupes.open = None;
        ex.view_mut().selected = vec![Sel::Node(id_of(r"Old\song.mp3"))];
        ex.dupes_follow_selection();
        assert_eq!(ex.dupes.open, Some(f.groups[1].id));
        // Keep the Photos copy, sort by name (the open group stays open), then ask to delete
        // every ticked group's extra copies.
        let keep = f.groups[0]
            .copies
            .iter()
            .position(|c| c.node as u32 == photo)
            .unwrap();
        ex.dupes_apply(
            vec![
                Do::Keep(f.groups[0].id, keep),
                Do::Sort(SortBy::Name),
                Do::DeleteAll,
            ],
            &model,
            &layout,
            &p.root,
            &ctx,
            &mut vec![],
        );
        assert_eq!(ex.dupes.open, Some(f.groups[1].id));
        let plan = ex.dupes.confirm.clone().expect("the dialog opens");
        assert_eq!(plan.files(), 3);
        assert!(
            plan.rule.contains("your own choice in 1 group"),
            "{}",
            plan.rule
        );
        frame(&ctx, &mut ex, &p, &nodes);
        assert!(
            ex.dupes.confirm.is_some() && !ex.deleting(),
            "nothing runs until confirmed"
        );
        ex.dupes.confirm = None;
        ex.dupes.start_delete(&ctx, plan, p.root.clone());
        assert!(ex.deleting());
        let mut taken = None;
        until("the delete", || {
            frame(&ctx, &mut ex, &p, &nodes);
            taken = ex.take_deleted();
            taken.is_some()
        });
        let (generation, paths) = taken.unwrap();
        assert_eq!((generation, paths.len()), (1, 3));
        assert!(root.join("Photos/beach.jpg").exists());
        assert!(!root.join("Backup/beach.jpg").exists());
        assert!(!root.join("Desktop/beach.jpg").exists());
        let songs = [
            root.join("Music/song.mp3").exists(),
            root.join("Old/song.mp3").exists(),
        ];
        assert_eq!(songs.iter().filter(|e| **e).count(), 1, "one song is kept");
        assert!(ex
            .dupes
            .report
            .as_ref()
            .unwrap()
            .summary()
            .starts_with("Deleted 3 files"));
        frame(&ctx, &mut ex, &p, &nodes);
        // The add-on patches the scan; the results follow and both groups are gone.
        let mut p2 = p.clone();
        let (patched, map) = crate::delete::remove_all(&mut p2, &nodes, &paths).unwrap();
        p2.generation = 2;
        ex.remap(&map, 1, 2);
        let nodes2 = Arc::new(patched);
        frame(&ctx, &mut ex, &p2, &nodes2);
        let f = ex.dupes.found.as_ref().unwrap();
        assert!(f.groups.is_empty());
        assert_eq!(f.generation, 2);
        // A new scan clears the results.
        let mut p3 = p2.clone();
        p3.generation = 3;
        frame(&ctx, &mut ex, &p3, &nodes2);
        assert!(ex.dupes.found.is_none());
        // Colour by returns to the usual panel.
        ex.view_mut().dupes = false;
        frame(&ctx, &mut ex, &p3, &nodes2);
    }
}
