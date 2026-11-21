//! SDK Add-on: writes a new, complete Toolboks add-on project into a folder the person chooses,
//! with `toolboks_addon_templates`. One page with two views: Create (what you need, the answers,
//! the folder, Create) and Reader (a Markdown file shown as a page, such as the new README).

pub mod markdown;
pub mod project;
mod reader;
mod shell;
mod toolchain;

use egui::{Color32, RichText, Vec2};
use project::Form;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};
use toolboks_shared::ui;
use toolchain::{Status, Toolchain};

pub const ID: &str = "toolboks.sdk-addon";
/// The saved preference: the folder chosen last.
const LAST_FOLDER: &str = "last_folder";
/// The widest the page's column grows.
const COLUMN: f32 = 680.0;
const LABEL_WIDTH: f32 = 110.0;

/// The two views of the page, switched at the top.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum View {
    #[default]
    Create,
    Reader,
}

#[derive(Default)]
pub struct SdkAddon {
    styled: bool,
    view: View,
    reader: reader::Reader,
    form: Form,
    advanced: bool,
    folder: Option<PathBuf>,
    toolchain: Option<Toolchain>,
    toolchain_check: Option<Receiver<Toolchain>>,
    picker: Option<Receiver<shell::Picked>>,
    picker_error: Option<String>,
    creating: Option<Receiver<Result<PathBuf, String>>>,
    outcome: Option<Result<PathBuf, String>>,
    opening: Option<Receiver<Result<(), String>>>,
    open_error: Option<String>,
    /// What happened when Install Rust was pressed: `Ok` once its PowerShell window opened.
    install: Option<Result<(), String>>,
}

/// A worker's answer, once it is there. A worker that ended without one counts as `gone`.
fn poll<T>(slot: &mut Option<Receiver<T>>, gone: impl FnOnce() -> T) -> Option<T> {
    let answer = match slot.as_ref()?.try_recv() {
        Ok(answer) => answer,
        Err(TryRecvError::Empty) => return None,
        Err(TryRecvError::Disconnected) => gone(),
    };
    *slot = None;
    Some(answer)
}

impl SdkAddon {
    /// Whether Create can be pressed: good answers, a folder that may be used, and no project
    /// being written.
    fn can_create(&self) -> bool {
        self.creating.is_none()
            && self.form.problem().is_none()
            && self
                .folder
                .as_deref()
                .is_some_and(|folder| folder_problem(folder).is_none())
    }

    /// Collects what the workers finished. Returns true when the saved folder changed.
    fn collect(&mut self, preferences: &mut BTreeMap<String, serde_json::Value>) -> bool {
        let mut changed = false;
        if let Some(found) = poll(&mut self.toolchain_check, || Toolchain {
            rust: Status::Unknown("The check stopped.".into()),
            build_tools: Status::Unknown("The check stopped.".into()),
        }) {
            self.toolchain = Some(found);
        }
        match poll(
            &mut self.picker,
            || Err("The folder picker stopped.".into()),
        ) {
            Some(Ok(Some(folder))) => {
                preferences.insert(
                    LAST_FOLDER.into(),
                    folder.to_string_lossy().into_owned().into(),
                );
                self.folder = Some(folder);
                self.picker_error = None;
                self.outcome = None;
                changed = true;
            }
            Some(Ok(None)) => {}
            Some(Err(error)) => self.picker_error = Some(error),
            None => {}
        }
        if let Some(outcome) = poll(&mut self.creating, || {
            Err("Creating the project stopped before it finished.".into())
        }) {
            self.outcome = Some(outcome);
        }
        if let Some(Err(error)) = poll(&mut self.opening, || Ok(())) {
            self.open_error = Some(error);
        }
        changed
    }

    /// Get started: the Reader view with the new project's README.
    fn get_started(&mut self, project: &Path, ctx: &egui::Context) {
        self.view = View::Reader;
        self.reader
            .open(project.join("README.md"), project.to_path_buf(), ctx);
    }

