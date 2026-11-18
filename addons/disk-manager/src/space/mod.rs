//! Space explorer (issue #180, design G): a full-window, WizTree-style map of one scan. Every
//! file is a tile sized by its logical size and coloured by type, age or top folder; folders
//! are blocks with thin borders. The wheel zooms toward the pointer, the middle button pans,
//! the left button selects and the right button opens the menu. The scan is only read: the
//! one thing that changes files is Delete permanently, through the add-on's usual dialog, and
//! the Duplicates panel's own delete of extra copies (issue #187).
mod dupes;
mod dupes_ui;
mod layout;
mod model;

use crate::category::{self, Category};
use crate::scanner::{Node, Progress};
use crate::theme::{self, ACCENT, CARD, HAIRLINE, MUTED, TEXT};
use egui::{text::LayoutJob, Color32, Galley, Key, Modifiers, Pos2, Rect, RichText, Sense, Vec2};
use layout::{Camera, Hit, Label, Layout, TileKind};
use model::{Filters, Model, NO_TOP};
use std::{
    collections::HashMap,
    sync::{mpsc, Arc},
};

const MAP_BG: Color32 = toolboks_shared::ui::WINDOW_BG;
const SELECTED_BG: Color32 = toolboks_shared::ui::SELECTED_BG;
/// Accent for text on a card (DESIGN.md: the plain accent is too dark for small text).
const ACCENT_TEXT: Color32 = Color32::from_rgb(0xb3, 0x9d, 0xfa);
/// The "Click to open space explorer" hint: darker than a caption, still 4.6:1 on a card.
pub const HINT: Color32 = Color32::from_rgb(0x7d, 0x81, 0x8c);
const DANGER_TEXT: Color32 = Color32::from_rgb(0xf0, 0x80, 0x7a);
const DANGER_BORDER: Color32 = Color32::from_rgb(0x4a, 0x2c, 0x2b);
const CHIP_BG: Color32 = Color32::from_rgba_premultiplied(12, 13, 14, 214);
const ASIDE: f32 = 272.0;
/// The right panel with Duplicates on: the group rows need the room.
const ASIDE_DUPES: f32 = 340.0;

/// File type colours of design G. The add-on's own category colours are reused where they
/// already match (videos, images, system); every non-empty entry is colourful.
pub fn type_color(c: Category) -> Color32 {
    match c {
        Category::Video => Color32::from_rgb(0xf0, 0xa8, 0x3a),
        Category::Games => Color32::from_rgb(0x7a, 0x58, 0xd6),
        Category::Images => Color32::from_rgb(0x45, 0xb8, 0xf0),
        Category::Archives => Color32::from_rgb(0xe0, 0x80, 0x6a),
        Category::Apps => Color32::from_rgb(0xe8, 0x6f, 0xa8),
        Category::Documents => Color32::from_rgb(0xa6, 0xd8, 0x4f),
        Category::Audio => Color32::from_rgb(0x2f, 0xc4, 0xb2),
        Category::Code => Color32::from_rgb(0xb9, 0xa4, 0xfb),
        Category::System => Color32::from_rgb(0x3e, 0xcf, 0x8e),
        Category::Other => Color32::from_rgb(0x5b, 0x86, 0xe5),
    }
}
/// The legend order of the design.
const TYPE_ORDER: [Category; 10] = [
    Category::Video,
    Category::Games,
    Category::Images,
    Category::Archives,
    Category::Apps,
    Category::Documents,
    Category::Audio,
    Category::Code,
    Category::System,
    Category::Other,
];
const AGE_LABELS: [&str; 6] = [
    "Last 30 days",
    "1 to 6 months",
    "6 to 12 months",
    "1 to 3 years",
    "Over 3 years",
    "No date",
];
fn age_color(bucket: u8) -> Color32 {
    [
        Color32::from_rgb(0x2f, 0xc4, 0xb2),
        Color32::from_rgb(0x45, 0xb8, 0xf0),
        Color32::from_rgb(0xb9, 0xa4, 0xfb),
        Color32::from_rgb(0xf0, 0xa8, 0x3a),
        Color32::from_rgb(0xe0, 0x80, 0x6a),
        Color32::from_rgb(0x5b, 0x86, 0xe5),
    ][bucket.min(5) as usize]
}
fn folder_color(rank: u32) -> Color32 {
    if rank == NO_TOP {
        return type_color(Category::Other);
    }
    type_color(TYPE_ORDER[rank as usize % TYPE_ORDER.len()])
}
fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColorBy {
    #[default]
    Type,
    Age,
    Folder,
}
/// One selected thing on the map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sel {
    Node(u32),
    /// The grouped small files of a folder, as they were drawn when selected.
    Group {
        folder: u32,
        bytes: u64,
        count: u64,
    },
}
/// A legend entry the map is highlighting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pick {
    Type(Category),
    Age(u8),
    Top(u32),
}

/// What the owner set up in the explorer. Kept while it is closed, until the scan changes.
#[derive(Clone)]
struct View {
    generation: u64,
    focus: usize,
    camera: Camera,
    selected: Vec<Sel>,
    color_by: ColorBy,
    highlight: String,
    pick: Option<Pick>,
    /// Duplicates on: the right panel lists duplicate groups and the map colours them.
    dupes: bool,
}
impl View {
    fn new(generation: u64, focus: usize) -> Self {
        Self {
            generation,
            focus,
            camera: Camera::default(),
            selected: vec![],
            color_by: ColorBy::Type,
            highlight: String::new(),
            pick: None,
            dupes: false,
        }
    }
}

/// What the add-on does for the explorer.
pub enum Command {
    /// Back to Overview, with the list on this folder (a path in the scan).
    Close(Option<String>),
    Reveal(String),
    Delete(Box<Node>),
    Scan,
}

/// The scan a model was built from, for the title line.
#[derive(Clone, Default)]
struct Info {
    root: String,
    bytes: u64,
    scanned_at: u64,
    cancelled: bool,
}
struct Job {
    generation: u64,
    filters: Filters,
    entries: usize,
    info: Info,
    rx: mpsc::Receiver<Model>,
}

/// A label's text, width in whole points and colour.
type LabelKey = (String, u32, [u8; 4]);
/// What a layout was made for: model, focus, camera and map size.
type LayoutKey = (u64, usize, [u64; 3], [u32; 2]);
/// Galleys of the labels drawn last frame, so a still map does no text layout at all.
#[derive(Default)]
struct Labels {
    map: HashMap<LabelKey, (Arc<Galley>, u64)>,
    frame: u64,
    ppp: f32,
}
impl Labels {
    fn get(
        &mut self,
        painter: &egui::Painter,
        text: &str,
        width: f32,
        color: Color32,
    ) -> Arc<Galley> {
        let key = (
            text.to_owned(),
            width.max(1.0).floor() as u32,
            color.to_array(),
        );
        if let Some((galley, used)) = self.map.get_mut(&key) {
            *used = self.frame;
            return galley.clone();
        }
        let galley = painter.layout_job(fit_job(text, key.1 as f32, color));
        self.map.insert(key, (galley.clone(), self.frame));
        galley
    }
    fn begin(&mut self, ppp: f32) {
        if ppp != self.ppp {
            self.map.clear();
            self.ppp = ppp;
        }
        let frame = self.frame;
        self.map.retain(|_, (_, used)| *used + 1 >= frame);
        self.frame += 1;
    }
}
/// One line of 11 px text, cut with "…" so it never gets wider than `width`.
pub(crate) fn fit_job(text: &str, width: f32, color: Color32) -> LayoutJob {
    let mut job = LayoutJob::simple(
        text.to_owned(),
        egui::FontId::proportional(11.0),
        color,
        width.max(1.0),
    );
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');
    job
}

#[derive(Default)]
pub struct Explorer {
    pub open: bool,
    view: Option<View>,
    model: Option<Arc<Model>>,
    info: Info,
    job: Option<Job>,
    error: Option<String>,
    hide_below: u64,
    layout: Option<(LayoutKey, Arc<Layout>)>,
    labels: Labels,
    menu: Option<(Sel, Pos2)>,
    focus_map: bool,
    close_requested: bool,
    /// Where the map was drawn, for a menu opened from the keyboard.
    map_origin: Vec2,
    dupes: dupes_ui::Dupes,
    #[cfg(test)]
    map_rect: Option<Rect>,
}

