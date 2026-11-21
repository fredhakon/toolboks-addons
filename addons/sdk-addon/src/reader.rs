//! The Reader view: shows a Markdown file the person opened (or the new project's README) as a
//! page inside the add-on. Files are read on a worker thread, at most [`MAX_BYTES`]. Links follow
//! [`classify`]: web links go to the browser through Toolboks, a relative link to another Markdown
//! file in the same folder tree opens here, everything else is refused. Nothing in a document is
//! ever run, fetched or handed to another program.

use crate::markdown::{self, Align, Block, Doc, Inline, Item, Span};
use crate::{poll, shell};
use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId, RichText, Stroke, Vec2};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use toolboks_shared::ui;

/// The largest file the reader opens: 2 MiB.
pub const MAX_BYTES: u64 = 2 * 1024 * 1024;
/// The widest the text column grows; wider pages centre it.
const READ_WIDTH: f32 = 720.0;
/// The extensions a link may open in the reader.
const READABLE: [&str; 3] = ["md", "markdown", "txt"];

/// A file that was read and parsed.
#[derive(Debug)]
pub struct Loaded {
    pub path: PathBuf,
    pub doc: Doc,
    /// The file was not valid UTF-8; it is shown with replacement characters.
    pub lossy: bool,
}

/// What a click on a link does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkAction {
    /// Open in the browser (through Toolboks, which checks it again).
    Web(String),
    /// Open this Markdown file in the reader.
    File(PathBuf),
    /// Scroll to the heading with this anchor.
    Anchor(String),
    /// Not opened; the reason in one line.
    Refused(String),
}

/// Decides what a link in the file `current` does. `root` is the folder the reader stays in (the
/// folder of the first file opened); relative links are resolved inside it and may not leave it.
pub fn classify(link: &str, current: &Path, root: &Path) -> LinkAction {
    let link = link.trim();
    if let Some(anchor) = link.strip_prefix('#') {
        return LinkAction::Anchor(anchor.to_lowercase());
    }
    if let Some((scheme, rest)) = scheme(link) {
        return match scheme.as_str() {
            "http" | "https" => {
                let host = rest.strip_prefix("//").unwrap_or("");
                if host.is_empty() || host.starts_with('/') {
                    LinkAction::Refused("That web link has no address.".into())
                } else {
                    LinkAction::Web(link.to_owned())
                }
            }
            "mailto" => LinkAction::Refused("Email links are not opened from the reader.".into()),
            _ => LinkAction::Refused(format!(
                "Only web links (https or http) open from the reader, not {scheme}: links."
            )),
        };
    }
    // A relative path: no query, no fragment, %XX decoded.
    let path = link.split(['#', '?']).next().unwrap_or("");
    let path = percent_decode(path);
    if path.is_empty() {
        return LinkAction::Refused("That link points nowhere.".into());
    }
    if path.starts_with(['/', '\\']) || path.contains(':') {
        return LinkAction::Refused("Links to other places on the PC are not opened.".into());
    }
    let readable = Path::new(&path)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| READABLE.iter().any(|ok| ok.eq_ignore_ascii_case(ext)));
    if !readable {
        return LinkAction::Refused(
            "Only Markdown and text files (.md, .markdown, .txt) open in the reader.".into(),
        );
    }
    let Ok(here) = current.parent().unwrap_or(root).strip_prefix(root) else {
        return LinkAction::Refused("That file is outside the folder you opened.".into());
    };
    let mut inside: Vec<String> = Vec::new();
    for part in here.components() {
        match part {
            Component::Normal(name) => inside.push(name.to_string_lossy().into_owned()),
            _ => return LinkAction::Refused("That file is outside the folder you opened.".into()),
        }
    }
    for part in path.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                if inside.pop().is_none() {
                    return LinkAction::Refused(
                        "That link leads outside the folder you opened.".into(),
                    );
                }
            }
            name => inside.push(name.to_owned()),
        }
    }
    let mut target = root.to_path_buf();
    target.extend(inside);
    LinkAction::File(target)
}