    /// The Create and Reader switch.
    fn view_switch(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add_space(ui::SPACE_L);
            egui::Frame::new()
                .fill(ui::PANEL_BG)
                .stroke(egui::Stroke::new(1.0, ui::BORDER))
                .corner_radius(ui::CONTROL_RADIUS + 2)
                .inner_margin(2.0)
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    for (view, name) in [(View::Create, "Create"), (View::Reader, "Reader")] {
                        if ui::pill(ui, name, self.view == view).clicked() {
                            self.view = view;
                        }
                    }
                });
        });
    }

    fn check_toolchain(&mut self, ctx: &egui::Context) {
        self.toolchain = Some(Toolchain::CHECKING);
        self.toolchain_check = Some(toolchain::start(ctx.clone()));
    }

    fn intro(&self, ui: &mut egui::Ui) {
        ui.label(RichText::new("SDK Add-on").size(ui::TEXT_TITLE).strong());
        ui.label(
            "Start your own Toolboks add-on. Fill in a few fields, choose a folder and press \
             Create: you get a complete Rust project that builds and passes its tests, a README \
             that shows which files to edit, and notes for AI coding assistants.",
        );
    }

    fn toolchain_card(&mut self, ui: &mut egui::Ui, width: f32) {
        section(ui, "What you need");
        let toolchain = self.toolchain.clone().unwrap_or(Toolchain::CHECKING);
        let mut again = false;
        ui::card(ui, width, 0.0, |ui| {
            status_row(ui, "Rust", &toolchain.rust);
            status_row(ui, "Visual C++ build tools", &toolchain.build_tools);
            let rust_missing = toolchain.rust == Status::Missing;
            if rust_missing {
                note(
                    ui,
                    "Press Install Rust: a PowerShell window opens and runs the official Rust \
                     installer (rustup), which also offers the Visual C++ build tools if they \
                     are missing. Nothing is installed until you press the button. When the \
                     window says it is done, press Check again.",
                    ui::MUTED,
                );
            } else if toolchain.build_tools == Status::Missing {
                note(
                    ui,
                    "Install the Visual Studio Build Tools with the \"Desktop development with \
                     C++\" workload, then press Check again.",
                    ui::MUTED,
                );
            }
            match &self.install {
                Some(Ok(())) => note(
                    ui,
                    "The installer is open in a PowerShell window. Follow it there.",
                    ui::MUTED,
                ),
                Some(Err(error)) => note(ui, error, ui::RED),
                None => {}
            }
            ui.horizontal(|ui| {
                if rust_missing && ui.add(primary("Install Rust")).clicked() {
                    self.install = Some(shell::install_rust());
                    ui.ctx().request_repaint();
                }
                let checking = self.toolchain_check.is_some();
                again = ui.add_enabled(!checking, outlined("Check again")).clicked();
            });
        });
        if again {
            self.install = None;
            self.check_toolchain(ui.ctx());
        }
    }

    fn answers_card(&mut self, ui: &mut egui::Ui, width: f32) {
        section(ui, "Your add-on");
        let form = &mut self.form;
        let advanced = &mut self.advanced;
        ui::card(ui, width, 0.0, |ui| {
            let field_width = (ui.available_width() - LABEL_WIDTH - ui::GAP).max(120.0);
            if field(
                ui,
                "Project name",
                &mut form.name,
                "word-counter",
                field_width,
            ) {
                form.name_changed();
            }
            caption_under(ui, "The folder, crate and program name.");
            if field(ui, "Title", &mut form.title, "Word counter", field_width) {
                form.title_changed();
            }
            caption_under(ui, "The name in the Toolboks sidebar.");
            field(ui, "Author", &mut form.author, "Your name", field_width);
            ui.horizontal(|ui| {
                label_cell(ui, "Accent colour");
                ui.add(
                    egui::TextEdit::singleline(&mut form.accent)
                        .hint_text("#3ecf8e")
                        .desired_width(90.0),
                );
                swatch(ui, project::accent_colour(&form.accent));
            });
            let chevron = if *advanced {
                "Hide advanced"
            } else {
                "Advanced"
            };
            if ui
                .add(egui::Button::new(RichText::new(chevron).color(ui::MUTED)).frame(false))
                .clicked()
            {
                *advanced = !*advanced;
            }
            if *advanced {
                if field(
                    ui,
                    "Add-on id",
                    &mut form.id,
                    "local.word-counter",
                    field_width,
                ) {
                    form.id_changed();
                }
                caption_under(
                    ui,
                    "How Toolboks tells add-ons apart. Keep the suggestion for your own use; \
                     to publish, use one with your name, like com.yourname.word-counter.",
                );
            }
            ui.add_space(ui::SPACE_XS);
            if form.name.trim().is_empty() {
                note(ui, "Start with a project name.", ui::MUTED);
            } else if let Some(problem) = form.problem() {
                note(ui, &problem, ui::RED);
            }
            note(
                ui,
                "The description is a placeholder; the README tells you where to change it.",
                ui::MUTED,
            );
        });
    }

    fn folder_card(&mut self, ui: &mut egui::Ui, width: f32) {
        section(ui, "Where");
        let mut choose = false;
        ui::card(ui, width, 0.0, |ui| {
            ui.horizontal(|ui| {
                let picking = self.picker.is_some();
                let label = if picking {
                    "Choosing..."
                } else {
                    "Choose folder..."
                };
                choose = ui.add_enabled(!picking, outlined(label)).clicked();
                match &self.folder {
                    Some(folder) => ui.label(folder.display().to_string()),
                    None => ui.label(RichText::new("No folder chosen").color(ui::MUTED)),
                };
            });
            if let Some(error) = &self.picker_error {
                note(ui, error, ui::RED);
            }
            match &self.folder {
                Some(folder) => match folder_problem(folder) {
                    Some(problem) => note(ui, &problem, ui::RED),
                    None => {
                        let name = self.form.name.trim();
                        let name = if name.is_empty() {
                            "<project name>"
                        } else {
                            name
                        };
                        note(
                            ui,
                            &format!(
                                "The project goes into a new folder: {}",
                                folder.join(name).display()
                            ),
                            ui::MUTED,
                        );
                    }
                },
                None => note(
                    ui,
                    "The project goes into a new folder inside the one you choose. Nothing is \
                     written until you press Create, and no file is ever replaced.",
                    ui::MUTED,
                ),
            }
        });
        if choose {
            self.picker = Some(shell::pick_folder(self.folder.clone(), ui.ctx().clone()));
        }
    }

    fn create_row(&mut self, ui: &mut egui::Ui) {
        let primary_here = !matches!(self.outcome, Some(Ok(_)));
        let label = if self.creating.is_some() {
            "Creating..."
        } else {
            "Create project"
        };
        let button = if primary_here {
            primary(label)
        } else {
            outlined(label)
        };
        if ui.add_enabled(self.can_create(), button).clicked() {
            if let Some(folder) = self.folder.clone() {
                let params = self.form.params();
                let ctx = ui.ctx().clone();
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let _ = tx.send(project::create(&params, &folder));
                    ctx.request_repaint();
                });
                self.creating = Some(rx);
                self.outcome = None;
                self.open_error = None;
            }
        }
    }

    fn outcome_card(&mut self, ui: &mut egui::Ui, width: f32) {
        let Some(outcome) = self.outcome.clone() else {
            return;
        };
        ui.add_space(ui::SPACE_M);
        ui::card(ui, width, 0.0, |ui| match outcome {
            Ok(project) => {
                ui.label(
                    RichText::new("Project created")
                        .size(ui::TEXT_SECTION)
                        .strong(),
                );
                ui.label(project.display().to_string());
                ui.horizontal(|ui| {
                    if ui.add(primary("Get started (README)")).clicked() {
                        self.open_error = None;
                        self.get_started(&project, ui.ctx());
                    }
                    if ui
                        .add_enabled(self.opening.is_none(), outlined("Open in another app"))
                        .on_hover_text("Opens the README with the program Windows uses for .md files, or Notepad.")
                        .clicked()
                    {
                        self.open_error = None;
                        self.opening = Some(shell::open_readme(
                            project.join("README.md"),
                            ui.ctx().clone(),
                        ));
                    }
                    if ui.add(outlined("Open folder")).clicked() {
                        self.open_error = shell::open_folder(&project).err();
                    }
                });
                note(
                    ui,
                    "Next: open a terminal in that folder and run cargo test. The README \
                     explains the rest.",
                    ui::MUTED,
                );
                if let Some(error) = &self.open_error {
                    note(ui, error, ui::RED);
                }
            }
            Err(error) => {
                ui.label(
                    RichText::new("The project was not created")
                        .size(ui::TEXT_SECTION)
                        .strong(),
                );
                note(ui, &error, ui::RED);
            }
        });
    }
}