impl Explorer {
    /// Opens the explorer. A new scan starts at `folder`; the same scan keeps the view. While a
    /// scan runs, the last map (if there is one) is shown.
    pub fn open_at(&mut self, folder: usize, generation: u64, running: bool) {
        let generation = match &self.model {
            Some(m) if running => m.generation,
            _ => generation,
        };
        if self
            .view
            .as_ref()
            .is_none_or(|v| v.generation != generation)
        {
            self.view = Some(View::new(generation, folder));
        }
        self.open = true;
        self.menu = None;
        self.focus_map = true;
    }
    /// Frees the last map once a newer scan is in, also while the explorer is closed: a map
    /// holds its whole scan in memory.
    pub fn forget_old(&mut self, p: &Progress) {
        self.dupes.sync(p.generation);
        if !p.running
            && !self.open
            && self
                .model
                .as_ref()
                .is_some_and(|m| m.generation != p.generation)
        {
            self.model = None;
            self.layout = None;
            self.job = None;
        }
    }
    /// A duplicates search or delete is running.
    pub fn busy(&self) -> bool {
        self.dupes.busy()
    }
    /// Duplicates are being deleted: no other delete may start.
    pub fn deleting(&self) -> bool {
        self.dupes.deleting()
    }
    /// The paths a finished duplicates delete removed and the scan they came from, once, for
    /// the add-on to take out of the scan like a single delete.
    pub fn take_deleted(&mut self) -> Option<(u64, Vec<String>)> {
        self.dupes.take_deleted()
    }
    #[cfg(test)]
    pub(crate) fn has_map(&self) -> bool {
        self.model.is_some()
    }
    #[cfg(test)]
    pub(crate) fn zoom_for_test(&mut self, folder: usize) {
        self.zoom_to(folder);
    }
    /// Finds the duplicates in `nodes` (any size), waits, and starts deleting every extra copy.
    /// Returns the number of files the delete will remove.
    #[cfg(test)]
    pub(crate) fn delete_duplicates_for_test(
        &mut self,
        nodes: Arc<Vec<Node>>,
        generation: u64,
        root: &str,
    ) -> u64 {
        let ctx = egui::Context::default();
        self.dupes.min_bytes = 0;
        self.dupes
            .start_search(&ctx, nodes, generation, root.to_owned());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while self.dupes.found.is_none() {
            assert!(std::time::Instant::now() < deadline, "search never ended");
            self.dupes.poll_search();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let f = self.dupes.found.as_ref().unwrap();
        let plan = dupes::plan(&f.groups, None, 0, generation, String::new()).unwrap();
        let files = plan.files();
        self.dupes.start_delete(&ctx, plan, root.to_owned());
        files
    }
    /// Drops the map and everything remembered (the add-on was suspended).
    pub fn reset(&mut self) {
        *self = Self {
            hide_below: self.hide_below,
            ..Default::default()
        };
    }
    /// After a delete patched the scan: keeps the view on the same entries under their new ids.
    pub fn remap(&mut self, map: &[Option<usize>], old: u64, new: u64) {
        self.dupes.remap(map, old, new);
        self.layout = None;
        self.menu = None;
        let Some(view) = self.view.as_mut().filter(|v| v.generation == old) else {
            return;
        };
        let get = |id: u32| map.get(id as usize).copied().flatten().map(|i| i as u32);
        view.generation = new;
        view.focus = get(view.focus as u32).map_or(0, |i| i as usize);
        view.camera = if view.focus == 0 {
            view.camera
        } else {
            Camera::default()
        };
        view.selected = view
            .selected
            .iter()
            .filter_map(|s| match *s {
                Sel::Node(id) => get(id).map(Sel::Node),
                Sel::Group { folder, .. } => get(folder).map(|folder| Sel::Group {
                    folder,
                    bytes: 0,
                    count: 0,
                }),
            })
            .filter(|s| !matches!(s, Sel::Group { bytes: 0, .. }))
            .collect();
        if let Some(Pick::Top(_)) = view.pick {
            view.pick = None;
        }
    }

    fn want(
        &mut self,
        ctx: &egui::Context,
        nodes: &Arc<Vec<Node>>,
        p: &Progress,
        filters: Filters,
    ) {
        if self
            .model
            .as_ref()
            .is_some_and(|m| m.generation == p.generation && m.filters == filters)
            || self
                .job
                .as_ref()
                .is_some_and(|j| j.generation == p.generation && j.filters == filters)
        {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let (nodes_for_job, ctx, generation, now) = (
            nodes.clone(),
            ctx.clone(),
            p.generation,
            crate::scanner::now(),
        );
        match std::thread::Builder::new()
            .name("disk-space-map".into())
            .spawn(move || {
                let _ = tx.send(Model::build(nodes_for_job, generation, filters, now));
                ctx.request_repaint();
            }) {
            Ok(_) => {
                self.error = None;
                self.job = Some(Job {
                    generation,
                    filters,
                    entries: nodes.len(),
                    info: Info {
                        root: crate::display_path(&p.root),
                        bytes: p.bytes,
                        scanned_at: p.scanned_at,
                        cancelled: p.cancelled,
                    },
                    rx,
                });
            }
            Err(e) => self.error = Some(format!("The map could not be prepared: {e}")),
        }
    }
    fn poll(&mut self) {
        let Some(job) = &self.job else { return };
        match job.rx.try_recv() {
            Ok(model) => {
                let job = self.job.take().unwrap();
                self.info = job.info;
                if self
                    .view
                    .as_ref()
                    .is_none_or(|v| v.generation != model.generation)
                {
                    // A new scan: start again from the top of the drive.
                    self.view = Some(View::new(model.generation, 0));
                }
                self.layout = None;
                self.model = Some(Arc::new(model));
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.job = None;
                self.error = Some(
                    "The map could not be prepared. Close the explorer and open it again.".into(),
                );
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }

    /// Draws the explorer over the whole page. `list` is the list's size and age filter;
    /// `modal` is set while the delete dialog is open.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        p: &Progress,
        nodes: &Arc<Vec<Node>>,
        list: (u64, u64),
        modal: bool,
    ) -> Vec<Command> {
        let mut out = vec![];
        let modal_other = modal;
        let filters = Filters {
            min_bytes: list.0,
            older_days: list.1,
            hide_below: self.hide_below,
        };
        if !p.running && p.generation > 0 && !nodes.is_empty() {
            self.want(ui.ctx(), nodes, p, filters);
        }
        self.poll();
        self.dupes.sync(p.generation);
        self.dupes.poll_search();
        if self.dupes.busy() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(150));
        }
        let modal = modal || self.dupes.confirm.is_some();
        let model = self.model.clone().filter(|m| {
            self.view
                .as_ref()
                .is_some_and(|v| v.generation == m.generation)
                && (p.running || m.generation == p.generation)
        });
        let menu_open = self.menu.is_some();
        // Taken before the widgets run: Esc in the highlight box only leaves the box.
        let busy = ui.ctx().text_edit_focused() || egui::Popup::is_any_open(ui.ctx());
        let keys_off = modal || busy;
        self.header(ui, p, model.is_some());
        ui.add_space(theme::SPACE_L);
        let Some(model) = model else {
            self.placeholder(ui, p, &mut out);
            self.global_keys(ui, None, keys_off, menu_open, &mut out);
            return out;
        };
        let view = self.view.as_ref().unwrap();
        let focus = view.focus.min(model.nodes.len() - 1);
        self.view.as_mut().unwrap().focus = focus;
        self.toolbar(ui, &model);
        ui.add_space(theme::SPACE_M);
        let body = ui.available_rect_before_wrap();
        let aside = if self.view().dupes && body.width() >= 1000.0 {
            ASIDE_DUPES
        } else if body.width() >= 760.0 {
            ASIDE
        } else {
            220.0
        };
        let card = Rect::from_min_max(body.min, egui::pos2(body.max.x - aside - 12.0, body.max.y))
            .with_max_y(body.max.y.max(body.min.y + 240.0));
        ui.painter().rect(
            card,
            10,
            CARD,
            egui::Stroke::new(1.0, HAIRLINE),
            egui::StrokeKind::Inside,
        );
        let inner = card.shrink(8.0);
        let map_rect = inner.with_max_y(inner.max.y - 28.0);
        let status_rect = Rect::from_min_max(
            egui::pos2(inner.min.x + 4.0, map_rect.max.y + 8.0),
            inner.max,
        );
        let layout = self.map(ui, &model, map_rect, modal, &mut out);
        self.status(ui, &model, &layout, status_rect);
        let aside_rect = Rect::from_min_max(
            egui::pos2(card.max.x + 12.0, body.min.y),
            egui::pos2(body.max.x, card.max.y),
        );
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(aside_rect));
        let dupes = self.view().dupes;
        egui::ScrollArea::vertical()
            .id_salt(if dupes {
                "space-explorer-dupes"
            } else {
                "space-explorer-aside"
            })
            .show(&mut child, |ui| {
                if dupes {
                    self.dupes_panel(ui, &model, &layout, &p.root, p.running, modal, &mut out);
                } else {
                    self.aside(ui, &model, &layout, &mut out)
                }
            });
        ui.advance_cursor_after_rect(card.union(aside_rect));
        self.global_keys(ui, Some((&model, &layout)), keys_off, menu_open, &mut out);
        let blocked = if p.running {
            Some("Wait for the scan to finish, then try again.")
        } else if modal_other {
            Some("Wait for the other delete to finish.")
        } else {
            None
        };
        self.dupes_confirm(ui.ctx(), &p.root, blocked);
        out
    }

    fn header(&mut self, ui: &mut egui::Ui, p: &Progress, has_map: bool) {
        ui.horizontal(|ui| {
            let back = ui
                .add(theme::outlined("      Overview"))
                .on_hover_text("Back to Overview (Esc)");
            chevron(ui, back.rect);
            back.widget_info(|| {
                egui::WidgetInfo::labeled(egui::Role::Button, true, "Back to Overview")
            });
            if back.clicked() {
                self.close_requested = true;
            }
            ui.add_space(6.0);
            theme::title(ui, "Space explorer");
            ui.add_space(6.0);
            let state = if p.running && has_map {
                "scanning, this map shows the last finished scan".to_owned()
            } else if p.running {
                "scanning".to_owned()
            } else if self.info.cancelled {
                format!("partial scan {}", crate::age(self.info.scanned_at))
            } else {
                format!("scan complete {}", crate::age(self.info.scanned_at))
            };
            let line = if has_map {
                format!(
                    "{} · {state} · {} analyzed · logical sizes, links and junctions left out",
                    self.info.root,
                    crate::size(self.info.bytes)
                )
            } else {
                "Logical sizes, links and junctions left out".to_owned()
            };
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                theme::caption(ui, "Esc closes");
                ui.add(egui::Label::new(RichText::new(line).size(11.0).color(MUTED)).truncate());
            });
        });
    }
}

/// One item the right panel and the actions work on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Item {
    Node(usize),
    Group {
        folder: usize,
        bytes: u64,
        count: u64,
    },
    Many {
        items: usize,
        files: u64,
        bytes: u64,
    },
}

impl Explorer {
    fn view(&self) -> &View {
        self.view
            .as_ref()
            .expect("the explorer has a view while a map is shown")
    }
    fn view_mut(&mut self) -> &mut View {
        self.view
            .as_mut()
            .expect("the explorer has a view while a map is shown")
    }
    /// The deepest folder filling the map, from the last layout.
    fn view_folder(&self) -> usize {
        let focus = self.view().focus;
        self.layout
            .as_ref()
            .filter(|(k, _)| k.1 == focus)
            .map_or(focus, |(_, l)| l.view_folder(focus))
    }
    fn zoom_to(&mut self, folder: usize) {
        let view = self.view_mut();
        view.focus = folder;
        view.camera = Camera::default();
        self.menu = None;
    }
    fn up(&mut self, model: &Model) {
        let current = self.view_folder();
        let parent = model
            .node(current)
            .and_then(|n| n.parent)
            .filter(|&p| p < current);
        self.zoom_to(parent.unwrap_or(0));
    }
    fn layout_for(&mut self, model: &Model, size: Vec2) -> Arc<Layout> {
        let view = self.view();
        let key = (
            model.serial,
            view.focus,
            [
                view.camera.scale.to_bits(),
                view.camera.x.to_bits(),
                view.camera.y.to_bits(),
            ],
            [size.x.to_bits(), size.y.to_bits()],
        );
        if let Some((k, l)) = &self.layout {
            if *k == key {
                return l.clone();
            }
        }
        let l = Arc::new(layout::layout(model, view.focus, &view.camera, size));
        self.layout = Some((key, l.clone()));
        l
    }