/// The lower-case scheme of a URL like `https://…` and what follows the colon. A single letter
/// before the colon is a drive (`C:\…`), not a scheme.
fn scheme(link: &str) -> Option<(String, &str)> {
    let (scheme, rest) = link.split_once(':')?;
    let mut chars = scheme.chars();
    let first = chars.next()?;
    let valid = scheme.len() > 1
        && first.is_ascii_alphabetic()
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    valid.then(|| (scheme.to_ascii_lowercase(), rest))
}

/// `my%20notes.md` -> `my notes.md`. Anything that does not decode to UTF-8 stays as it was.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| text.to_owned())
}

/// Reads and parses `path`, which must lie inside `root` (after resolving links on the disk).
/// Refuses files over [`MAX_BYTES`]; text that is not UTF-8 is decoded with replacement
/// characters and marked `lossy`.
pub fn read_document(path: &Path, root: &Path) -> Result<Loaded, String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let plain = |e: std::io::Error| match e.kind() {
        std::io::ErrorKind::NotFound => format!("{name} was not found."),
        std::io::ErrorKind::PermissionDenied => format!("Windows did not allow reading {name}."),
        _ => format!("Could not read {name}: {e}"),
    };
    let real = path.canonicalize().map_err(plain)?;
    let real_root = root.canonicalize().map_err(plain)?;
    if !real.starts_with(&real_root) {
        return Err(format!("{name} is outside the folder you opened."));
    }
    // Windows refuses to open a folder as a file, so ask first for the plain answer.
    if !std::fs::metadata(&real).map_err(plain)?.is_file() {
        return Err(format!("{name} is not a file."));
    }
    let file = std::fs::File::open(&real).map_err(plain)?;
    let meta = file.metadata().map_err(plain)?;
    if meta.len() > MAX_BYTES {
        return Err(format!(
            "{name} is larger than 2 MiB, too big for the reader."
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(plain)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(format!(
            "{name} is larger than 2 MiB, too big for the reader."
        ));
    }
    let (text, lossy) = match String::from_utf8(bytes) {
        Ok(text) => (text, false),
        Err(e) => (String::from_utf8_lossy(e.as_bytes()).into_owned(), true),
    };
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    Ok(Loaded {
        path: path.to_path_buf(),
        doc: markdown::parse(text),
        lossy,
    })
}

/// The reader's state. Only the open file is remembered, and only while the add-on runs.
#[derive(Default)]
pub struct Reader {
    /// The folder links may not leave.
    pub root: Option<PathBuf>,
    /// The file on screen.
    pub file: Option<PathBuf>,
    pub doc: Option<Doc>,
    pub lossy: bool,
    /// Why the last open failed.
    pub error: Option<String>,
    /// A refused link, in one line.
    pub notice: Option<String>,
    loading: Option<Receiver<Result<Loaded, String>>>,
    /// The root the file being read will have.
    loading_root: Option<PathBuf>,
    picker: Option<Receiver<shell::Picked>>,
    /// A heading to scroll to on the next frame.
    anchor: Option<String>,
    scroll_top: bool,
}

impl Reader {
    /// Starts reading `path` on a worker thread. Links then stay inside `root`.
    pub fn open(&mut self, path: PathBuf, root: PathBuf, ctx: &egui::Context) {
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        let worker_root = root.clone();
        std::thread::spawn(move || {
            let _ = tx.send(read_document(&path, &worker_root));
            ctx.request_repaint();
        });
        self.loading = Some(rx);
        self.loading_root = Some(root);
        self.notice = None;
    }

    pub fn is_loading(&self) -> bool {
        self.loading.is_some()
    }

    /// Takes what the workers finished.
    pub fn collect(&mut self, ctx: &egui::Context) {
        if let Some(result) = poll(&mut self.loading, || {
            Err("Reading the file stopped before it finished.".into())
        }) {
            self.finish(result);
        }
        match poll(&mut self.picker, || Err("The file picker stopped.".into())) {
            Some(Ok(Some(path))) => {
                let root = path.parent().map(Path::to_path_buf).unwrap_or_default();
                self.open(path, root, ctx);
            }
            Some(Ok(None)) | None => {}
            Some(Err(error)) => self.error = Some(error),
        }
    }

    fn finish(&mut self, result: Result<Loaded, String>) {
        let root = self.loading_root.take();
        match result {
            Ok(loaded) => {
                self.file = Some(loaded.path);
                self.doc = Some(loaded.doc);
                self.lossy = loaded.lossy;
                self.root = root;
                self.error = None;
                self.scroll_top = true;
            }
            Err(error) => self.error = Some(error),
        }
    }

    /// Blocks until the file being read is in (tests only).
    #[cfg(test)]
    pub fn wait(&mut self) {
        if let Some(rx) = self.loading.take() {
            let result = rx
                .recv_timeout(std::time::Duration::from_secs(20))
                .unwrap_or_else(|_| Err("timed out".into()));
            self.finish(result);
        }
    }

    /// Acts on a clicked link.
    fn follow(&mut self, link: &str, ctx: &egui::Context) {
        let (Some(current), Some(root)) = (self.file.clone(), self.root.clone()) else {
            return;
        };
        match classify(link, &current, &root) {
            LinkAction::Web(url) => {
                self.notice = None;
                ctx.open_url(egui::OpenUrl::new_tab(url));
            }
            LinkAction::File(path) => self.open(path, root, ctx),
            LinkAction::Anchor(anchor) => {
                if self.doc.as_ref().is_some_and(|doc| doc.has_anchor(&anchor)) {
                    self.notice = None;
                    self.anchor = Some(anchor);
                } else {
                    self.notice = Some(format!("No heading in this file answers #{anchor}."));
                }
            }
            LinkAction::Refused(why) => self.notice = Some(why),
        }
    }

    /// The Reader view: the toolbar, then the document in a scroll area.
    pub fn ui(&mut self, ui: &mut egui::Ui, last_folder: Option<&Path>) {
        let avail = ui.available_width();
        let width = (avail - 2.0 * ui::SPACE_L).clamp(240.0, READ_WIDTH);
        let pad = ((avail - width) / 2.0).max(ui::SPACE_L);
        ui.horizontal(|ui| {
            ui.add_space(pad);
            ui.vertical(|ui| {
                ui.set_width(width);
                self.toolbar(ui, last_folder);
            });
        });
        ui.add_space(ui::GAP);
        let mut scroll = egui::ScrollArea::vertical()
            .id_salt("reader-document")
            .auto_shrink([false, false]);
        if std::mem::take(&mut self.scroll_top) {
            scroll = scroll.vertical_scroll_offset(0.0);
        }
        let mut draw = Draw {
            anchor: self.anchor.take(),
            ..Draw::default()
        };
        scroll.show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add_space(pad);
                ui.vertical(|ui| {
                    ui.set_width(width);
                    match &self.doc {
                        Some(doc) => blocks(ui, &doc.blocks, &mut draw),
                        None => empty_state(ui),
                    }
                    ui.add_space(ui::SPACE_L);
                });
            });
        });
        if let Some(link) = draw.clicked {
            self.follow(&link, ui.ctx());
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui, last_folder: Option<&Path>) {
        let mut pick = false;
        let mut reload = false;
        ui.horizontal(|ui| {
            let picking = self.picker.is_some();
            let label = if picking {
                "Choosing..."
            } else {
                "Open Markdown file..."
            };
            pick = ui.add_enabled(!picking, crate::outlined(label)).clicked();
            let can_reload = self.file.is_some() && !self.is_loading();
            reload = ui
                .add_enabled(can_reload, crate::outlined("Reload"))
                .clicked();
            if self.is_loading() {
                ui.label(RichText::new("Reading...").color(ui::MUTED));
            } else if let Some(file) = &self.file {
                let name = file
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                ui.add(egui::Label::new(RichText::new(name).color(ui::MUTED)).truncate())
                    .on_hover_text(file.display().to_string());
            }
        });
        if let Some(error) = &self.error {
            crate::note(ui, error, ui::RED);
        }
        if self.lossy {
            crate::note(
                ui,
                "This file is not valid UTF-8 text; characters that could not be read show as \u{fffd}.",
                ui::MUTED,
            );
        }
        if let Some(notice) = &self.notice {
            crate::note(ui, notice, ui::MUTED);
        }
        if pick {
            let start = self
                .file
                .as_deref()
                .and_then(Path::parent)
                .or(last_folder)
                .map(Path::to_path_buf);
            self.error = None;
            self.picker = Some(shell::pick_markdown(start, ui.ctx().clone()));
        }
        if reload {
            if let (Some(file), Some(root)) = (self.file.clone(), self.root.clone()) {
                self.error = None;
                self.open(file, root, ui.ctx());
            }
        }
    }
}

