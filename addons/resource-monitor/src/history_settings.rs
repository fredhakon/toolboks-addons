//! History settings and privacy (#45), shown at the top of the History page: the one-time note
//! that says what is saved and where, and one compact group with the "Record resource
//! history" switch, the size on disk, "Delete history" and "Open history folder".
//!
//! Recording is on by default (owner decision on #45). With the switch off nothing is recorded
//! or written for the history; the files saved earlier are offered for deletion. Toolboks still
//! saves its own settings (this switch among them), so the wording says "resource history", not
//! "nothing is written".

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use egui::RichText;
use toolboks_shared::format::size_plain;
use toolboks_shared::ui::{card, toggle_switch, AMBER, TEXT_SECTION};

use crate::recording::{self, Control, Recorder};

/// Set once the one-time note on the History page was dismissed.
pub const NOTE_SEEN_KEY: &str = "history_note_seen";
/// The group never stretches across a wide window (DESIGN.md: readable width).
const MAX_WIDTH: f32 = 640.0;
/// How often the size on disk is read again.
const SIZE_EVERY: Duration = Duration::from_secs(5);

pub const NOTE_TITLE: &str = "Resource history is saved on this PC";
pub const NOTE_TEXT: &str = "Resource Monitor records which apps used the processor, memory, \
graphics and disk, and when: app names, times and figures, never window titles, file names or \
what you do in an app. It records while Toolboks runs, also when the window is closed to the \
tray, and stops when you quit Toolboks. The history is kept for 7 days in Resource Monitor's \
data folder on this PC and is never sent anywhere.";
pub const SWITCH_LABEL: &str = "Record resource history";
pub const ON_TEXT: &str = "Saves app names, times and figures on this PC for 7 days. It records \
while Toolboks runs, also in the tray; quitting Toolboks stops it.";
pub const OFF_TEXT: &str = "Off: no resource history is recorded or written. Toolboks still \
saves its own settings, such as this switch.";

/// What the size line says. While recording, the files are written once a minute.
pub fn size_text(bytes: u64, recording: bool) -> String {
    if bytes == 0 && recording {
        "Nothing saved yet (saved once a minute)".to_string()
    } else if bytes == 0 {
        "No history saved".to_string()
    } else {
        format!("{} saved on this PC", size_plain(bytes))
    }
}

/// True when the one-time note should show.
pub fn note_due(prefs: &BTreeMap<String, serde_json::Value>) -> bool {
    !prefs
        .get(NOTE_SEEN_KEY)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// Sets the switch in the preferences and starts or stops the recorder.
pub fn set_recording(
    prefs: &mut BTreeMap<String, serde_json::Value>,
    control: &mut Control,
    on: bool,
    start: fn() -> Option<Recorder>,
) {
    prefs.insert(recording::PREF_KEY.into(), serde_json::Value::Bool(on));
    control.apply(on, start);
}

#[derive(Default)]
pub struct Settings {
    /// Size on disk and whether the folder exists, with when it was read.
    size: Option<(Instant, u64, bool)>,
    confirm_delete: bool,
    /// The note's "Recording settings" link moves keyboard focus to the switch.
    focus_switch: bool,
}

impl Settings {
    /// Draws the note (until dismissed) and the group. Returns true when the preferences
    /// changed.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        prefs: &mut BTreeMap<String, serde_json::Value>,
        control: &mut Control,
        dir: &Path,
        start: fn() -> Option<Recorder>,
    ) -> bool {
        let width = ui.available_width().min(MAX_WIDTH);
        let mut changed = false;
        if note_due(prefs) {
            changed |= self.note(ui, width, prefs);
        }
        changed |= self.group(ui, width, prefs, control, dir, start);
        changed
    }

    fn note(
        &mut self,
        ui: &mut egui::Ui,
        width: f32,
        prefs: &mut BTreeMap<String, serde_json::Value>,
    ) -> bool {
        let mut seen = false;
        card(ui, width, 0.0, |ui| {
            ui.label(RichText::new(NOTE_TITLE).size(TEXT_SECTION).strong());
            ui.label(NOTE_TEXT);
            ui.horizontal(|ui| {
                if ui.button("Got it").clicked() {
                    seen = true;
                }
                if ui
                    .link("Go to the switch")
                    .on_hover_text(format!("{SWITCH_LABEL}, below"))
                    .clicked()
                {
                    self.focus_switch = true;
                }
            });
        });
        if seen {
            prefs.insert(NOTE_SEEN_KEY.into(), serde_json::Value::Bool(true));
        }
        seen
    }

    fn group(
        &mut self,
        ui: &mut egui::Ui,
        width: f32,
        prefs: &mut BTreeMap<String, serde_json::Value>,
        control: &mut Control,
        dir: &Path,
        start: fn() -> Option<Recorder>,
    ) -> bool {
        if control.deleted().is_some() && self.confirm_delete {
            self.confirm_delete = false;
        }
        let fresh = self
            .size
            .is_some_and(|(at, ..)| at.elapsed() < SIZE_EVERY && !control.deleting());
        if !fresh {
            self.size = Some((
                Instant::now(),
                recording::disk::size_on_disk(dir),
                dir.is_dir(),
            ));
        }
        let (_, bytes, exists) = self.size.unwrap_or((Instant::now(), 0, false));
        let mut changed = false;
        card(ui, width, 0.0, |ui| {
            ui.horizontal(|ui| {
                let mut on = control.wanted();
                let r =
                    toggle_switch(ui, &mut on).on_hover_text(if on { ON_TEXT } else { OFF_TEXT });
                if std::mem::take(&mut self.focus_switch) {
                    r.request_focus();
                    r.scroll_to_me(Some(egui::Align::Center));
                }
                if r.changed() {
                    set_recording(prefs, control, on, start);
                    changed = true;
                }
                ui.label(RichText::new(SWITCH_LABEL).strong());
            });
            ui.label(RichText::new(if control.wanted() { ON_TEXT } else { OFF_TEXT }).weak());
            if control.deleting() {
                ui.label(RichText::new("Deleting the saved history...").weak());
            } else if self.confirm_delete {
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!(
                        "Delete all saved resource history ({})? This cannot be undone.",
                        size_plain(bytes)
                    ));
                    if ui.button("Delete").clicked() {
                        control.delete_history(dir.to_path_buf(), start);
                        self.size = None;
                    }
                    if ui.button("Cancel").clicked() {
                        self.confirm_delete = false;
                    }
                });
            } else {
                ui.horizontal_wrapped(|ui| {
                    ui.label(size_text(bytes, control.wanted()));
                    if !control.wanted() && bytes > 0 {
                        ui.label(RichText::new("· saved earlier, kept until you delete it").weak());
                    }
                    if ui
                        .add_enabled(bytes > 0, egui::Button::new("Delete history"))
                        .clicked()
                    {
                        self.confirm_delete = true;
                    }
                    if ui
                        .add_enabled(exists, egui::Button::new("Open history folder"))
                        .on_disabled_hover_text("Nothing has been saved yet")
                        .clicked()
                    {
                        open_folder(dir);
                    }
                });
                match control.deleted() {
                    Some(Ok(_)) if bytes == 0 => {
                        ui.label(RichText::new("History deleted.").weak());
                    }
                    Some(Err(e)) => {
                        ui.label(RichText::new(e).color(AMBER));
                    }
                    _ => {}
                }
            }
        });
        changed
    }
}