    fn placeholder(&mut self, ui: &mut egui::Ui, p: &Progress, out: &mut Vec<Command>) {
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(200.0);
            if let Some(error) = self.error.clone() {
                theme::section(ui, "The map could not be prepared");
                theme::weak(ui, error);
            } else if p.running {
                theme::section(ui, "Scanning...");
                theme::weak(
                    ui,
                    "The map opens when the scan finishes. A full scan clears the previous results to save memory, so there is no older map to show meanwhile.",
                );
                theme::caption(ui, format!("{} files read so far.", count(p.files)));
            } else if let Some(job) = &self.job {
                theme::section(ui, "Preparing the map...");
                theme::weak(
                    ui,
                    format!(
                        "Sorting {} entries by size. A large scan takes a moment.",
                        count(job.entries as u64)
                    ),
                );
            } else {
                theme::section(ui, "Scan a drive first");
                theme::weak(
                    ui,
                    "The space explorer maps your last scan. Scan the drive or folder chosen on Overview to see it here.",
                );
                ui.add_space(theme::SPACE_M);
                if theme::primary(ui, "Scan").clicked() {
                    out.push(Command::Scan);
                }
            }
        });
    }

    fn toolbar(&mut self, ui: &mut egui::Ui, model: &Model) {
        const CONTROLS: f32 = 800.0;
        let crumbs_width = ui.available_width() - CONTROLS - 16.0;
        if crumbs_width >= 160.0 {
            ui.horizontal(|ui| {
                self.crumbs(ui, model, crumbs_width);
                ui.add_space((ui.available_width() - CONTROLS).max(0.0));
                self.controls(ui, model);
            });
        } else {
            let width = ui.available_width();
            ui.horizontal(|ui| self.crumbs(ui, model, width));
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| self.controls(ui, model));
        }
    }
    fn crumbs(&mut self, ui: &mut egui::Ui, model: &Model, width: f32) {
        let path = model.path_to(self.view_folder());
        let mut go = None;
        ui.allocate_ui(Vec2::new(width, 28.0), |ui| {
            egui::ScrollArea::horizontal()
                .id_salt("space-explorer-path")
                .max_width(width)
                .stick_to_right(true)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        for (i, &id) in path.iter().enumerate() {
                            let Some(n) = model.node(id) else { continue };
                            if i > 0 {
                                ui.label(RichText::new("/").color(MUTED));
                            }
                            let text = if i + 1 == path.len() {
                                RichText::new(crate::node_name(n)).color(TEXT).strong()
                            } else {
                                RichText::new(crate::node_name(n)).color(ACCENT_TEXT)
                            };
                            let r = ui
                                .add(egui::Button::new(text).frame(false))
                                .on_hover_text(crate::display_path(&n.path));
                            if r.clicked() {
                                go = Some(id);
                            }
                        }
                    });
                });
        });
        if let Some(id) = go {
            self.zoom_to(id);
        }
    }
    fn controls(&mut self, ui: &mut egui::Ui, model: &Model) {
        ui.spacing_mut().item_spacing.x = 8.0;
        let view = self.view_mut();
        ui.add(
            egui::TextEdit::singleline(&mut view.highlight)
                .id_salt("space-explorer-highlight")
                .hint_text("Highlight name or extension")
                .desired_width(210.0),
        )
        .on_hover_text("Matching files stay bright and the rest dims. Nothing is hidden.");
        let mut hide = self.hide_below;
        egui::ComboBox::from_id_salt("space-explorer-hide")
            .selected_text(hide_label(hide))
            .show_ui(ui, |ui| {
                for v in [0, 100 << 20, 1 << 30] {
                    ui.selectable_value(&mut hide, v, hide_label(v));
                }
            });
        self.hide_below = hide;
        let view = self.view_mut();
        egui::Frame::new()
            .fill(MAP_BG)
            .stroke(egui::Stroke::new(1.0, HAIRLINE))
            .corner_radius(8)
            .inner_margin(2)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                for (mode, label) in [
                    (ColorBy::Type, "File type"),
                    (ColorBy::Age, "Age"),
                    (ColorBy::Folder, "Folder"),
                ] {
                    let on = view.color_by == mode && !view.dupes;
                    let r = ui.add(
                        egui::Button::new(if on {
                            RichText::new(label).color(TEXT).strong()
                        } else {
                            RichText::new(label).color(MUTED)
                        })
                        .fill(if on {
                            SELECTED_BG
                        } else {
                            Color32::TRANSPARENT
                        })
                        .stroke(egui::Stroke::NONE)
                        .corner_radius(6)
                        .min_size(Vec2::new(0.0, 24.0)),
                    );
                    r.widget_info(|| {
                        egui::WidgetInfo::selected(
                            egui::Role::Button,
                            true,
                            on,
                            format!("Colour by {}", label.to_lowercase()),
                        )
                    });
                    if r.clicked() && !on {
                        view.color_by = mode;
                        view.dupes = false;
                        if !matches!(view.pick, Some(Pick::Type(_))) {
                            view.pick = None;
                        }
                    }
                }
            });
        let view = self.view_mut();
        let on = view.dupes;
        let r = ui
            .add(
                theme::outlined("Duplicates")
                    .fill(if on {
                        SELECTED_BG
                    } else {
                        Color32::TRANSPARENT
                    })
                    .stroke(egui::Stroke::new(
                        1.0,
                        if on { ACCENT } else { theme::CONTROL_BORDER },
                    )),
            )
            .on_hover_text(if on {
                "Back to the usual colours and panel"
            } else {
                "Find files with the same content, sort them and delete extra copies"
            });
        r.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, true, on, "Duplicates"));
        if r.clicked() {
            view.dupes = !on;
            view.pick = None;
        }
        let at_top = self.view().focus == 0 && self.view().camera == Camera::default();
        let target = self
            .selected_folder(model)
            .filter(|&f| f != self.view_folder());
        ui.spacing_mut().item_spacing.x = 4.0;
        if icon_button(ui, !at_top, "Zoom out one level", false).clicked() {
            self.up(model);
        }
        if ui
            .add_enabled(!at_top, theme::outlined("Fit"))
            .on_hover_text("Show the whole scan (Home)")
            .clicked()
        {
            self.zoom_to(0);
        }
        if icon_button(ui, target.is_some(), "Zoom in to the selected folder", true).clicked() {
            if let Some(f) = target {
                self.zoom_to(f);
            }
        }
        let view = self.view();
        let root = model.nodes[0].bytes.max(1) as f64;
        let here = model.nodes[view.focus].bytes.max(1) as f64;
        ui.add_sized(
            Vec2::new(46.0, 28.0),
            egui::Label::new(
                RichText::new(zoom_text(root / here * view.camera.scale)).color(MUTED),
            ),
        );
    }
    /// The folder the plus button and Enter zoom to: the selected folder, or a file's folder.
    fn selected_folder(&self, model: &Model) -> Option<usize> {
        match self.view().selected.first()? {
            Sel::Node(id) => {
                let n = model.node(*id as usize)?;
                if n.dir {
                    Some(n.id)
                } else {
                    n.parent
                }
            }
            Sel::Group { folder, .. } => Some(*folder as usize),
        }
    }

    fn map(
        &mut self,
        ui: &mut egui::Ui,
        model: &Arc<Model>,
        rect: Rect,
        modal: bool,
        out: &mut Vec<Command>,
    ) -> Arc<Layout> {
        let id = ui.scope_id().with("space-explorer-map");
        self.map_origin = rect.min.to_vec2();
        #[cfg(test)]
        {
            self.map_rect = Some(rect);
        }
        let response = ui.interact(rect, id, Sense::click_and_drag());
        if std::mem::take(&mut self.focus_map) {
            response.request_focus();
        }
        if response.has_focus() {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    id,
                    egui::EventFilter {
                        tab: false,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                )
            });
        }
        let size = rect.size();
        let origin = rect.min.to_vec2();
        let pointer = ui
            .input(|i| i.pointer.hover_pos())
            .filter(|p| rect.contains(*p) && response.contains_pointer());
        {
            let max = Camera::max_scale(model.shown[self.view().focus]);
            let menu = self.menu.is_some();
            let view = self.view_mut();
            if let Some(at) = pointer.filter(|_| !modal && !menu) {
                let factor = ui.input(|i| wheel_factor(&i.events, size.y));
                if factor != 1.0 {
                    view.camera.zoom_at(factor, at - origin, size, max);
                }
            }
            // Only the middle button (the held wheel) pans; left selects, right opens the menu.
            if response.dragged_by(egui::PointerButton::Middle) {
                view.camera.pan(ui.input(|i| i.pointer.delta()), size, max);
            }
        }
        let grabbing = response.is_pointer_button_down_on()
            && ui.input(|i| i.pointer.button_down(egui::PointerButton::Middle));
        if grabbing {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        }
        let layout = self.layout_for(model, size);
        self.labels.begin(ui.ctx().pixels_per_point());
        let painter = ui.painter_at(rect);
        let chips = self.chips(&painter, model, &layout, rect);
        // Enter and Space "click" a focused widget in egui; here they have their own meaning,
        // so only a real pointer click selects.
        let pointer_click = ui.input(|i| {
            i.events
                .iter()
                .any(|e| matches!(e, egui::Event::PointerButton { .. }))
        });
        if !modal && pointer_click && (response.clicked() || response.secondary_clicked()) {
            response.request_focus();
            let pos = response.interact_pointer_pos().unwrap_or(rect.center());
            // The host sends modifiers with each event, not as the frame's key state.
            let add = ui.input(|i| {
                i.modifiers.command
                    || i.modifiers.ctrl
                    || i.events.iter().any(|e| {
                        matches!(e, egui::Event::PointerButton { modifiers, .. }
                            if modifiers.ctrl || modifiers.command)
                    })
            });
            let chip = chips.iter().find(|c| c.0.contains(pos)).map(|c| c.1);
            let hit = layout.hit(pos - origin).and_then(|h| sel_of(&layout, h));
            if response.clicked() {
                self.menu = None;
                match chip {
                    Some(folder) => self.zoom_to(folder as usize),
                    None => self.select(hit, add),
                }
                if self.view().dupes {
                    self.dupes_follow_selection();
                }
            } else if let Some(sel) = hit.or(chip.map(Sel::Node)) {
                if !self.view().selected.contains(&sel) {
                    self.view_mut().selected = vec![sel];
                }
                self.menu = Some((sel, pos));
            }
        }
        // Tiles: flat colours with a thin dark gap.
        painter.rect_filled(rect, 6, MAP_BG);
        let view = self.view().clone();
        let query = view.highlight.trim().to_lowercase();
        let dimming = !query.is_empty() || view.pick.is_some();
        let mut memo = HashMap::new();
        let mut fills = Vec::with_capacity(layout.tiles.len());
        for t in &layout.tiles {
            let r = t.rect.translate(origin);
            let bright = !dimming || matches(model, t, &query, view.pick, &mut memo);
            let mut fill = match t.kind {
                _ if view.dupes => self.dupes_fill(t.kind),
                TileKind::Hidden { .. } => SELECTED_BG,
                TileKind::Group { .. } => mix(tile_color(model, t, view.color_by), MAP_BG, 0.45),
                TileKind::File(_) => tile_color(model, t, view.color_by),
            };
            if !bright {
                fill = mix(fill, MAP_BG, 0.72);
            }
            fills.push(fill);
            if r.width() <= 0.0 || r.height() <= 0.0 || !r.intersects(rect) {
                continue;
            }
            let draw = if r.width() > 1.6 && r.height() > 1.6 {
                r.shrink(0.4)
            } else {
                r
            };
            let radius = if draw.width().min(draw.height()) >= 6.0 {
                2
            } else {
                0
            };
            painter.rect_filled(draw, radius, fill);
        }
        // Names: only on tiles big enough, on screen and not under a folder chip.
        for (t, fill) in layout.tiles.iter().zip(&fills) {
            let r = t.rect.translate(origin);
            if !r.intersects(rect) {
                continue;
            }
            let clip = painter.with_clip_rect(r.shrink(1.0).intersect(rect));
            if let TileKind::Hidden { files } = t.kind {
                if r.width() >= 120.0 && r.height() >= 16.0 {
                    let text = format!(
                        "Hidden by filters · {} in {} files",
                        crate::size(t.bytes),
                        count(files)
                    );
                    let g = self.labels.get(&painter, &text, r.width() - 12.0, MUTED);
                    let shown = r.intersect(rect);
                    clip.galley(
                        egui::pos2(shown.left() + 6.0, shown.center().y - g.size().y / 2.0),
                        g,
                        MUTED,
                    );
                }
                continue;
            }
            let label = layout::tile_label(r.size());
            if label == Label::None {
                continue;
            }
            let line = Rect::from_min_size(r.min, Vec2::new(r.width(), 18.0));
            if chips.iter().any(|c| c.0.intersects(line)) {
                continue;
            }
            let ink = category::text(*fill);
            let name = match t.kind {
                TileKind::File(id) => model.nodes[id as usize].name.clone(),
                TileKind::Group { count: n, .. } => format!("{} small files", count(n)),
                TileKind::Hidden { .. } => continue,
            };
            let g = self.labels.get(&painter, &name, r.width() - 10.0, ink);
            clip.galley(r.min + Vec2::new(5.0, 3.0), g, ink);
            if label == Label::NameAndSize {
                let g = self
                    .labels
                    .get(&painter, &crate::size(t.bytes), r.width() - 10.0, ink);
                clip.galley(r.min + Vec2::new(5.0, 17.0), g, ink);
            }
        }
        // Folder chips: name and size, a click target that zooms.
        for (chip, _, galley) in &chips {
            painter.rect_filled(*chip, 5, CHIP_BG);
            painter.galley(
                egui::pos2(chip.left() + 7.0, chip.center().y - galley.size().y / 2.0),
                galley.clone(),
                TEXT,
            );
        }
        let hovered = pointer.and_then(|p| layout.hit(p - origin));
        if let (Some(Hit::Tile(i)), false) = (hovered, grabbing) {
            painter.rect_stroke(
                layout.tiles[i].rect.translate(origin),
                2,
                egui::Stroke::new(1.0, Color32::from_white_alpha(150)),
                egui::StrokeKind::Inside,
            );
        }
        if chips
            .iter()
            .any(|c| pointer.is_some_and(|p| c.0.contains(p)))
        {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        // Selection: a white line on a dark halo, readable on every colour.
        for sel in &view.selected {
            let r = match sel {
                Sel::Node(id) => layout
                    .find(TileKind::File(*id))
                    .map(|i| layout.tiles[i].rect)
                    .or_else(|| layout.blocks.iter().find(|b| b.id == *id).map(|b| b.rect)),
                Sel::Group { .. } => tile_of(&layout, *sel).map(|i| layout.tiles[i].rect),
            };
            if let Some(r) = r.map(|r| r.translate(origin)) {
                painter.rect_stroke(
                    r,
                    3,
                    egui::Stroke::new(4.0, MAP_BG),
                    egui::StrokeKind::Middle,
                );
                painter.rect_stroke(
                    r.shrink(1.0),
                    2,
                    egui::Stroke::new(2.0, Color32::WHITE),
                    egui::StrokeKind::Middle,
                );
            }
        }
        if layout.tiles.is_empty() {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                if model.empty_files > 0 {
                    "Everything here is empty (0 bytes). Empty files get no tile."
                } else {
                    "Nothing to draw here."
                },
                egui::FontId::proportional(12.0),
                MUTED,
            );
        }
        if response.has_focus() {
            painter.rect_stroke(
                rect,
                6,
                egui::Stroke::new(2.0, ACCENT),
                egui::StrokeKind::Inside,
            );
        }
        let vf = layout.view_folder(view.focus);
        let label = format!(
            "Space map of {}. {} tiles, each sized by its logical size. Selected: {}. Arrow keys move between tiles, Enter zooms in, Backspace zooms out, the menu key opens actions. The Overview list reads every item.",
            model
                .node(vf)
                .map_or_else(String::new, |n| crate::display_path(&n.path)),
            count(layout.tiles.len() as u64),
            self.summary(model).0
        );
        response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Image, true, &label));
        if let (Some(hit), false, None, false) = (hovered, grabbing, self.menu, modal) {
            let scan = self.info.bytes;
            response.on_hover_ui_at_pointer(|ui| tooltip(ui, model, &layout, hit, scan));
        }
        self.menu_ui(ui, id, model, out);
        layout
    }
    fn select(&mut self, sel: Option<Sel>, add: bool) {
        let view = self.view_mut();
        match (sel, add) {
            (Some(s), true) => {
                if let Some(i) = view.selected.iter().position(|x| *x == s) {
                    view.selected.remove(i);
                } else {
                    view.selected.push(s);
                }
            }
            (sel, false) => view.selected = sel.into_iter().collect(),
            (None, true) => {}
        }
    }
    fn chips(
        &mut self,
        painter: &egui::Painter,
        model: &Model,
        layout: &Layout,
        rect: Rect,
    ) -> Vec<(Rect, u32, Arc<Galley>)> {
        let mut out = vec![];
        for b in &layout.blocks {
            if !layout::chip_fits(b.depth, b.rect.size()) || out.len() >= 300 {
                continue;
            }
            let r = b.rect.translate(rect.min.to_vec2());
            let room = r.width() - 24.0;
            let Some(n) = model.node(b.id as usize) else {
                continue;
            };
            let full = format!("{} · {}", n.name, crate::size(model.shown[b.id as usize]));
            let mut g = self.labels.get(painter, &full, 4000.0, TEXT);
            if g.size().x > room {
                g = self.labels.get(painter, &n.name, room, TEXT);
            }
            let chip =
                Rect::from_min_size(r.min + Vec2::splat(5.0), Vec2::new(g.size().x + 14.0, 20.0));
            if chip.intersects(rect) {
                out.push((chip, b.id, g));
            }
        }
        out
    }
    fn menu_ui(&mut self, ui: &mut egui::Ui, id: egui::Id, model: &Model, out: &mut Vec<Command>) {
        let Some((sel, pos)) = self.menu else { return };
        let mut open = true;
        let mut act = None;
        let is_node = matches!(sel, Sel::Node(_));
        let has_copies = matches!(sel, Sel::Node(id) if self.dupes.group_of(id).is_some());
        egui::Popup::new(id.with("menu"), ui.ctx().clone(), pos, ui.layer_id())
            .kind(egui::PopupKind::Menu)
            .open_bool(&mut open)
            .show(|ui| {
                ui.set_min_width(196.0);
                for (a, label, keys) in [
                    (Act::Reveal, "Show in Explorer", "Ctrl+E"),
                    (Act::Copy, "Copy path", "Ctrl+C"),
                    (Act::Zoom, "Zoom to this folder", "Enter"),
                    (Act::Highlight, "Highlight this file type", ""),
                ] {
                    if ui
                        .add(egui::Button::new(label).shortcut_text(keys))
                        .clicked()
                    {
                        act = Some(a);
                        ui.close();
                    }
                }
                if has_copies && ui.button("Select its other copies").clicked() {
                    act = Some(Act::OtherCopies);
                    ui.close();
                }
                ui.separator();
                let delete = ui
                    .add_enabled(
                        is_node,
                        egui::Button::new(crate::delete::menu_label()).shortcut_text("Shift+Del"),
                    )
                    .on_disabled_hover_text(
                        "Grouped small files are deleted one by one: zoom in first.",
                    );
                if delete.clicked() {
                    act = Some(Act::Delete);
                    ui.close();
                }
            });
        if !open {
            self.menu = None;
            self.focus_map = true;
        }
        if let Some(a) = act {
            self.menu = None;
            self.focus_map = true;
            self.act(a, &[sel], model, ui.ctx(), out);
        }
    }
    fn act(
        &mut self,
        a: Act,
        sels: &[Sel],
        model: &Model,
        ctx: &egui::Context,
        out: &mut Vec<Command>,
    ) {
        let target = |s: &Sel| match s {
            Sel::Node(id) => *id as usize,
            Sel::Group { folder, .. } => *folder as usize,
        };
        let Some(first) = sels.first() else { return };
        match a {
            Act::Reveal if sels.len() == 1 => {
                if let Some(n) = model.node(target(first)) {
                    out.push(Command::Reveal(n.path.clone()));
                }
            }
            Act::Copy => {
                let all: Vec<String> = sels
                    .iter()
                    .filter_map(|s| model.node(target(s)))
                    .map(|n| crate::display_path(&n.path))
                    .collect();
                if !all.is_empty() {
                    ctx.copy_text(all.join("\r\n"));
                }
            }
            Act::Zoom => {
                let folder = match first {
                    Sel::Node(id) => {
                        model
                            .node(*id as usize)
                            .and_then(|n| if n.dir { Some(n.id) } else { n.parent })
                    }
                    Sel::Group { folder, .. } => Some(*folder as usize),
                };
                if let Some(f) = folder {
                    self.zoom_to(f);
                }
            }
            Act::Highlight => {
                let c = model.category(target(first));
                self.view_mut().pick = Some(Pick::Type(c));
            }
            Act::OtherCopies => {
                if let Sel::Node(id) = first {
                    let others: Vec<Sel> = self
                        .dupes
                        .copies_of(*id)
                        .into_iter()
                        .filter(|c| c != id)
                        .map(Sel::Node)
                        .collect();
                    if !others.is_empty() {
                        self.view_mut().selected = others;
                    }
                }
            }
            Act::Delete if sels.len() == 1 => {
                if let Sel::Node(id) = first {
                    if let Some(n) = model.node(*id as usize) {
                        out.push(Command::Delete(Box::new(n.clone())));
                    }
                }
            }
            _ => {}
        }
    }
    /// The selected item, or the folder in view when nothing is selected.
    fn item(&self, model: &Model) -> Item {
        match self.view().selected.as_slice() {
            [] => Item::Node(self.view_folder()),
            [Sel::Node(id)] => Item::Node(*id as usize),
            [Sel::Group {
                folder,
                bytes,
                count,
            }] => Item::Group {
                folder: *folder as usize,
                bytes: *bytes,
                count: *count,
            },
            many => {
                let (files, bytes) = selection_total(model, many);
                Item::Many {
                    items: many.len(),
                    files,
                    bytes,
                }
            }
        }
    }
    /// "Selected: ..." and the path beside it.
    fn summary(&self, model: &Model) -> (String, String) {
        if self.view().selected.is_empty() {
            let vf = self.view_folder();
            return (
                "nothing".into(),
                model
                    .node(vf)
                    .map_or_else(String::new, |n| crate::display_path(&n.path)),
            );
        }
        match self.item(model) {
            Item::Node(id) => {
                let Some(n) = model.node(id) else {
                    return ("nothing".into(), String::new());
                };
                let what = if n.dir {
                    format!(
                        "folder · {} files · {}",
                        count(n.files),
                        crate::size(n.bytes)
                    )
                } else {
                    format!("1 file · {}", crate::size(n.bytes))
                };
                (what, crate::display_path(&n.path))
            }
            Item::Group {
                folder,
                bytes,
                count: n,
            } => (
                format!("{} small files · {}", count(n), crate::size(bytes)),
                format!(
                    "in {}",
                    model
                        .node(folder)
                        .map_or_else(String::new, |n| crate::display_path(&n.path))
                ),
            ),
            Item::Many {
                items,
                files,
                bytes,
            } => (
                format!(
                    "{items} items · {} files · {}",
                    count(files),
                    crate::size(bytes)
                ),
                "Ctrl+click adds or removes items.".into(),
            ),
        }
    }
    fn status(&self, ui: &mut egui::Ui, model: &Model, layout: &Layout, rect: Rect) {
        let (what, path) = self.summary(model);
        let mut ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        ui.label(RichText::new("Selected:").strong());
        ui.label(what);
        ui.add_space(12.0);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let mut tiles = format!(
                "{} tiles drawn · small files grouped",
                count(layout.tiles.len() as u64)
            );
            if layout.capped {
                tiles.push_str(" · tile limit reached");
            }
            theme::caption(ui, tiles);
            ui.add_space(12.0);
            ui.add(egui::Label::new(RichText::new(path).color(MUTED)).truncate());
        });
    }

    fn aside(&mut self, ui: &mut egui::Ui, model: &Model, layout: &Layout, out: &mut Vec<Command>) {
        let width = ui.available_width();
        let view = self.view().clone();
        // The legend: what is on screen, by the current colour.
        let mut sums: Vec<(Pick, u64)> = vec![];
        for t in &layout.tiles {
            if matches!(t.kind, TileKind::Hidden { .. }) {
                continue;
            }
            let key = pick_of(model, t, view.color_by);
            match sums.iter_mut().find(|(k, _)| *k == key) {
                Some((_, b)) => *b += t.bytes,
                None => sums.push((key, t.bytes)),
            }
        }
        sums.sort_by_key(|s| std::cmp::Reverse(s.1));
        let total: u64 = sums.iter().map(|s| s.1).sum::<u64>().max(1);
        let mut toggle = None;
        aside_card(ui, width, |ui| {
            theme::section(
                ui,
                match view.color_by {
                    ColorBy::Type => "File types in view",
                    ColorBy::Age => "Ages in view",
                    ColorBy::Folder => "Top folders in view",
                },
            );
            if sums.is_empty() {
                theme::caption(ui, "Nothing drawn here.");
            }
            for (key, bytes) in sums.iter().take(10) {
                let on = view.pick == Some(*key);
                let name = pick_label(model, *key);
                let r = list_row(
                    ui,
                    pick_color(*key),
                    &name,
                    *bytes,
                    *bytes as f64 / total as f64,
                    on,
                    view.pick.is_some() && !on,
                );
                r.widget_info(|| {
                    egui::WidgetInfo::selected(
                        egui::Role::Button,
                        true,
                        on,
                        format!("Highlight {name}"),
                    )
                });
                if r.clicked() {
                    toggle = Some(*key);
                }
            }
            theme::caption(
                ui,
                match view.color_by {
                    ColorBy::Type => "Click a type to highlight it. Everything else dims.",
                    ColorBy::Age => "Click an age to highlight it. Everything else dims.",
                    ColorBy::Folder => "Click a folder to highlight it. Everything else dims.",
                },
            );
        });
        if let Some(key) = toggle {
            let v = self.view_mut();
            v.pick = if v.pick == Some(key) { None } else { Some(key) };
        }
        ui.add_space(12.0);
        // Largest in view: the biggest entries of the folder that fills the map.
        let vf = layout.view_folder(view.focus);
        let here = model.shown[vf].max(1);
        let mut go = None;
        aside_card(ui, width, |ui| {
            theme::section(ui, "Largest in view");
            let kids = model.children(vf);
            if kids.is_empty() {
                theme::caption(ui, "Nothing drawn here.");
            }
            for &k in kids.iter().take(6) {
                let k = k as usize;
                let n = &model.nodes[k];
                let color = match view.color_by {
                    ColorBy::Type => type_color(model.category(k)),
                    ColorBy::Age => age_color(model.age_bucket(k)),
                    ColorBy::Folder => folder_color(model.top[k]),
                };
                let r = list_row(
                    ui,
                    color,
                    &n.name,
                    model.shown[k],
                    model.shown[k] as f64 / here as f64,
                    false,
                    false,
                );
                let name = if n.dir {
                    format!("Zoom to {}", n.name)
                } else {
                    format!("Select {}", n.name)
                };
                r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, &name));
                if r.clicked() {
                    go = Some(k);
                }
            }
        });
        if let Some(k) = go {
            if model.nodes[k].dir {
                self.zoom_to(k);
            } else {
                self.view_mut().selected = vec![Sel::Node(k as u32)];
            }
        }
        ui.add_space(12.0);
        self.details(ui, model, width, out);
    }
    fn details(&mut self, ui: &mut egui::Ui, model: &Model, width: f32, out: &mut Vec<Command>) {
        let item = self.item(model);
        let scan = self.info.bytes.max(1) as f64;
        let (name, kind, bytes, kind_label, modified) = match item {
            Item::Node(id) => {
                let n = &model.nodes[id];
                let parent = n
                    .parent
                    .and_then(|p| model.node(p))
                    .map_or_else(String::new, crate::node_name);
                (
                    crate::node_name(n),
                    if n.dir {
                        format!("Folder · {} files", count(n.files))
                    } else {
                        format!("File in {parent}")
                    },
                    n.bytes,
                    if n.dir {
                        "Folder".to_owned()
                    } else {
                        model.category(id).label().to_owned()
                    },
                    date(n.modified),
                )
            }
            Item::Group {
                folder,
                bytes,
                count: c,
            } => {
                let f = &model.nodes[folder];
                (
                    format!("{} small files", count(c)),
                    format!("Small files grouped in {}", crate::node_name(f)),
                    bytes,
                    "Small files".to_owned(),
                    date(f.modified),
                )
            }
            Item::Many {
                items,
                files,
                bytes,
            } => (
                format!("{items} items selected"),
                format!("{} files in all", count(files)),
                bytes,
                "Several".to_owned(),
                "Several".to_owned(),
            ),
        };
        let mut act = None;
        aside_card(ui, width, |ui| {
            ui.add(egui::Label::new(RichText::new(name).size(14.0).strong()).truncate());
            theme::caption(ui, kind);
            ui.add_space(6.0);
            egui::Grid::new("space-explorer-detail")
                .num_columns(2)
                .spacing([12.0, 6.0])
                .min_col_width((ui.available_width() - 12.0) / 2.0)
                .show(ui, |ui| {
                    for pair in [
                        [
                            ("Size", crate::size(bytes)),
                            ("Of scan", format!("{:.2}%", bytes as f64 / scan * 100.0)),
                        ],
                        [("Type", kind_label), ("Modified", modified)],
                    ] {
                        for (label, value) in pair {
                            ui.vertical(|ui| {
                                theme::caption(ui, label);
                                ui.label(RichText::new(value).strong());
                            });
                        }
                        ui.end_row();
                    }
                });
            ui.add_space(8.0);
            let single = !matches!(item, Item::Many { .. });
            let full = ui.available_width();
            let reveal = ui.add_enabled(
                single,
                egui::Button::new(RichText::new("Show in Explorer").color(TEXT).strong())
                    .fill(toolboks_shared::ui::HOST_ACCENT_FILL)
                    .corner_radius(6)
                    .min_size(Vec2::new(full, 28.0)),
            );
            if reveal.clicked() {
                act = Some(Act::Reveal);
            }
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let half = (full - 6.0) / 2.0;
                if ui
                    .add(theme::outlined("Copy path").min_size(Vec2::new(half, 28.0)))
                    .clicked()
                {
                    act = Some(Act::Copy);
                }
                let delete = ui
                    .add_enabled(
                        matches!(item, Item::Node(_)),
                        egui::Button::new(RichText::new("Delete permanently").color(DANGER_TEXT))
                            .fill(Color32::TRANSPARENT)
                            .stroke(egui::Stroke::new(1.0, DANGER_BORDER))
                            .corner_radius(6)
                            .min_size(Vec2::new(half, 28.0)),
                    )
                    .on_disabled_hover_text("Select one file or folder to delete it.");
                if delete.clicked() {
                    act = Some(Act::Delete);
                }
            });
        });
        if let Some(a) = act {
            let sels: Vec<Sel> = match item {
                Item::Node(id) => vec![Sel::Node(id as u32)],
                _ => self.view().selected.clone(),
            };
            self.act(a, &sels, model, ui.ctx(), out);
        }
    }

    fn global_keys(
        &mut self,
        ui: &mut egui::Ui,
        map: Option<(&Model, &Layout)>,
        modal: bool,
        menu_open: bool,
        out: &mut Vec<Command>,
    ) {
        let close_path = |s: &Self| {
            map.and_then(|(m, l)| {
                let vf = l.view_folder(s.view.as_ref()?.focus);
                m.node(vf).map(|n| n.path.clone())
            })
        };
        if std::mem::take(&mut self.close_requested) {
            out.push(Command::Close(close_path(self)));
            return;
        }
        let ctx = ui.ctx().clone();
        if modal || ctx.text_edit_focused() || egui::Popup::is_any_open(&ctx) {
            return;
        }
        if !menu_open
            && self.menu.is_none()
            && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
        {
            out.push(Command::Close(close_path(self)));
            return;
        }
        let Some((model, layout)) = map else { return };
        if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Backspace)) {
            self.up(model);
        }
        if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Home)) {
            self.zoom_to(0);
        }
        let map_id = ui.scope_id().with("space-explorer-map");
        if !ctx.memory(|m| m.focused().is_none_or(|f| f == map_id)) {
            return;
        }
        if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
            if let Some(f) = self.selected_folder(model) {
                self.zoom_to(f);
            }
        }
        let max = Camera::max_scale(model.shown[self.view().focus]);
        for (key, dir) in [
            (Key::ArrowLeft, -Vec2::X),
            (Key::ArrowRight, Vec2::X),
            (Key::ArrowUp, -Vec2::Y),
            (Key::ArrowDown, Vec2::Y),
        ] {
            if ui.input_mut(|i| i.consume_key(Modifiers::SHIFT, key)) {
                let step = -dir * layout.view * 0.1;
                self.view_mut().camera.pan(step, layout.view, max);
            }
            if ui.input_mut(|i| i.consume_key(Modifiers::NONE, key)) {
                let current = self
                    .view()
                    .selected
                    .first()
                    .and_then(|s| tile_of(layout, *s));
                let next = match current {
                    Some(i) => layout.neighbour(i, dir),
                    None => match layout.hit((layout.view / 2.0).to_pos2()) {
                        Some(Hit::Tile(i)) => Some(i),
                        _ => (!layout.tiles.is_empty()).then_some(0),
                    },
                };
                if let Some(sel) = next.and_then(|i| sel_of(layout, Hit::Tile(i))) {
                    self.view_mut().selected = vec![sel];
                }
            }
        }
        let sels = self.view().selected.clone();
        if ui.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::E)) {
            self.act(Act::Reveal, &sels, model, &ctx, out);
        }
        if ui.input_mut(|i| i.consume_key(Modifiers::SHIFT, Key::Delete)) {
            self.act(Act::Delete, &sels, model, &ctx, out);
        }
        let copy = ui.input_mut(|i| {
            let before = i.events.len();
            i.events.retain(|e| !matches!(e, egui::Event::Copy));
            before != i.events.len()
        });
        if copy {
            self.act(Act::Copy, &sels, model, &ctx, out);
        }
        let menu_key = ui.input_mut(|i| {
            i.consume_key(Modifiers::SHIFT, Key::F10) || i.consume_key(Modifiers::NONE, Key::Menu)
        });
        if let (true, Some(&sel)) = (menu_key, sels.first()) {
            let at = tile_of(layout, sel)
                .map(|i| layout.tiles[i].rect.center())
                .unwrap_or((layout.view / 2.0).to_pos2());
            self.menu = Some((sel, at + self.map_origin));
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Act {
    Reveal,
    Copy,
    Zoom,
    Highlight,
    OtherCopies,
    Delete,
}

fn sel_of(layout: &Layout, hit: Hit) -> Option<Sel> {
    match hit {
        Hit::Folder(id) => Some(Sel::Node(id)),
        Hit::Tile(i) => match layout.tiles.get(i)?.kind {
            TileKind::File(id) => Some(Sel::Node(id)),
            TileKind::Group { folder, count } => Some(Sel::Group {
                folder,
                bytes: layout.tiles[i].bytes,
                count,
            }),
            TileKind::Hidden { .. } => None,
        },
    }
}
fn tile_of(layout: &Layout, sel: Sel) -> Option<usize> {
    match sel {
        Sel::Node(id) => layout.find(TileKind::File(id)),
        Sel::Group { folder, .. } => layout
            .tiles
            .iter()
            .position(|t| matches!(t.kind, TileKind::Group { folder: f, .. } if f == folder)),
    }
}
/// Files and bytes of a selection; an entry inside a selected folder is not counted twice.
fn selection_total(model: &Model, sels: &[Sel]) -> (u64, u64) {
    let picked: std::collections::HashSet<usize> = sels
        .iter()
        .filter_map(|s| match s {
            Sel::Node(id) => Some(*id as usize),
            _ => None,
        })
        .collect();
    let under_picked = |start: Option<usize>| {
        let mut next = start;
        while let Some(p) = next {
            if picked.contains(&p) {
                return true;
            }
            next = model.node(p).and_then(|n| n.parent).filter(|&q| q < p);
        }
        false
    };
    let (mut files, mut bytes) = (0u64, 0u64);
    for s in sels {
        match *s {
            Sel::Node(id) => {
                let Some(n) = model.node(id as usize) else {
                    continue;
                };
                if under_picked(n.parent.filter(|&p| p < n.id)) {
                    continue;
                }
                files += if n.dir { n.files } else { 1 };
                bytes = bytes.saturating_add(n.bytes);
            }
            Sel::Group {
                folder,
                bytes: b,
                count,
            } => {
                if under_picked(Some(folder as usize)) {
                    continue;
                }
                files += count;
                bytes = bytes.saturating_add(b);
            }
        }
    }
    (files, bytes)
}
fn pick_of(model: &Model, t: &layout::Tile, by: ColorBy) -> Pick {
    let id = match t.kind {
        TileKind::File(id) => id as usize,
        TileKind::Group { folder, .. } => folder as usize,
        TileKind::Hidden { .. } => 0,
    };
    match by {
        ColorBy::Type => Pick::Type(model.category(id)),
        ColorBy::Age => Pick::Age(model.age_bucket(id)),
        ColorBy::Folder => Pick::Top(model.top.get(id).copied().unwrap_or(NO_TOP)),
    }
}
fn pick_color(p: Pick) -> Color32 {
    match p {
        Pick::Type(c) => type_color(c),
        Pick::Age(a) => age_color(a),
        Pick::Top(r) => folder_color(r),
    }
}
fn pick_label(model: &Model, p: Pick) -> String {
    match p {
        Pick::Type(c) => c.label().to_owned(),
        Pick::Age(a) => AGE_LABELS[a.min(5) as usize].to_owned(),
        Pick::Top(NO_TOP) => "Files at the top".to_owned(),
        Pick::Top(r) => model
            .top_ids
            .get(r as usize)
            .and_then(|id| model.node(*id as usize))
            .map_or_else(String::new, |n| n.name.clone()),
    }
}
fn tile_color(model: &Model, t: &layout::Tile, by: ColorBy) -> Color32 {
    pick_color(pick_of(model, t, by))
}
/// Whether a tile stays bright: it matches the highlight text (its name or a folder above it)
/// and the legend entry picked.
fn matches(
    model: &Model,
    t: &layout::Tile,
    query: &str,
    pick: Option<Pick>,
    memo: &mut HashMap<usize, bool>,
) -> bool {
    let (id, folder) = match t.kind {
        TileKind::File(id) => (Some(id as usize), model.nodes[id as usize].parent),
        TileKind::Group { folder, .. } => (None, Some(folder as usize)),
        TileKind::Hidden { .. } => return false,
    };
    if let Some(p) = pick {
        let by = match p {
            Pick::Type(_) => ColorBy::Type,
            Pick::Age(_) => ColorBy::Age,
            Pick::Top(_) => ColorBy::Folder,
        };
        if pick_of(model, t, by) != p {
            return false;
        }
    }
    if query.is_empty() {
        return true;
    }
    if id.is_some_and(|id| model.nodes[id].name.to_lowercase().contains(query)) {
        return true;
    }
    folder.is_some_and(|f| folder_matches(model, f, query, memo))
}
fn folder_matches(model: &Model, id: usize, query: &str, memo: &mut HashMap<usize, bool>) -> bool {
    // Walk up to the first folder already known, then fill in the way back down.
    let mut chain = vec![];
    let mut next = Some(id);
    let mut known = false;
    while let Some(f) = next {
        if let Some(&m) = memo.get(&f) {
            known = m;
            break;
        }
        chain.push(f);
        next = model.nodes[f].parent.filter(|&p| p < f);
    }
    for f in chain.into_iter().rev() {
        known = known || model.nodes[f].name.to_lowercase().contains(query);
        memo.insert(f, known);
    }
    known
}
fn wheel_factor(events: &[egui::Event], page: f32) -> f64 {
    let mut factor = 1.0f64;
    for e in events {
        match e {
            egui::Event::MouseWheel { unit, delta, .. } => {
                let dy = match unit {
                    egui::MouseWheelUnit::Point => delta.y,
                    egui::MouseWheelUnit::Line => delta.y * 40.0,
                    egui::MouseWheelUnit::Page => delta.y * page,
                };
                factor *= (dy as f64 * 0.003).exp();
            }
            egui::Event::Zoom(z) if z.is_finite() && *z > 0.0 => factor *= *z as f64,
            _ => {}
        }
    }
    factor
}
fn tooltip(ui: &mut egui::Ui, model: &Model, layout: &Layout, hit: Hit, scan: u64) {
    ui.set_max_width(340.0);
    let share = |b: u64| format!("{:.2}% of scan", b as f64 / scan.max(1) as f64 * 100.0);
    let (title, path, line1, line2) = match hit {
        Hit::Folder(id) => {
            let n = &model.nodes[id as usize];
            (
                crate::node_name(n),
                crate::display_path(&n.path),
                format!("{} · {}", crate::size(n.bytes), share(n.bytes)),
                format!(
                    "Folder · {} files · modified {}",
                    count(n.files),
                    date(n.modified)
                ),
            )
        }
        Hit::Tile(i) => {
            let t = layout.tiles[i];
            match t.kind {
                TileKind::File(id) => {
                    let n = &model.nodes[id as usize];
                    (
                        n.name.clone(),
                        crate::display_path(&n.path),
                        format!("{} · {}", crate::size(n.bytes), share(n.bytes)),
                        format!(
                            "{} · modified {}",
                            model.category(id as usize).label(),
                            date(n.modified)
                        ),
                    )
                }
                TileKind::Group { folder, count: c } => {
                    let f = &model.nodes[folder as usize];
                    (
                        format!("{} small files", count(c)),
                        format!("in {}", crate::display_path(&f.path)),
                        format!("{} · {}", crate::size(t.bytes), share(t.bytes)),
                        "Each is too small to draw. Zoom in to see them.".to_owned(),
                    )
                }
                TileKind::Hidden { files } => (
                    "Hidden by filters".to_owned(),
                    format!("{} files", count(files)),
                    format!("{} · {}", crate::size(t.bytes), share(t.bytes)),
                    "The list's size and age filters and Hide smaller than leave these out."
                        .to_owned(),
                ),
            }
        }
    };
    ui.label(RichText::new(title).strong());
    ui.label(RichText::new(path).size(11.0).color(MUTED));
    ui.label(line1);
    ui.label(RichText::new(line2).color(MUTED));
}
fn hide_label(v: u64) -> &'static str {
    match v {
        0 => "Show all sizes",
        v if v >= 1 << 30 => "Hide under 1 GiB",
        _ => "Hide under 100 MiB",
    }
}
fn zoom_text(z: f64) -> String {
    if !z.is_finite() {
        "1.0×".into()
    } else if z < 10.0 {
        format!("{z:.1}×")
    } else {
        format!("{}×", count(z.round() as u64))
    }
}
/// 1234567 as "1,234,567".
fn count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}
/// A Unix time as a calendar date (UTC), "2026-03-14".
fn date(t: u64) -> String {
    if t == 0 {
        return "not recorded".into();
    }
    // Howard Hinnant's days-to-civil.
    let z = (t / 86400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}
fn aside_card(ui: &mut egui::Ui, width: f32, add: impl FnOnce(&mut egui::Ui)) {
    theme::card()
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.set_width((width - 30.0).max(10.0));
            ui.spacing_mut().item_spacing.y = 4.0;
            add(ui);
        });
}
/// A legend or "largest" row: dot, name, size, share. A click target.
fn list_row(
    ui: &mut egui::Ui,
    color: Color32,
    name: &str,
    bytes: u64,
    share: f64,
    on: bool,
    quiet: bool,
) -> egui::Response {
    let (rect, r) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 24.0), Sense::click());
    let back = rect.expand2(Vec2::new(6.0, 0.0));
    if on || r.hovered() || r.has_focus() {
        ui.painter().rect_filled(back, 6, SELECTED_BG);
    }
    if r.has_focus() {
        ui.painter().rect_stroke(
            back,
            6,
            egui::Stroke::new(1.5, ACCENT),
            egui::StrokeKind::Inside,
        );
    }
    let p = ui.painter();
    let dot = Rect::from_center_size(
        egui::pos2(rect.left() + 6.0, rect.center().y),
        Vec2::splat(10.0),
    );
    p.rect_filled(dot, 3, color);
    let ink = if quiet { MUTED } else { TEXT };
    let font = egui::FontId::proportional(12.0);
    p.text(
        egui::pos2(rect.right(), rect.center().y),
        egui::Align2::RIGHT_CENTER,
        format!("{:.0}%", share * 100.0),
        font.clone(),
        MUTED,
    );
    p.text(
        egui::pos2(rect.right() - 46.0, rect.center().y),
        egui::Align2::RIGHT_CENTER,
        crate::size(bytes),
        font.clone(),
        ink,
    );
    let room = (rect.width() - 20.0 - 46.0 - 78.0).max(10.0);
    let mut job = fit_job(name, room, ink);
    job.sections[0].format.font_id = font;
    let g = p.layout_job(job);
    p.galley(
        egui::pos2(rect.left() + 20.0, rect.center().y - g.size().y / 2.0),
        g,
        ink,
    );
    if r.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    r
}
/// A 28 x 28 outlined button with a drawn minus or plus.
fn icon_button(ui: &mut egui::Ui, enabled: bool, name: &str, plus: bool) -> egui::Response {
    let r = ui
        .add_enabled(enabled, theme::outlined("").min_size(Vec2::splat(28.0)))
        .on_hover_text(name);
    let c = r.rect.center();
    let ink = if enabled {
        TEXT
    } else {
        Color32::from_rgb(0x6e, 0x72, 0x7c)
    };
    let stroke = egui::Stroke::new(1.7, ink);
    ui.painter()
        .line_segment([c - Vec2::X * 5.0, c + Vec2::X * 5.0], stroke);
    if plus {
        ui.painter()
            .line_segment([c - Vec2::Y * 5.0, c + Vec2::Y * 5.0], stroke);
    }
    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, name));
    r
}
fn chevron(ui: &egui::Ui, rect: Rect) {
    let c = egui::pos2(rect.left() + 15.0, rect.center().y);
    ui.painter().line(
        vec![
            c + Vec2::new(3.0, -5.0),
            c + Vec2::new(-2.0, 0.0),
            c + Vec2::new(3.0, 5.0),
        ],
        egui::Stroke::new(1.7, TEXT),
    );
}