fn empty_state(ui: &mut egui::Ui) {
    ui.add_space(ui::SPACE_L);
    ui.label(
        RichText::new(
            "Open a Markdown file, or press Get started (README) on the Create view after \
             making a project.",
        )
        .color(ui::MUTED),
    );
}

/// What drawing a document found: a clicked link, and whether the heading to scroll to was seen.
#[derive(Default)]
struct Draw {
    clicked: Option<String>,
    anchor: Option<String>,
    /// Inside how many block quotes (their text is muted).
    quote: u32,
}

fn blocks(ui: &mut egui::Ui, blocks: &[Block], draw: &mut Draw) {
    for (index, block) in blocks.iter().enumerate() {
        if index > 0 {
            ui.add_space(ui::GAP);
        }
        ui.push_id(index, |ui| block_ui(ui, block, draw));
    }
}

fn block_ui(ui: &mut egui::Ui, block: &Block, draw: &mut Draw) {
    match block {
        Block::Heading {
            level,
            anchor,
            text,
        } => heading(ui, *level, anchor, text, draw),
        Block::Paragraph(text) => inline(ui, text, ui::TEXT_BODY, false, draw),
        Block::List { start, items } => list(ui, *start, items, draw),
        Block::Quote(inner) => quote(ui, inner, draw),
        Block::Code(text) => code_block(ui, text),
        Block::Table {
            aligns,
            header,
            rows,
        } => table(ui, aligns, header, rows, draw),
        Block::Rule => {
            ui.separator();
        }
        Block::Html(text) => {
            ui.add(
                egui::Label::new(
                    RichText::new(text)
                        .monospace()
                        .size(ui::TEXT_BODY)
                        .color(ui::MUTED),
                )
                .wrap(),
            );
        }
    }
}