/// Opens the history folder in Explorer.
fn open_folder(dir: &Path) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let Ok(windows) = toolboks_shared::win::windows_dir() else {
        return;
    };
    let _ = std::process::Command::new(windows.join("explorer.exe"))
        .arg(dir)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_shows_until_dismissed() {
        let mut prefs = BTreeMap::new();
        assert!(note_due(&prefs));
        prefs.insert(NOTE_SEEN_KEY.into(), serde_json::json!(true));
        assert!(!note_due(&prefs));
    }

    #[test]
    fn wording_says_what_is_saved_where_and_for_how_long() {
        for needle in [
            "app names, times and figures",
            "never window titles",
            "closed to the tray",
            "quit Toolboks",
            "7 days",
            "on this PC",
            "never sent anywhere",
        ] {
            assert!(NOTE_TEXT.contains(needle), "{needle}");
        }
        assert!(ON_TEXT.contains("7 days") && ON_TEXT.contains("quitting Toolboks stops it"));
        // Off stops the history, not every write Toolboks makes.
        assert!(OFF_TEXT.contains("no resource history is recorded or written"));
        assert!(OFF_TEXT.contains("still saves its own settings"));
    }

    #[test]
    fn size_text_cases() {
        assert_eq!(size_text(0, false), "No history saved");
        assert!(size_text(0, true).starts_with("Nothing saved yet"));
        assert!(size_text(3 << 20, true).ends_with("saved on this PC"));
    }

    #[test]
    fn the_switch_writes_the_preference_and_drives_the_recorder() {
        fn fake() -> Option<Recorder> {
            recording::fake_recorder(Duration::from_secs(3600))
        }
        let mut prefs = BTreeMap::new();
        let mut control = Control::default();
        control.apply(recording::wanted(&prefs), fake);
        assert!(control.is_on(), "on by default");
        set_recording(&mut prefs, &mut control, false, fake);
        assert_eq!(prefs[recording::PREF_KEY], serde_json::json!(false));
        assert!(!control.is_on() && !control.wanted());
        assert!(!recording::wanted(&prefs), "stays off on the next start");
        set_recording(&mut prefs, &mut control, true, fake);
        let until = Instant::now() + Duration::from_secs(10);
        while !control.is_on() && Instant::now() < until {
            control.poll(fake);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(control.is_on());
        assert!(recording::wanted(&prefs));
    }
}