pub const HINT_FULL: &str = "Click to open space explorer";
pub const HINT_SHORT: &str = "Open explorer";
/// The long hint when it fits beside the title, else the short one: it never wraps.
pub fn hint_text(available: f32, full_width: f32) -> &'static str {
    if available >= full_width {
        HINT_FULL
    } else {
        HINT_SHORT
    }
}
/// "Click to open space explorer" with an expand icon, for the Space map card's header. Use it
/// in a right-to-left layout beside the title.
pub fn open_hint(ui: &mut egui::Ui) -> egui::Response {
    const ICON: f32 = 10.0;
    let font = egui::FontId::proportional(11.0);
    let full = ui
        .painter()
        .layout_no_wrap(HINT_FULL.into(), font.clone(), HINT)
        .size()
        .x
        + ICON
        + 10.0;
    let text = hint_text(ui.available_width(), full);
    let width = ui
        .painter()
        .layout_no_wrap(text.into(), font.clone(), HINT)
        .size()
        .x;
    let (rect, r) = ui.allocate_exact_size(Vec2::new(width + ICON + 10.0, 18.0), Sense::click());
    let lit = r.hovered() || r.has_focus();
    let ink = if lit { MUTED } else { HINT };
    let p = ui.painter();
    let g = p.layout_no_wrap(text.into(), font, ink);
    p.galley(
        egui::pos2(rect.left() + 2.0, rect.center().y - g.size().y / 2.0),
        g,
        ink,
    );
    // Expand: two corners and the diagonal between them.
    let b = Rect::from_center_size(
        egui::pos2(rect.right() - 2.0 - ICON / 2.0, rect.center().y),
        Vec2::splat(ICON - 1.0),
    );
    let s = egui::Stroke::new(1.3, ink);
    p.line(
        vec![
            b.right_top() + Vec2::new(-3.5, 0.0),
            b.right_top(),
            b.right_top() + Vec2::new(0.0, 3.5),
        ],
        s,
    );
    p.line(
        vec![
            b.left_bottom() + Vec2::new(3.5, 0.0),
            b.left_bottom(),
            b.left_bottom() + Vec2::new(0.0, -3.5),
        ],
        s,
    );
    p.line_segment([b.left_bottom(), b.right_top()], s);
    if r.has_focus() {
        p.rect_stroke(
            rect.expand(2.0),
            4,
            egui::Stroke::new(1.5, ACCENT),
            egui::StrokeKind::Inside,
        );
    }
    if r.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, "Open space explorer"));
    r.on_hover_text("Open the full-window space explorer (M)")
}