fn heading(ui: &mut egui::Ui, level: u8, anchor: &str, text: &Inline, draw: &mut Draw) {
    // DESIGN.md's sizes only: 18, 14, 12 and 11.
    let (size, rule) = match level {
        1 => (ui::TEXT_TITLE, true),
        2 => (ui::TEXT_SECTION, true),
        3 => (ui::TEXT_SECTION, false),
        4 | 5 => (ui::TEXT_BODY, false),
        _ => (ui::TEXT_CAPTION, false),
    };
    if level <= 2 {
        ui.add_space(ui::SPACE_XS);
    }
    let muted = level >= 5;
    let response = ui
        .scope(|ui| {
            if muted {
                draw.quote += 1;
                inline(ui, text, size, true, draw);
                draw.quote -= 1;
            } else {
                inline(ui, text, size, true, draw);
            }
        })
        .response;
    if rule {
        let y = response.rect.bottom() + 3.0;
        ui.painter().hline(
            response.rect.left()..=response.rect.left() + ui.available_width(),
            y,
            Stroke::new(1.0, ui::BORDER),
        );
        ui.add_space(ui::SPACE_XS);
    }
    if draw.anchor.as_deref() == Some(anchor) {
        ui.scroll_to_rect(response.rect, Some(egui::Align::TOP));
        draw.anchor = None;
    }
}

/// The colour of plain text here.
fn text_colour(draw: &Draw) -> Color32 {
    if draw.quote > 0 {
        ui::MUTED
    } else {
        ui::TEXT
    }
}

