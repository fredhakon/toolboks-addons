//! The window side of issue #231: the ask-before-clean prompt, the result line on Home, and the
//! clean-log switches. The arithmetic is in `freed.rs`, the log in `clean_log.rs`.

use egui::RichText;

use super::clean_log::{self, MAX_BYTES, MAX_ENTRIES};
use super::freed::{Change, RunResult};
use super::report::{self, Trigger};
use super::style::section;
use super::Memory;

/// A manual action the add-on runs by itself, which can ask first.
///
/// Only these ask. "Clear low-priority cache" and "Clean selected regions" already end in the
/// administrator helper's own Confirm / Cancel window for the same action, so a prompt of ours
/// in front of it would only be a second click. Automatic runs never ask.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confirm {
    TrimIdle,
    TrimSelected,
}

impl Confirm {
    fn title(self, selected: usize) -> String {
        match self {
            Confirm::TrimIdle => "Trim idle apps now?".to_string(),
            Confirm::TrimSelected => format!("Trim the {selected} selected rows now?"),
        }
    }

    fn body(self) -> &'static str {
        match self {
            Confirm::TrimIdle => {
                "This lowers the memory priority of apps you have not used for a while and asks \
                 them to move unused memory to the cache. Windows does this by itself when it \
                 needs to, so you may notice no difference."
            }
            Confirm::TrimSelected => {
                "This asks the selected apps to move unused memory to the cache. Protected apps \
                 and games are skipped. Windows does this by itself when it needs to, so you may \
                 notice no difference."
            }
        }
    }
}

impl Memory {
    /// Runs a manual action now, or asks first when the preference says so (the default).
    pub(super) fn ask_or_run(&mut self, ctx: &egui::Context, action: Confirm) {
        if self.options.confirm_manual {
            self.pending_confirm = Some(action);
        } else {
            self.run_confirmed(ctx, action);
        }
    }

    fn run_confirmed(&mut self, ctx: &egui::Context, action: Confirm) {
        match action {
            Confirm::TrimIdle => {
                self.start_run(ctx, Trigger::Manual);
            }
            Confirm::TrimSelected => self.start_trim(ctx),
        }
    }

    /// Draws the prompt while one is pending.
    pub(super) fn confirm_ui(&mut self, ctx: &egui::Context) {
        let Some(action) = self.pending_confirm else {
            return;
        };
        let (mut run, mut cancel) = (false, false);
        let modal = egui::Modal::new(egui::Id::unique("memory-cleaner-confirm")).show(ctx, |ui| {
            ui.set_max_width(420.0);
            section(ui, &action.title(self.selection.len()));
            ui.add(egui::Label::new(action.body()).wrap());
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                run = ui.button("Trim").clicked();
                cancel = ui.button("Cancel").clicked();
            });
        });
        if run || cancel || modal.should_close() {
            self.pending_confirm = None;
        }
        if run {
            self.run_confirmed(ctx, action);
        }
    }

    /// Remembers the last clean for Home and, if the log is on, appends it there. A log that
    /// cannot be written is said so, never silently skipped.
    pub(super) fn record_result(
        &mut self,
        trigger: Trigger,
        what: Vec<String>,
        change: Option<Change>,
        detail: Vec<String>,
    ) {
        let result = RunResult {
            trigger,
            when: toolboks_shared::win::local_time_hms(),
            what,
            change,
            detail,
        };
        if self.options.log {
            let entry = clean_log::Entry::new(&result, super::health_feed::unix_now());
            match clean_log::append(&mut self.log_store, entry) {
                Ok(kept) => {
                    self.log_count = kept;
                    self.log_note = None;
                }
                Err(e) => self.log_note = Some(format!("Could not write the clean log: {e}")),
            }
        }
        self.last_result = Some(result);
    }

    /// Home: source, time, freed, standby and modified change, and the disk-read note.
    pub(super) fn result_line(&self, ui: &mut egui::Ui) {
        let Some(result) = &self.last_result else {
            return;
        };
        section(ui, "Last result");
        ui.label(RichText::new(result.headline()).small());
        for line in &result.detail {
            ui.label(RichText::new(line).weak().small());
        }
        ui.label(RichText::new(report::honest_note()).weak().small());
    }

    /// Rules tab: the ask-first switch and the clean log.
    pub(super) fn clean_options_ui(&mut self, ui: &mut egui::Ui) {
        section(ui, "Confirmation and log");
        ui.checkbox(&mut self.options.confirm_manual, "Ask before a manual trim");
        ui.label(
            RichText::new(
                "Applies to Trim idle apps and Trim selected. The administrator actions ask in \
                 their own window, and automatic runs never ask.",
            )
            .weak()
            .small(),
        );
        ui.checkbox(&mut self.options.log, "Keep a local log of cleans");
        ui.label(
            RichText::new(format!(
                "Off by default. Kept only on this PC, in the Toolboks data folder, and never \
                 sent anywhere. Each entry holds the source, what was cleaned, how much was freed \
                 and the time: no app names, no file paths. At most {MAX_ENTRIES} entries and \
                 {} KB, oldest dropped first.",
                MAX_BYTES / 1024
            ))
            .weak()
            .small(),
        );
        ui.horizontal(|ui| {
            ui.label(format!("{} saved", self.log_count));
            if ui
                .add_enabled(self.log_count > 0, egui::Button::new("Delete log"))
                .clicked()
            {
                self.delete_log();
            }
        });
        if let Some(note) = &self.log_note {
            ui.label(RichText::new(note).small());
        }
    }

    fn delete_log(&mut self) {
        use clean_log::Store;
        match self.log_store.delete() {
            Ok(()) => {
                self.log_count = 0;
                self.log_note = None;
            }
            Err(e) => self.log_note = Some(format!("Could not delete the clean log: {e}")),
        }
    }
}