/// Why a project may not go into `folder` on this PC.
fn folder_problem(folder: &Path) -> Option<String> {
    project::folder_problem(folder, &|name| std::env::var_os(name))
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(ui::SPACE_M);
    ui.label(RichText::new(title).size(ui::TEXT_SECTION).strong());
    ui.add_space(ui::SPACE_XS);
}

/// One line of caption text, wrapped, in `colour`.
fn note(ui: &mut egui::Ui, text: &str, colour: Color32) {
    ui.label(RichText::new(text).size(ui::TEXT_CAPTION).color(colour));
}

fn label_cell(ui: &mut egui::Ui, text: &str) {
    ui.add_sized(
        [LABEL_WIDTH, 20.0],
        egui::Label::new(text).halign(egui::Align::LEFT),
    );
}

/// A caption under a field, lined up with the field.
fn caption_under(ui: &mut egui::Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(LABEL_WIDTH + ui.spacing().item_spacing.x);
        ui.vertical(|ui| note(ui, text, ui::MUTED));
    });
}

/// A labelled one-line text field. Returns true when the text changed.
fn field(ui: &mut egui::Ui, label: &str, text: &mut String, hint: &str, width: f32) -> bool {
    ui.horizontal(|ui| {
        label_cell(ui, label);
        ui.add(
            egui::TextEdit::singleline(text)
                .hint_text(hint)
                .desired_width(width),
        )
        .changed()
    })
    .inner
}

