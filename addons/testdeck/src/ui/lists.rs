//! The two suite lists, edited as plain text: flaky suites (part of the config, saved with Settings' Apply) and the
//! known-red list (`known-red.txt` in the data folder, saved on its own).
use super::ok_color;
use crate::lanes::data_dir;
use crate::state::Shared;
use egui::Ui;
use std::path::Path;

const KNOWN_RED_HEADER: &str =
    "# Suites that are red on a clean origin/staging: one path or path fragment per line.\n";

#[derive(Default)]
pub(crate) struct State {
    flaky_text: String,
    known: Known,
}

#[derive(Default)]
struct Known {
    text: String,
    /// What the file held at the last Load or Save, to tell unsaved edits.
    on_disk: String,
    loaded: bool,
    /// Ok = what happened, Err = what went wrong.
    note: Option<Result<String, String>>,
}

fn parse(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

/// One suite path or fragment per line; the list is `d.flaky_suites` of the Settings draft.
pub(crate) fn flaky(ui: &mut Ui, st: &mut State, list: &mut Vec<String>) {
    ui.label(egui::RichText::new("Flaky suites").strong());
    ui.weak("A run whose only failures are listed here reruns them once alone. One path or fragment per line. Saved with Apply.");
    // A draft that changed from outside (Revert) wins over the text; typing keeps both equal.
    if parse(&st.flaky_text) != *list {
        st.flaky_text = list.join("\n");
    }
    let edit = egui::TextEdit::multiline(&mut st.flaky_text)
        .font(egui::TextStyle::Monospace)
        .desired_rows(4)
        .desired_width(f32::INFINITY);
    if ui.add(edit).changed() {
        *list = parse(&st.flaky_text);
    }
}

pub(crate) fn known_red(ui: &mut Ui, st: &mut State, shared: &Shared) {
    let path = data_dir(shared).join("known-red.txt");
    let k = &mut st.known;
    if !k.loaded {
        load(k, &path);
    }
    ui.label(egui::RichText::new("Known red on staging").strong());
    ui.weak("Failures matching a line are tagged \"known red on staging\" in the summary. One path or fragment per line; # starts a comment.");
    let edit = egui::TextEdit::multiline(&mut k.text)
        .font(egui::TextStyle::Monospace)
        .desired_rows(6)
        .desired_width(f32::INFINITY);
    ui.add(edit);
    let dirty = k.text != k.on_disk;
    ui.horizontal(|ui| {
        if ui.add_enabled(dirty, egui::Button::new("Save")).clicked() {
            k.note = Some(save(k, &path));
        }
        if ui
            .button("Load")
            .on_hover_text("Reads the file again and drops unsaved edits")
            .clicked()
        {
            load(k, &path);
        }
        match &k.note {
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, e.as_str());
            }
            Some(Ok(m)) if !dirty => {
                ui.colored_label(ok_color(ui), m.as_str());
            }
            _ if dirty => {
                ui.weak("Unsaved changes");
            }
            _ => {}
        }
        ui.weak(path.display().to_string());
    });
}

/// Reads the file; a missing one is created with a header comment.
fn load(k: &mut Known, path: &Path) {
    k.loaded = true;
    match read_or_create(path) {
        Ok(t) => {
            k.on_disk = t.clone();
            k.text = t;
            k.note = Some(Ok("Loaded.".into()));
        }
        Err(e) => k.note = Some(Err(format!("Could not read known-red.txt: {e}"))),
    }
}

fn read_or_create(path: &Path) -> std::io::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(t.replace("\r\n", "\n")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(path, KNOWN_RED_HEADER)?;
            Ok(KNOWN_RED_HEADER.to_string())
        }
        Err(e) => Err(e),
    }
}

fn save(k: &mut Known, path: &Path) -> Result<String, String> {
    let mut text = k.text.replace("\r\n", "\n");
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    std::fs::write(path, &text).map_err(|e| format!("Could not save known-red.txt: {e}"))?;
    k.text = text.clone();
    k.on_disk = text;
    Ok("Saved.".into())
}