/// `spans` as one layout job. Link text gets the placeholder colour, which egui's link widget
/// paints in the link colour.
fn job(spans: &[Span], size: f32, strong: bool, colour: Color32) -> LayoutJob {
    let mut job = LayoutJob::default();
    for span in spans {
        let style = span.style;
        let mut colour = if style.strong || strong {
            if colour == ui::TEXT {
                Color32::WHITE
            } else {
                ui::TEXT
            }
        } else {
            colour
        };
        if style.image {
            colour = ui::MUTED;
        }
        if span.link.is_some() {
            colour = Color32::PLACEHOLDER;
        }
        let format = TextFormat {
            font_id: if style.code {
                FontId::monospace(size)
            } else {
                FontId::proportional(size)
            },
            color: colour,
            background: if style.code {
                ui::SELECTED_BG
            } else {
                Color32::TRANSPARENT
            },
            italics: style.emphasis || style.image,
            strikethrough: if style.strike {
                Stroke::new(1.0, colour)
            } else {
                Stroke::NONE
            },
            ..TextFormat::default()
        };
        job.append(&span.text, 0.0, format);
    }
    job
}

/// Wrapped text. Without links it is one label (one piece for Ctrl+C); with links each run is
/// its own label or link, flowing on the same lines.
fn inline(ui: &mut egui::Ui, spans: &[Span], size: f32, strong: bool, draw: &mut Draw) {
    let colour = text_colour(draw);
    if spans.iter().all(|span| span.link.is_none()) {
        ui.add(egui::Label::new(job(spans, size, strong, colour)).wrap());
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        let mut rest = spans;
        while let Some(first) = rest.first() {
            let run = rest
                .iter()
                .take_while(|span| span.link == first.link)
                .count();
            let (now, later) = rest.split_at(run);
            rest = later;
            let text = job(now, size, strong, colour);
            match &first.link {
                None => {
                    ui.add(egui::Label::new(text).wrap());
                }
                Some(link) => {
                    let response = ui.add(egui::Link::new(text)).on_hover_text(link);
                    if response.clicked() {
                        draw.clicked = Some(link.clone());
                    }
                }
            }
        }
    });
}

fn list(ui: &mut egui::Ui, start: Option<u64>, items: &[Item], draw: &mut Draw) {
    let line = ui.text_style_height(&egui::TextStyle::Body);
    let marker_width = if start.is_some() { 26.0 } else { 16.0 };
    for (n, item) in items.iter().enumerate() {
        if n > 0 {
            ui.add_space(ui::SPACE_XS);
        }
        ui.push_id(n, |ui| {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = ui::SPACE_XS;
                let (rect, _) =
                    ui.allocate_exact_size(Vec2::new(marker_width, line), egui::Sense::hover());
                let colour = text_colour(draw);
                match (item.task, start) {
                    (Some(done), _) => task_box(ui, rect, done),
                    (None, Some(first)) => {
                        ui.painter().text(
                            egui::pos2(rect.right() - ui::SPACE_XS, rect.top()),
                            egui::Align2::RIGHT_TOP,
                            format!("{}.", first.saturating_add(n as u64)),
                            FontId::proportional(ui::TEXT_BODY),
                            colour,
                        );
                    }
                    (None, None) => {
                        ui.painter().circle_filled(
                            egui::pos2(rect.left() + 6.0, rect.center().y),
                            2.5,
                            ui::MUTED,
                        );
                    }
                }
                ui.vertical(|ui| blocks(ui, &item.blocks, draw));
            });
        });
    }
}

/// A small checkbox picture for a task list item: an outline, or filled with a tick when done.
/// It cannot be clicked; the file is never changed.
fn task_box(ui: &egui::Ui, rect: egui::Rect, done: bool) {
    let side = 11.0;
    let square = egui::Rect::from_center_size(
        egui::pos2(rect.left() + side / 2.0 + 1.0, rect.center().y),
        Vec2::splat(side),
    );
    let painter = ui.painter();
    if done {
        painter.rect_filled(square, 3.0, ui::HOST_ACCENT_FILL);
        let tick = [
            egui::pos2(square.left() + 2.5, square.center().y),
            egui::pos2(square.left() + 4.5, square.bottom() - 3.0),
            egui::pos2(square.right() - 2.5, square.top() + 3.0),
        ];
        painter.add(egui::Shape::line(
            tick.to_vec(),
            Stroke::new(1.5, Color32::WHITE),
        ));
    } else {
        painter.rect_stroke(
            square,
            3.0,
            Stroke::new(1.0, ui::MUTED),
            egui::StrokeKind::Inside,
        );
    }
}

