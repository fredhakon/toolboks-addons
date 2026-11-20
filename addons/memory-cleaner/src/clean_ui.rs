//! The window side of issue #231: the ask-before-clean prompt, the result line on Home, and the
//! clean-log switches. The arithmetic is in `freed.rs`, the log in `clean_log.rs`.

use egui::RichText;

use super::clean_log::{self, MAX_BYTES, MAX_ENTRIES};
use super::freed::{Change, RunResult};
use super::report::{self, Trigger};
use super::style::section;
use super::Memory;
use toolboks_addon_sdk::TrayItem;

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

/// The id of the one tray entry; the host sends it back to `tray_action`.
const TRAY_CLEAN_NOW: &str = "clean-now";
const TRAY_LABEL: &str = "Clean now";
const TRAY_LABEL_BUSY: &str = "Clean now (running)";
/// Said in the entry itself: a prompt cannot be seen while the page is hidden, and the add-on
/// has no way to raise its page, so the entry waits for the page instead of cleaning unasked.
const TRAY_LABEL_CONFIRM: &str = "Clean now (open Memory Cleaner to confirm)";

const BUSY_TEXT: &str = "A run is already going. Try again when it has finished.";

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
        // An automatic run can start while the prompt is open; say why nothing happens.
        if self.busy() {
            self.status = Some(BUSY_TEXT.to_string());
            return;
        }
        match action {
            Confirm::TrimIdle => {
                self.start_run(ctx, Trigger::Manual);
            }
            Confirm::TrimSelected => self.start_trim(ctx),
        }
    }

    /// Why the tray "Clean now" cannot be used right now, as the label of the entry; `None` if
    /// it can. Never runs a clean the user was not asked about: with the ask-first switch on,
    /// only a shown page can draw the prompt.
    fn clean_now_blocked(&self) -> Option<&'static str> {
        if self.busy() {
            Some(TRAY_LABEL_BUSY)
        } else if self.options.confirm_manual && !self.active {
            Some(TRAY_LABEL_CONFIRM)
        } else {
            None
        }
    }

    /// The tray menu: one "Clean now" entry, the same run as Home "Trim idle apps".
    pub(super) fn clean_now_tray_items(&self) -> Vec<TrayItem> {
        let blocked = self.clean_now_blocked();
        vec![TrayItem {
            id: TRAY_CLEAN_NOW.to_string(),
            label: blocked.unwrap_or(TRAY_LABEL).to_string(),
            enabled: blocked.is_none(),
        }]
    }

    /// The user chose a tray entry: ask first or run, exactly like the Home button. An unknown
    /// id, or a state the entry was offered disabled in, does nothing.
    pub(super) fn clean_now_tray_action(&mut self, id: &str) {
        // An open prompt (for example "Trim selected") is left alone, not swapped for this one.
        if id != TRAY_CLEAN_NOW
            || self.clean_now_blocked().is_some()
            || self.pending_confirm.is_some()
        {
            return;
        }
        // Never ticked (started hidden): a throwaway context. Nothing is lost, because
        // `tick_background` polls the run every 200 ms.
        let ctx = self.ctx.clone().unwrap_or_default();
        self.ask_or_run(&ctx, Confirm::TrimIdle);
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

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;
    use crate::{RefreshAction, RefreshWorker};

    /// Puts a fake sampling worker in place, so a run that starts is seen on the returned
    /// receiver and no thread reads Windows.
    fn fake_worker(memory: &mut Memory) -> mpsc::Receiver<Option<RefreshAction>> {
        let (request, requests) = mpsc::channel();
        let (_results, result) = mpsc::channel();
        memory.refresh_worker = Some(RefreshWorker { request, result });
        requests
    }

    fn entry(memory: &Memory) -> TrayItem {
        let mut items = memory.clean_now_tray_items();
        assert_eq!(items.len(), 1);
        items.remove(0)
    }

    #[test]
    fn the_tray_entry_waits_for_the_page_when_it_must_ask() {
        let mut memory = Memory::for_test();
        assert!(memory.options.confirm_manual, "asking is the default");
        memory.active = false;
        let hidden = entry(&memory);
        assert_eq!(hidden.id, "clean-now");
        assert_eq!(hidden.label, "Clean now (open Memory Cleaner to confirm)");
        assert!(!hidden.enabled);
        assert!(hidden.label.chars().count() <= 60, "the host clips at 60");

        memory.active = true;
        assert_eq!(
            entry(&memory),
            TrayItem {
                id: "clean-now".into(),
                label: "Clean now".into(),
                enabled: true
            }
        );

        // Not asking: it can run with the page hidden.
        memory.active = false;
        memory.options.confirm_manual = false;
        assert!(entry(&memory).enabled);
        assert_eq!(entry(&memory).label, "Clean now");
    }

    #[test]
    fn the_tray_entry_is_disabled_while_a_job_is_busy() {
        for confirm in [true, false] {
            let mut memory = Memory::for_test();
            memory.active = true;
            memory.options.confirm_manual = confirm;
            memory.refresh_action_pending = true;
            let busy = entry(&memory);
            assert!(!busy.enabled);
            assert_eq!(busy.label, "Clean now (running)");
        }
    }

    #[test]
    fn choosing_the_tray_entry_with_the_page_shown_asks_first() {
        let mut memory = Memory::for_test();
        memory.active = true;
        let requests = fake_worker(&mut memory);
        memory.clean_now_tray_action("clean-now");
        assert_eq!(memory.pending_confirm, Some(Confirm::TrimIdle));
        assert!(
            requests.try_recv().is_err(),
            "nothing runs before the answer"
        );
        assert!(!memory.busy());
    }

    #[test]
    fn choosing_the_tray_entry_without_asking_runs_the_same_manual_run_even_hidden() {
        let mut memory = Memory::for_test();
        memory.active = false;
        memory.options.confirm_manual = false;
        let requests = fake_worker(&mut memory);
        memory.clean_now_tray_action("clean-now");
        assert_eq!(memory.pending_confirm, None);
        assert!(matches!(
            requests.try_recv(),
            Ok(Some(RefreshAction::Optimize(Trigger::Manual)))
        ));
        assert!(memory.busy());
    }

    #[test]
    fn a_tray_click_leaves_an_open_prompt_alone() {
        let mut memory = Memory::for_test();
        memory.active = true;
        memory.pending_confirm = Some(Confirm::TrimSelected);
        memory.clean_now_tray_action("clean-now");
        assert_eq!(memory.pending_confirm, Some(Confirm::TrimSelected));
    }

    #[test]
    fn the_tray_action_never_cleans_unasked() {
        // Hidden with asking on (the entry is offered disabled), an unknown id, and busy.
        let mut memory = Memory::for_test();
        memory.active = false;
        let requests = fake_worker(&mut memory);
        memory.clean_now_tray_action("clean-now");
        memory.options.confirm_manual = false;
        memory.clean_now_tray_action("something-else");
        memory.clean_now_tray_action("");
        memory.refresh_action_pending = true;
        memory.clean_now_tray_action("clean-now");
        assert!(requests.try_recv().is_err());
        assert_eq!(memory.pending_confirm, None);
        assert_eq!(memory.status, None, "no busy text either: it did nothing");
    }

    #[test]
    fn asking_is_the_default_and_starts_nothing_until_answered() {
        let mut memory = Memory::for_test();
        memory.ask_or_run(&egui::Context::default(), Confirm::TrimIdle);
        assert_eq!(memory.pending_confirm, Some(Confirm::TrimIdle));
        assert!(!memory.busy());
    }

    #[test]
    fn answering_while_another_job_runs_says_so_instead_of_doing_nothing() {
        let mut memory = Memory::for_test();
        memory.options.confirm_manual = false;
        memory.refresh_action_pending = true;
        memory.ask_or_run(&egui::Context::default(), Confirm::TrimSelected);
        assert_eq!(memory.pending_confirm, None);
        assert_eq!(memory.status.as_deref(), Some(BUSY_TEXT));
    }

    #[test]
    fn a_run_without_readable_numbers_is_recorded_without_claims() {
        let mut memory = Memory::for_test();
        memory.record_result(
            Trigger::Interval,
            vec!["Idle apps".into()],
            None,
            Vec::new(),
        );
        let result = memory.last_result.unwrap();
        assert_eq!(result.change, None);
        assert!(result.headline().starts_with("Auto interval, "));
    }
}