/// A small square of the accent colour, or an empty outline when the text is not a colour.
fn swatch(ui: &mut egui::Ui, colour: Option<[u8; 3]>) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(16.0), egui::Sense::hover());
    let radius = f32::from(ui::CONTROL_RADIUS) / 2.0;
    match colour {
        Some([r, g, b]) => ui
            .painter()
            .rect_filled(rect, radius, Color32::from_rgb(r, g, b)),
        None => ui.painter().rect_stroke(
            rect,
            radius,
            egui::Stroke::new(1.0, ui::BORDER),
            egui::StrokeKind::Inside,
        ),
    };
}

/// A status dot, the status word and what was found. The word carries the meaning; the dot only
/// repeats it.
fn status_row(ui: &mut egui::Ui, name: &str, status: &Status) {
    let (word, colour, detail) = match status {
        Status::Checking => ("Checking...", ui::MUTED, String::new()),
        Status::Found(what) => ("Found", ui::GREEN, what.clone()),
        Status::Missing => ("Missing", ui::RED, "Not installed".into()),
        Status::Unknown(why) => ("Could not check", ui::AMBER, why.clone()),
    };
    ui.horizontal(|ui| {
        let (dot, _) = ui.allocate_exact_size(Vec2::splat(10.0), egui::Sense::hover());
        ui.painter().circle_filled(dot.center(), 4.0, colour);
        ui.add_sized(
            [LABEL_WIDTH + 50.0, 18.0],
            egui::Label::new(RichText::new(name).strong()).halign(egui::Align::LEFT),
        );
        ui.add_sized(
            [100.0, 18.0],
            egui::Label::new(RichText::new(word).color(colour)).halign(egui::Align::LEFT),
        );
        if !detail.is_empty() {
            ui.label(
                RichText::new(detail)
                    .size(ui::TEXT_CAPTION)
                    .color(ui::MUTED),
            );
        }
    });
}

/// The page's one accent-filled button.
fn primary(text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.to_owned()).color(Color32::WHITE))
        .fill(ui::HOST_ACCENT_FILL)
        .min_size(Vec2::new(0.0, 28.0))
}

fn outlined(text: &str) -> egui::Button<'static> {
    egui::Button::new(text.to_owned())
        .fill(Color32::TRANSPARENT)
        .stroke(egui::Stroke::new(1.0, ui::BORDER))
        .min_size(Vec2::new(0.0, 28.0))
}

impl toolboks_addon_sdk::NativeAddon for SdkAddon {
    fn load_preferences(&mut self, preferences: &BTreeMap<String, serde_json::Value>) {
        self.folder = preferences
            .get(LAST_FOLDER)
            .and_then(serde_json::Value::as_str)
            .filter(|folder| !folder.trim().is_empty())
            .map(PathBuf::from);
    }