fn quote(ui: &mut egui::Ui, inner: &[Block], draw: &mut Draw) {
    let response = egui::Frame::new()
        .inner_margin(egui::Margin {
            left: 12,
            right: 0,
            top: 2,
            bottom: 2,
        })
        .show(ui, |ui| {
            draw.quote += 1;
            blocks(ui, inner, draw);
            draw.quote -= 1;
        })
        .response;
    let rect = response.rect;
    ui.painter().vline(
        rect.left() + 1.5,
        rect.y_range(),
        Stroke::new(3.0, ui::BORDER),
    );
}

fn code_block(ui: &mut egui::Ui, text: &str) {
    egui::Frame::new()
        .fill(ui::BAR_BG)
        .stroke(Stroke::new(1.0, ui::BORDER))
        .corner_radius(ui::CONTROL_RADIUS)
        .inner_margin(ui::GAP)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::horizontal()
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(text)
                                .monospace()
                                .size(ui::TEXT_BODY)
                                .color(ui::TEXT),
                        )
                        .extend(),
                    );
                });
        });
}

fn table(
    ui: &mut egui::Ui,
    aligns: &[Align],
    header: &[Inline],
    rows: &[Vec<Inline>],
    draw: &mut Draw,
) {
    let columns = rows
        .iter()
        .map(Vec::len)
        .chain([header.len(), aligns.len()])
        .max()
        .unwrap_or(0);
    if columns == 0 {
        return;
    }
    let gap = ui::SPACE_M;
    // Each column as wide as its widest cell on one line, at most 320, at least 40; squeezed
    // to the page when that is wider, down to 60 a column (then the table scrolls sideways).
    let font = FontId::proportional(ui::TEXT_BODY);
    let mut widths = vec![40.0_f32; columns];
    for row in std::iter::once(header).chain(rows.iter().map(Vec::as_slice)) {
        for (column, cell) in row.iter().enumerate() {
            let natural = ui
                .painter()
                .layout_no_wrap(markdown::plain(cell), font.clone(), ui::TEXT)
                .size()
                .x
                + 2.0;
            widths[column] = widths[column].max(natural.min(320.0));
        }
    }
    let room = ui.available_width() - gap * (columns as f32 - 1.0);
    let total: f32 = widths.iter().sum();
    if total > room {
        let scale = room / total;
        for width in &mut widths {
            *width = (*width * scale).max(60.0);
        }
    }
    egui::ScrollArea::horizontal()
        .auto_shrink([false, true])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            table_row(ui, header, aligns, &widths, gap, true, draw);
            for (n, row) in rows.iter().enumerate() {
                ui.push_id(n, |ui| {
                    table_row(ui, row, aligns, &widths, gap, false, draw)
                });
            }
        });
}