#[cfg(test)]
mod tests {
    use super::model::tests::{node, totals};
    use super::*;
    use egui::{Event, PointerButton};

    fn luminance(c: Color32) -> f64 {
        let ch = |v: u8| {
            let x = v as f64 / 255.0;
            if x <= 0.04045 {
                x / 12.92
            } else {
                ((x + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * ch(c.r()) + 0.7152 * ch(c.g()) + 0.0722 * ch(c.b())
    }
    fn contrast(a: Color32, b: Color32) -> f64 {
        let (x, y) = (luminance(a), luminance(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }
    #[test]
    fn design_colours_are_distinct_colourful_and_labels_stay_readable() {
        let hex = |c: Color32| format!("#{:02x}{:02x}{:02x}", c.r(), c.g(), c.b());
        let spec = [
            (Category::Video, "#f0a83a"),
            (Category::Games, "#7a58d6"),
            (Category::Images, "#45b8f0"),
            (Category::Archives, "#e0806a"),
            (Category::Apps, "#e86fa8"),
            (Category::Documents, "#a6d84f"),
            (Category::Audio, "#2fc4b2"),
            (Category::Code, "#b9a4fb"),
            (Category::System, "#3ecf8e"),
            (Category::Other, "#5b86e5"),
        ];
        for (c, want) in spec {
            assert_eq!(hex(type_color(c)), want, "{c:?}");
        }
        let mut all: Vec<_> = TYPE_ORDER
            .iter()
            .map(|c| type_color(*c).to_array())
            .collect();
        all.sort();
        all.dedup();
        assert_eq!(all.len(), 10);
        let colours = TYPE_ORDER
            .iter()
            .map(|c| type_color(*c))
            .chain((0..6).map(age_color));
        for c in colours {
            let max = c.r().max(c.g()).max(c.b()) as f32;
            let min = c.r().min(c.g()).min(c.b()) as f32;
            assert!((max - min) / max >= 0.3, "{} reads as grey", hex(c));
            // Names on plain, grouped (paler) and dimmed tiles.
            for fill in [c, mix(c, MAP_BG, 0.45), mix(c, MAP_BG, 0.72)] {
                assert!(contrast(fill, category::text(fill)) >= 4.5, "{}", hex(fill));
            }
        }
        // The hint is darker than a caption but still readable on a card.
        assert!(contrast(HINT, CARD) >= 4.5);
        assert!(contrast(HINT, CARD) < contrast(MUTED, CARD));
        assert!(
            contrast(MUTED, SELECTED_BG) >= 4.5,
            "the hidden strip's label"
        );
    }
    #[test]
    fn names_are_cut_with_an_ellipsis_and_never_overflow_the_tile() {
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            let long = "a very long file name that never fits in a small tile, part 2.mkv";
            for width in (8..400).step_by(7) {
                let room = width as f32 - 10.0;
                let g = ui.painter().layout_job(fit_job(long, room, TEXT));
                assert!(g.size().x <= room.max(1.0) + 0.5, "{} > {room}", g.size().x);
                assert_eq!(g.rows.len(), 1);
                if room > 40.0 && room < 300.0 {
                    let last = g.rows[0].glyphs.last().map(|g| g.chr);
                    assert_eq!(last, Some('…'), "cut text shows an ellipsis");
                }
            }
            let short = ui.painter().layout_job(fit_job("a.mkv", 200.0, TEXT));
            assert_eq!(short.text(), "a.mkv");
        });
        out.textures_delta.clear();
    }
    #[test]
    fn the_hint_shortens_instead_of_wrapping() {
        assert_eq!(hint_text(200.0, 180.0), HINT_FULL);
        assert_eq!(hint_text(180.0, 180.0), HINT_FULL);
        assert_eq!(hint_text(179.0, 180.0), HINT_SHORT);
        assert_eq!(hint_text(0.0, 180.0), HINT_SHORT);
    }
    #[test]
    fn selection_sums_count_nested_items_once() {
        let m = Model::build(model::tests::sample(), 1, Filters::default(), 0);
        // SteamLibrary (800) and a file in it, plus loose.zip (50).
        let sels = [Sel::Node(1), Sel::Node(2), Sel::Node(7)];
        assert_eq!(selection_total(&m, &sels), (3, 850));
        let sels = [
            Sel::Node(5),
            Sel::Group {
                folder: 1,
                bytes: 200,
                count: 1,
            },
        ];
        assert_eq!(selection_total(&m, &sels), (2, 300));
        let sels = [
            Sel::Node(1),
            Sel::Group {
                folder: 1,
                bytes: 200,
                count: 1,
            },
        ];
        assert_eq!(selection_total(&m, &sels), (2, 800));
    }
    #[test]
    fn numbers_dates_and_zoom_read_plainly() {
        assert_eq!(count(0), "0");
        assert_eq!(count(1234567), "1,234,567");
        assert_eq!(date(0), "not recorded");
        assert_eq!(date(1_773_446_400), "2026-03-14");
        assert_eq!(date(951_782_400), "2000-02-29");
        assert_eq!(zoom_text(1.0), "1.0×");
        assert_eq!(zoom_text(12.4), "12×");
        assert_eq!(zoom_text(f64::NAN), "1.0×");
    }

    // The explorer drawn for real, with pointer and keyboard input.
    fn scan() -> (Progress, Arc<Vec<Node>>) {
        let mut nodes = vec![node(0, None, r"\\?\D:\", true, 0)];
        for f in 0..4 {
            let id = nodes.len();
            nodes.push(node(id, Some(0), &format!("folder{f}"), true, 0));
            for i in 0..6 {
                let fid = nodes.len();
                nodes.push(node(
                    fid,
                    Some(id),
                    &format!("file{f}-{i}.mkv"),
                    false,
                    (f as u64 + 1) * 1_000_000 + i * 10_000,
                ));
            }
        }
        let nodes = Arc::new(totals(nodes));
        let p = Progress {
            generation: 1,
            bytes: nodes[0].bytes,
            files: 24,
            root: r"\\?\D:\".into(),
            ..Default::default()
        };
        (p, nodes)
    }
    struct Rig {
        ctx: egui::Context,
        ex: Explorer,
        p: Progress,
        nodes: Arc<Vec<Node>>,
    }
    impl Rig {
        fn new() -> Self {
            let (p, nodes) = scan();
            let mut rig = Self {
                ctx: egui::Context::default(),
                ex: Explorer::default(),
                p,
                nodes,
            };
            rig.ex.open_at(0, 1, false);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while rig.ex.model.is_none() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the map was never prepared"
                );
                rig.frame(vec![]);
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            rig.frame(vec![]);
            rig
        }
        fn frame(&mut self, events: Vec<Event>) -> Vec<Command> {
            let mut commands = vec![];
            let mut out = self.ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 900.0))),
                    events,
                    ..Default::default()
                },
                |ui| commands = self.ex.show(ui, &self.p, &self.nodes, (0, 0), false),
            );
            out.textures_delta.clear();
            commands
        }
        fn camera(&self) -> Camera {
            self.ex.view().camera
        }
        fn tile_center(&self, id: u32) -> Pos2 {
            let (_, l) = self.ex.layout.as_ref().unwrap();
            l.tiles[l.find(TileKind::File(id)).unwrap()].rect.center()
                + self.ex.map_rect.unwrap().min.to_vec2()
        }
        fn click(&mut self, pos: Pos2, button: PointerButton, modifiers: Modifiers) {
            for pressed in [true, false] {
                self.frame(vec![
                    Event::PointerMoved(pos),
                    Event::PointerButton {
                        pos,
                        button,
                        pressed,
                        modifiers,
                    },
                ]);
            }
        }
        fn drag(&mut self, from: Pos2, by: Vec2, button: PointerButton) {
            self.frame(vec![
                Event::PointerMoved(from),
                Event::PointerButton {
                    pos: from,
                    button,
                    pressed: true,
                    modifiers: Modifiers::NONE,
                },
            ]);
            for step in 1..=4 {
                self.frame(vec![Event::PointerMoved(from + by * step as f32 / 4.0)]);
            }
            self.frame(vec![Event::PointerButton {
                pos: from + by,
                button,
                pressed: false,
                modifiers: Modifiers::NONE,
            }]);
        }
        fn key(&mut self, key: Key, modifiers: Modifiers) -> Vec<Command> {
            self.frame(vec![Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            }])
        }
    }
    fn wheel(at: Pos2, dy: f32) -> Vec<Event> {
        vec![
            Event::PointerMoved(at),
            Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: Vec2::new(0.0, dy),
                phase: egui::TouchPhase::Move,
                source: egui::MouseWheelSource::Wheel,
                modifiers: Modifiers::NONE,
            },
        ]
    }
    #[test]
    fn wheel_zooms_middle_button_pans_and_left_button_never_pans() {
        let mut rig = Rig::new();
        let map = rig.ex.map_rect.unwrap();
        assert!(map.width() > 500.0 && map.height() > 400.0, "{map:?}");
        rig.frame(wheel(map.center(), 300.0));
        let zoomed = rig.camera();
        assert!(zoomed.scale > 2.0, "{zoomed:?}");
        rig.drag(map.center(), Vec2::new(80.0, 60.0), PointerButton::Primary);
        assert_eq!(rig.camera(), zoomed, "the left button only selects");
        rig.drag(
            map.center(),
            Vec2::new(80.0, 60.0),
            PointerButton::Secondary,
        );
        assert_eq!(rig.camera(), zoomed, "the right button only opens the menu");
        rig.ex.menu = None;
        rig.drag(map.center(), Vec2::new(80.0, 60.0), PointerButton::Middle);
        let panned = rig.camera();
        assert!(
            panned.x < zoomed.x && panned.y < zoomed.y,
            "{panned:?} vs {zoomed:?}"
        );
        assert_eq!(panned.scale, zoomed.scale);
        rig.frame(wheel(map.center(), -100_000.0));
        assert_eq!(
            rig.camera(),
            Camera::default(),
            "zooming out stops at the whole scan"
        );
        rig.key(Key::Home, Modifiers::NONE);
        assert_eq!(rig.camera(), Camera::default());
    }
    #[test]
    fn clicks_select_ctrl_adds_and_keys_move_zoom_and_close() {
        let mut rig = Rig::new();
        let a = rig.tile_center(2);
        rig.click(a, PointerButton::Primary, Modifiers::NONE);
        assert_eq!(rig.ex.view().selected, [Sel::Node(2)]);
        let b = rig.tile_center(9);
        rig.click(b, PointerButton::Primary, Modifiers::CTRL);
        assert_eq!(rig.ex.view().selected, [Sel::Node(2), Sel::Node(9)]);
        let m = rig.ex.model.clone().unwrap();
        let (files, bytes) = selection_total(&m, &rig.ex.view().selected);
        assert_eq!((files, bytes), (2, rig.nodes[2].bytes + rig.nodes[9].bytes));
        assert!(rig.ex.summary(&m).0.starts_with("2 items · 2 files"));
        // Right-click opens the menu on that tile and selects only it.
        rig.click(a, PointerButton::Secondary, Modifiers::NONE);
        assert_eq!(rig.ex.menu.map(|m| m.0), Some(Sel::Node(2)));
        rig.key(Key::Escape, Modifiers::NONE);
        assert!(rig.ex.menu.is_none(), "Esc closes the menu first");
        assert!(rig.ex.open);
        rig.frame(vec![]);
        // Arrows move to a neighbouring tile.
        rig.click(a, PointerButton::Primary, Modifiers::NONE);
        rig.frame(vec![]);
        // The tile sits at an edge of the map, so not every direction has a neighbour.
        for key in [
            Key::ArrowLeft,
            Key::ArrowUp,
            Key::ArrowRight,
            Key::ArrowDown,
        ] {
            rig.key(key, Modifiers::NONE);
            if rig.ex.view().selected != [Sel::Node(2)] {
                break;
            }
        }
        let moved = rig.ex.view().selected.clone();
        assert_eq!(moved.len(), 1);
        assert_ne!(moved[0], Sel::Node(2));
        // Enter zooms to the selected file's folder; Backspace goes back up.
        rig.click(a, PointerButton::Primary, Modifiers::NONE);
        rig.frame(vec![]);
        rig.key(Key::Enter, Modifiers::NONE);
        assert_eq!(rig.ex.view().focus, 1);
        rig.frame(vec![]);
        rig.key(Key::Backspace, Modifiers::NONE);
        assert_eq!(rig.ex.view().focus, 0);
        // Esc closes and hands back the folder that was zoomed to.
        rig.ex.zoom_to(8);
        rig.frame(vec![]);
        let closed = rig.key(Key::Escape, Modifiers::NONE);
        assert!(matches!(closed.as_slice(), [Command::Close(Some(p))] if *p == rig.nodes[8].path));
    }
    #[test]
    fn a_folder_border_selects_the_folder_and_a_chip_zooms() {
        let mut rig = Rig::new();
        let origin = rig.ex.map_rect.unwrap().min.to_vec2();
        let block = {
            let (_, l) = rig.ex.layout.as_ref().unwrap();
            *l.blocks.iter().find(|b| b.id == 1).unwrap()
        };
        // Just inside the border, below the chip.
        let border = egui::pos2(block.rect.left() + 1.0, block.rect.center().y) + origin;
        rig.click(border, PointerButton::Primary, Modifiers::NONE);
        assert_eq!(rig.ex.view().selected, [Sel::Node(1)]);
        let m = rig.ex.model.clone().unwrap();
        assert!(rig.ex.summary(&m).0.starts_with("folder · 6 files"));
        let chip = block.rect.min + Vec2::new(12.0, 12.0) + origin;
        rig.click(chip, PointerButton::Primary, Modifiers::NONE);
        assert_eq!(rig.ex.view().focus, 1);
        assert_eq!(rig.camera(), Camera::default());
    }
    #[test]
    fn the_view_is_kept_until_the_scan_changes_and_follows_a_delete() {
        let mut rig = Rig::new();
        rig.ex.zoom_to(8);
        rig.ex.view_mut().color_by = ColorBy::Age;
        rig.ex.open = false;
        rig.ex.open_at(0, 1, false);
        assert_eq!(rig.ex.view().focus, 8, "same scan, same view");
        assert_eq!(rig.ex.view().color_by, ColorBy::Age);
        // A delete before folder 8 shifts its id by 7.
        let mut map: Vec<Option<usize>> = (0..rig.nodes.len()).map(Some).collect();
        for (i, slot) in map.iter_mut().enumerate() {
            *slot = match i {
                1..=7 => None,
                0 => Some(0),
                i => Some(i - 7),
            };
        }
        rig.ex.remap(&map, 1, 2);
        assert_eq!((rig.ex.view().generation, rig.ex.view().focus), (2, 1));
        rig.ex.open_at(0, 3, false);
        assert_eq!(rig.ex.view().focus, 0, "a new scan starts again");
        assert_eq!(rig.ex.view().color_by, ColorBy::Type);
    }
    #[test]
    fn without_a_scan_it_asks_for_one() {
        let ctx = egui::Context::default();
        let mut ex = Explorer::default();
        ex.open_at(0, 0, false);
        let nodes = Arc::new(vec![]);
        let p = Progress::default();
        let mut commands = vec![];
        for pressed in [None, Some(true), Some(false)] {
            let mut events = vec![];
            if let Some(pressed) = pressed {
                // The Scan button sits in the card under the title row.
                let pos = egui::pos2(40.0, 120.0);
                events.push(Event::PointerMoved(pos));
                events.push(Event::PointerButton {
                    pos,
                    button: PointerButton::Primary,
                    pressed,
                    modifiers: Modifiers::NONE,
                });
            }
            let mut out = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 900.0))),
                    events,
                    ..Default::default()
                },
                |ui| commands.extend(ex.show(ui, &p, &nodes, (0, 0), false)),
            );
            out.textures_delta.clear();
        }
        assert!(ex.model.is_none() && ex.job.is_none());
        assert!(
            commands.iter().any(|c| matches!(c, Command::Scan)),
            "Scan button"
        );
    }
}