    fn ui(
        &mut self,
        ui: &mut egui::Ui,
        preferences: &mut BTreeMap<String, serde_json::Value>,
    ) -> bool {
        if !self.styled {
            ui::apply_style(ui.ctx());
            self.styled = true;
        }
        if self.toolchain.is_none() {
            self.check_toolchain(ui.ctx());
        }
        let changed = self.collect(preferences);
        self.reader.collect(ui.ctx());
        ui.add_space(ui::SPACE_M);
        self.view_switch(ui);
        if self.view == View::Reader {
            ui.add_space(ui::SPACE_M);
            self.reader.ui(ui, self.folder.as_deref());
            return changed;
        }
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(ui::SPACE_M);
                ui.horizontal(|ui| {
                    ui.add_space(ui::SPACE_L);
                    let width = (ui.available_width() - ui::SPACE_L).clamp(320.0, COLUMN);
                    ui.vertical(|ui| {
                        ui.set_width(width);
                        self.intro(ui);
                        self.toolchain_card(ui, width);
                        self.answers_card(ui, width);
                        self.folder_card(ui, width);
                        ui.add_space(ui::SPACE_M);
                        self.create_row(ui);
                        self.outcome_card(ui, width);
                        ui.add_space(ui::SPACE_L);
                    });
                });
            });
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use toolboks_addon_sdk::NativeAddon;

    fn frame(addon: &mut SdkAddon, preferences: &mut BTreeMap<String, serde_json::Value>) -> bool {
        let ctx = egui::Context::default();
        let mut changed = false;
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(900.0, 700.0),
                )),
                ..Default::default()
            },
            |ui| changed = addon.ui(ui, preferences),
        );
        output.textures_delta.clear();
        changed
    }

    #[test]
    fn the_last_folder_is_restored_and_a_blank_one_ignored() {
        let mut addon = SdkAddon::default();
        let saved = BTreeMap::from([(LAST_FOLDER.to_string(), r"D:\Projects".into())]);
        addon.load_preferences(&saved);
        assert_eq!(addon.folder, Some(PathBuf::from(r"D:\Projects")));
        let blank = BTreeMap::from([(LAST_FOLDER.to_string(), " ".into())]);
        addon.load_preferences(&blank);
        assert_eq!(addon.folder, None);
    }

    #[test]
    fn create_waits_for_good_answers_and_a_usable_folder() {
        let dir = tempfile::tempdir().unwrap();
        let mut addon = SdkAddon::default();
        assert!(!addon.can_create());
        addon.form.name = "counter".into();
        addon.form.name_changed();
        addon.form.author = "Ada".into();
        assert!(!addon.can_create(), "no folder yet");
        addon.folder = Some(PathBuf::from(r"C:\"));
        assert!(!addon.can_create(), "a drive root");
        addon.folder = Some(dir.path().to_path_buf());
        assert!(addon.can_create());
        addon.form.accent = "green".into();
        assert!(!addon.can_create());
    }

    /// With Rust missing the page offers Install Rust, and draws both with and without an error
    /// from the installer window. Nothing is installed by drawing.
    #[test]
    fn a_missing_rust_offers_the_install_button_without_installing() {
        let mut addon = SdkAddon {
            toolchain: Some(Toolchain {
                rust: Status::Missing,
                build_tools: Status::Missing,
            }),
            ..SdkAddon::default()
        };
        let mut preferences = BTreeMap::new();
        frame(&mut addon, &mut preferences);
        assert!(addon.install.is_none());
        addon.install = Some(Err("Could not open PowerShell: gone".into()));
        frame(&mut addon, &mut preferences);
        addon.install = Some(Ok(()));
        frame(&mut addon, &mut preferences);
    }

    /// The page draws headlessly with nothing chosen and with a result, and the toolchain check
    /// starts on the first frame without blocking it.
    #[test]
    fn the_page_draws_and_a_chosen_folder_is_saved() {
        let mut addon = SdkAddon::default();
        let mut preferences = BTreeMap::new();
        assert!(!frame(&mut addon, &mut preferences));
        assert!(addon.toolchain.is_some());
        // A folder from the picker is saved as the last folder.
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Ok(Some(PathBuf::from(r"D:\Projects")))).unwrap();
        addon.picker = Some(rx);
        assert!(frame(&mut addon, &mut preferences));
        assert_eq!(preferences[LAST_FOLDER], r"D:\Projects");
        // A cancelled picker changes nothing.
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Ok(None)).unwrap();
        addon.picker = Some(rx);
        assert!(!frame(&mut addon, &mut preferences));
        assert_eq!(addon.folder, Some(PathBuf::from(r"D:\Projects")));
        // Both outcomes draw.
        addon.outcome = Some(Ok(PathBuf::from(r"D:\Projects\counter")));
        frame(&mut addon, &mut preferences);
        addon.outcome = Some(Err(
            "D:\\Projects\\counter already exists and is not empty.".into(),
        ));
        frame(&mut addon, &mut preferences);
    }

    /// The README a new project gets, as the templates crate writes it.
    fn generated_readme() -> Vec<u8> {
        let params = Form::new("counter", "Ada").params();
        toolboks_addon_templates::generate(&params)
            .unwrap()
            .into_iter()
            .find(|file| file.path == "README.md")
            .unwrap()
            .bytes
    }

    /// The generated README parses into the headings, table, list and code a reader shows.
    #[test]
    fn the_generated_readme_renders() {
        let text = String::from_utf8(generated_readme()).unwrap();
        let doc = markdown::parse(&text);
        let has = |pred: &dyn Fn(&markdown::Block) -> bool| doc.blocks.iter().any(pred);
        assert!(has(&|b| matches!(
            b,
            markdown::Block::Heading { level: 1, .. }
        )));
        assert!(has(&|b| matches!(
            b,
            markdown::Block::Table { rows, .. } if !rows.is_empty()
        )));
        assert!(has(&|b| matches!(
            b,
            markdown::Block::List { start: Some(1), .. }
        )));
        assert!(has(&|b| matches!(b, markdown::Block::Code(_))));
        assert!(doc.has_anchor("the-five-edits-in-order"));
    }

    /// The switch changes the view; the Reader draws empty, with a document and with an error.
    #[test]
    fn the_reader_view_draws_empty_with_a_document_and_with_an_error() {
        let mut addon = SdkAddon::default();
        let mut preferences = BTreeMap::new();
        frame(&mut addon, &mut preferences);
        assert_eq!(addon.view, View::Create);
        addon.view = View::Reader;
        assert!(!frame(&mut addon, &mut preferences), "nothing is saved");
        assert!(addon.reader.doc.is_none());
        let readme = String::from_utf8(generated_readme()).unwrap();
        let extra = "\n\n> quote with [a link](https://example.com)\n\n- [x] done\n- [ ] open\n\n\
                     | a | b |\n|--:|:-:|\n| [l](x.md) | ~~s~~ |\n\n![alt](i.png)\n\n\
                     <p>html</p>\n\n---\n";
        addon.reader.doc = Some(markdown::parse(&format!("{readme}{extra}")));
        addon.reader.file = Some(PathBuf::from(r"D:\Projects\counter\README.md"));
        frame(&mut addon, &mut preferences);
        addon.reader.error = Some("README.md was not found.".into());
        addon.reader.lossy = true;
        addon.reader.notice = Some("Email links are not opened from the reader.".into());
        frame(&mut addon, &mut preferences);
        addon.reader.doc = None;
        frame(&mut addon, &mut preferences);
        assert!(preferences.is_empty(), "the reader remembers no file");
    }

    /// Get started switches to the Reader and reads the project's README there; it starts no
    /// other program.
    #[test]
    fn get_started_opens_the_readme_in_the_reader() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("counter");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join("README.md"), generated_readme()).unwrap();
        let mut addon = SdkAddon {
            outcome: Some(Ok(project.clone())),
            ..SdkAddon::default()
        };
        let ctx = egui::Context::default();
        addon.get_started(&project, &ctx);
        assert_eq!(addon.view, View::Reader);
        assert!(addon.opening.is_none(), "no other app is started");
        addon.reader.wait();
        assert_eq!(addon.reader.file, Some(project.join("README.md")));
        assert_eq!(addon.reader.root.as_deref(), Some(project.as_path()));
        assert!(addon.reader.error.is_none());
        let doc = addon.reader.doc.as_ref().unwrap();
        assert!(doc.has_anchor("the-five-edits-in-order"));
        let mut preferences = BTreeMap::new();
        frame(&mut addon, &mut preferences);
    }
}