fn table_row(
    ui: &mut egui::Ui,
    cells: &[Inline],
    aligns: &[Align],
    widths: &[f32],
    gap: f32,
    header: bool,
    draw: &mut Draw,
) {
    ui.add_space(ui::SPACE_XS);
    let response = ui
        .horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for (column, width) in widths.iter().enumerate() {
                let cell = cells.get(column).map(Vec::as_slice).unwrap_or(&[]);
                let align = match aligns.get(column) {
                    Some(Align::Center) => egui::Align::Center,
                    Some(Align::Right) => egui::Align::Max,
                    _ => egui::Align::Min,
                };
                ui.push_id(column, |ui| {
                    ui.allocate_ui_with_layout(
                        Vec2::new(*width, 0.0),
                        egui::Layout::top_down(align),
                        |ui| {
                            ui.set_width(*width);
                            if !cell.is_empty() {
                                inline(ui, cell, ui::TEXT_BODY, header, draw);
                            }
                        },
                    );
                });
            }
        })
        .response;
    ui.add_space(ui::SPACE_XS);
    let colour = if header { ui::BORDER } else { ui::BAR_DIVIDER };
    ui.painter().hline(
        response.rect.x_range(),
        ui.cursor().top(),
        Stroke::new(1.0, colour),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn root() -> PathBuf {
        PathBuf::from(r"D:\Docs\project")
    }

    /// The link policy, case by case.
    #[test]
    fn links_follow_the_policy() {
        let root = root();
        let readme = root.join("README.md");
        let nested = root.join("docs").join("guide.md");
        let web = |l: &str| LinkAction::Web(l.into());
        let cases: Vec<(&str, &Path, LinkAction)> = vec![
            (
                "https://example.com/a",
                &readme,
                web("https://example.com/a"),
            ),
            ("http://example.com", &readme, web("http://example.com")),
            ("HTTPS://Example.com", &readme, web("HTTPS://Example.com")),
            (
                "  https://example.com  ",
                &readme,
                web("https://example.com"),
            ),
            (
                "#The-Edits",
                &readme,
                LinkAction::Anchor("the-edits".into()),
            ),
            ("docs/guide.md", &readme, LinkAction::File(nested.clone())),
            (
                "./docs/guide.md#usage",
                &readme,
                LinkAction::File(nested.clone()),
            ),
            ("docs\\guide.md", &readme, LinkAction::File(nested.clone())),
            ("../README.md", &nested, LinkAction::File(readme.clone())),
            (
                "NOTES.TXT",
                &readme,
                LinkAction::File(root.join("NOTES.TXT")),
            ),
            (
                "my%20notes.markdown",
                &readme,
                LinkAction::File(root.join("my notes.markdown")),
            ),
        ];
        for (link, current, expected) in cases {
            assert_eq!(classify(link, current, &root), expected, "{link}");
        }
        for (link, current) in [
            ("mailto:a@example.com", &readme),
            ("file:///C:/Windows/notepad.exe", &readme),
            ("javascript:alert(1)", &readme),
            ("ftp://example.com/x.md", &readme),
            ("ms-settings:privacy", &readme),
            ("https:", &readme),
            ("https:///path", &readme),
            ("../outside.md", &readme),
            ("docs/../../outside.md", &readme),
            ("../../outside.md", &nested),
            ("C:\\Windows\\win.ini", &readme),
            ("C:/notes.md", &readme),
            ("/etc/notes.md", &readme),
            ("\\\\server\\share\\x.md", &readme),
            ("//server/share/x.md", &readme),
            ("tool.exe", &readme),
            ("script.ps1", &readme),
            ("notes", &readme),
            ("", &readme),
            ("?x=1", &readme),
            ("a.md:stream", &readme),
        ] {
            assert!(
                matches!(classify(link, current, &root), LinkAction::Refused(_)),
                "{link} should be refused"
            );
        }
        // A file outside the root never resolves anything relative.
        assert!(matches!(
            classify("a.md", Path::new(r"E:\elsewhere\b.md"), &root),
            LinkAction::Refused(_)
        ));
    }

    #[test]
    fn percent_decoding_is_plain() {
        assert_eq!(percent_decode("a%20b.md"), "a b.md");
        assert_eq!(percent_decode("100%.md"), "100%.md");
        assert_eq!(percent_decode("a%2"), "a%2");
        assert_eq!(percent_decode("%C3%A6.md"), "æ.md");
        assert_eq!(percent_decode("%FF.md"), "%FF.md");
    }

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::File::create(&path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
        path
    }

    /// 2 MiB opens, one byte more does not.
    #[test]
    fn files_over_two_mebibytes_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut text = b"# Big\n\n".to_vec();
        text.resize(MAX_BYTES as usize, b'a');
        let fits = write(dir.path(), "fits.md", &text);
        assert!(read_document(&fits, dir.path()).is_ok());
        text.push(b'a');
        let big = write(dir.path(), "big.md", &text);
        let error = read_document(&big, dir.path()).unwrap_err();
        assert!(error.contains("larger than 2 MiB"), "{error}");
    }

    /// UTF-8 (with or without a byte order mark) reads as is; anything else is decoded with
    /// replacement characters and marked.
    #[test]
    fn text_that_is_not_utf8_is_shown_with_a_note() {
        let dir = tempfile::tempdir().unwrap();
        let good = write(dir.path(), "good.md", "\u{feff}# Blåbær\n".as_bytes());
        let loaded = read_document(&good, dir.path()).unwrap();
        assert!(!loaded.lossy);
        assert!(loaded.doc.has_anchor("blåbær"));
        let bad = write(dir.path(), "bad.md", b"# Bl\xe5b\xe6r\n");
        let loaded = read_document(&bad, dir.path()).unwrap();
        assert!(loaded.lossy);
        let Block::Heading { text, .. } = &loaded.doc.blocks[0] else {
            unreachable!()
        };
        assert!(markdown::plain(text).contains('\u{fffd}'));
    }

    #[test]
    fn missing_folders_and_outside_files_are_plain_errors() {
        let dir = tempfile::tempdir().unwrap();
        let inner = dir.path().join("inner");
        std::fs::create_dir(&inner).unwrap();
        let missing = read_document(&inner.join("nope.md"), &inner).unwrap_err();
        assert_eq!(missing, "nope.md was not found.");
        let outside = write(dir.path(), "outside.md", b"# x");
        let error = read_document(&outside, &inner).unwrap_err();
        assert!(error.contains("outside the folder"), "{error}");
        let error = read_document(&inner, dir.path()).unwrap_err();
        assert_eq!(error, "inner is not a file.");
    }

    /// Opening runs on a worker; the result lands through the channel, and a link to a file
    /// next to it opens that file with the same root.
    #[test]
    fn a_relative_link_opens_the_next_file_inside_the_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("docs")).unwrap();
        let readme = write(
            dir.path(),
            "README.md",
            b"# Start\n\n[Guide](docs/guide.md)\n",
        );
        let guide = write(
            dir.path(),
            "docs/guide.md",
            b"# Guide\n\n[Back](../README.md)\n",
        );
        let ctx = egui::Context::default();
        let mut reader = Reader::default();
        reader.open(readme.clone(), dir.path().to_path_buf(), &ctx);
        assert!(reader.is_loading());
        reader.wait();
        assert_eq!(reader.file.as_deref(), Some(readme.as_path()));
        assert!(reader.doc.as_ref().unwrap().has_anchor("start"));
        reader.follow("docs/guide.md", &ctx);
        reader.wait();
        assert_eq!(reader.file.as_deref(), Some(guide.as_path()));
        assert_eq!(reader.root.as_deref(), Some(dir.path()));
        reader.follow("../../escape.md", &ctx);
        assert!(!reader.is_loading());
        assert!(reader.notice.is_some());
        reader.follow("#guide", &ctx);
        assert_eq!(reader.anchor.as_deref(), Some("guide"));
        assert_eq!(reader.notice, None);
        reader.follow("#nothing-here", &ctx);
        assert!(reader.notice.as_deref().unwrap().contains("#nothing-here"));
        // A failed open keeps the page on screen and says why.
        reader.open(dir.path().join("gone.md"), dir.path().to_path_buf(), &ctx);
        reader.wait();
        assert_eq!(reader.file.as_deref(), Some(guide.as_path()));
        assert_eq!(reader.error.as_deref(), Some("gone.md was not found."));
    }

    /// A web link goes out as an open-URL request for Toolboks, which checks it again.
    #[test]
    fn a_web_link_asks_toolboks_to_open_it() {
        let ctx = egui::Context::default();
        let mut reader = Reader {
            file: Some(root().join("README.md")),
            root: Some(root()),
            ..Reader::default()
        };
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            reader.follow("https://example.com/", ui.ctx());
            reader.follow("mailto:a@example.com", ui.ctx());
        });
        let urls: Vec<String> = output
            .platform_output
            .commands
            .iter()
            .filter_map(|command| match command {
                egui::OutputCommand::OpenUrl(url) => Some(url.url.clone()),
                _ => None,
            })
            .collect();
        output.textures_delta.clear();
        assert_eq!(urls, ["https://example.com/"]);
    }
}
