//! The Debloater pages: a centered item list and a right-hand Review inspector.

use std::sync::mpsc;

use egui::{self, Color32, RichText};

use super::apps::{self, AppRow, AppRun};
use super::model::{Applicable, Edition, Group, Hive, Item, RegValue, Risk, State, Status};
use super::restore::{self, RestoreStatus};
use super::saved_undo::{self, Loaded, RegStep, ServiceStep, StepResult, UndoList};
use super::services::{self, ServiceDef, ServiceState};
use super::startup::{self, Class, Entry, Source, StartupStore};
use super::style::{self, Chip};
use super::tasks::{self, TaskDef, TaskState};
use super::{
    admin, backend, catalog, plan, AdminKind, Debloater, ServiceJob, StartupCost, StartupLoad,
    TaskJob,
};
use toolboks_addon_sdk::{AdminOutcome, AdminRequest, HostRequest};
use toolboks_shared::elevation;
use toolboks_shared::ui::{self, AMBER, GAP, GREEN, MUTED, SPACE_M};

const INSPECTOR_WIDTH: f32 = 256.0;

/// Text for the state column of a row.
fn state_text(status: Status) -> &'static str {
    if status.applicable == Applicable::EditionNotSupported {
        return "Not on this edition";
    }
    match (status.state, status.applicable) {
        (State::Off, _) => "Already off",
        (State::Unknown, _) => "Unknown",
        (State::Active, Applicable::NeedsAdmin) => "Needs admin",
        (State::Active, _) => "Now on",
    }
}

/// Only features that are still on, and that this edition supports, can be ticked.
fn tickable(status: Status) -> bool {
    status.state == State::Active && status.applicable != Applicable::EditionNotSupported
}

/// The privacy and ads items split by risk: (safe, careful), catalog order kept.
fn group_by_risk(items: &[Item]) -> (Vec<&Item>, Vec<&Item>) {
    let mut safe = Vec::new();
    let mut careful = Vec::new();
    for item in items.iter().filter(|i| i.group == Group::PrivacyAds) {
        match item.risk {
            Risk::Safe => safe.push(item),
            Risk::Careful => careful.push(item),
        }
    }
    (safe, careful)
}

fn edition_label(edition: Edition) -> &'static str {
    match edition {
        Edition::Unknown => "Windows",
        Edition::Home => "Windows Home",
        Edition::Pro => "Windows Pro",
        Edition::Enterprise => "Windows Enterprise",
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn reversibility(item: &Item) -> &'static str {
    if item.needs_admin() {
        "Can be switched back by undoing, or by turning the setting on again in Windows. Needs administrator rights."
    } else {
        "Can be switched back by undoing, or by turning the setting on again in Windows."
    }
}

fn risk_color(risk: Risk) -> Color32 {
    match risk {
        Risk::Safe => GREEN,
        Risk::Careful => AMBER,
    }
}

/// The Safe / Careful chip of a row; muted when the row cannot be changed.
fn risk_chip(risk: Risk, active: bool) -> Chip {
    Chip::new(
        risk_text(risk),
        if active { risk_color(risk) } else { MUTED },
    )
}

/// Colour of the state chip of a privacy row; the word says the same thing.
fn state_color(status: Status) -> Color32 {
    if status.applicable == Applicable::EditionNotSupported {
        return MUTED;
    }
    match (status.state, status.applicable) {
        (State::Off, _) => GREEN,
        (State::Unknown, _) => MUTED,
        (State::Active, Applicable::NeedsAdmin) => AMBER,
        (State::Active, _) => MUTED,
    }
}

fn risk_text(risk: Risk) -> &'static str {
    match risk {
        Risk::Safe => "Safe",
        Risk::Careful => "Careful",
    }
}

const RESTORE_NOTE: &str = "A restore point saves Windows settings so you can roll back in \
                            Windows. It does not bring back removed apps.";

/// A restore point is made only when the switch is on, we are admin and (Tasks page) none was
/// made yet this session.
fn should_create(switch: bool, elevated: bool, already_created: bool) -> bool {
    switch && elevated && !already_created
}

impl Debloater {
    /// The "Create a restore point first" switch with its notes.
    /// Machine-wide changes (tasks, services) go through the administrator helper, which asks for
    /// rights itself; the add-on does not need to be elevated to offer them.
    fn machine_ok(&self) -> bool {
        true
    }

    /// `helper` is true where the administrator helper can make the restore point for the page.
    fn restore_switch(&mut self, ui: &mut egui::Ui, busy: bool, helper: bool) {
        let usable = self.env.elevated || helper;
        ui.horizontal(|ui| {
            ui.add_enabled_ui(usable && !busy, |ui| {
                ui::toggle_switch(ui, &mut self.restore.on);
            });
            ui.label("Create a restore point first");
        });
        if !usable {
            style::note(ui, "Needs admin");
        } else if helper && self.restore.on {
            style::note(ui, "A separate administrator window asks you to confirm.");
        }
        style::note(ui, RESTORE_NOTE);
    }

    /// Sends one administrator request. The separate helper checks it, shows its own Confirm /
    /// Cancel window and only then changes anything; nothing here can approve it.
    fn send_admin(&mut self, kind: AdminKind, json: String) {
        self.next_request = self.next_request.wrapping_add(1);
        self.admin_pending = Some((self.next_request, kind));
        self.request = Some(HostRequest::AdminOperation(AdminRequest {
            id: self.next_request,
            operation: admin::OPERATION.into(),
            json,
        }));
    }

    /// The helper answered. This only shows the outcome; it never authorizes anything.
    pub(super) fn admin_finished(&mut self, id: u64, outcome: AdminOutcome) {
        let Some((pending, kind)) = self.admin_pending else {
            return;
        };
        if pending != id {
            return;
        }
        self.admin_pending = None;
        if matches!(
            kind,
            AdminKind::ServiceSet
                | AdminKind::ServiceUndo
                | AdminKind::TaskSet
                | AdminKind::TaskUndo
        ) {
            self.finish_job(kind, outcome);
            return;
        }
        if matches!(kind, AdminKind::StartupSet | AdminKind::StartupUndo) {
            self.finish_startup(kind, outcome);
            return;
        }
        match outcome {
            AdminOutcome::Cancelled => {
                self.message = Some("Cancelled. Nothing was changed.".to_string());
            }
            AdminOutcome::Failed { reason } => self.message = Some(reason),
            AdminOutcome::Completed { json } => {
                let value: serde_json::Value =
                    serde_json::from_str(&json).unwrap_or(serde_json::Value::Null);
                match kind {
                    AdminKind::RegistryApply => self.registry_applied(&value),
                    AdminKind::RegistryUndo => self.registry_undone(&value),
                    // Handled above, before this match.
                    _ => {}
                }
            }
        }
        self.refresh();
    }

    /// Turns the helper's answer for a service or task request into the same job the page used to
    /// get from its worker thread, so the existing message and undo handling runs unchanged.
    fn finish_job(&mut self, kind: AdminKind, outcome: AdminOutcome) {
        let completed = match &outcome {
            AdminOutcome::Completed { json } => {
                serde_json::from_str::<serde_json::Value>(json).ok()
            }
            _ => None,
        };
        let stopped = match outcome {
            AdminOutcome::Cancelled => "Cancelled. Nothing was changed.".to_string(),
            AdminOutcome::Failed { reason } => reason,
            AdminOutcome::Completed { .. } => {
                "The administrator helper returned an unreadable answer.".to_string()
            }
        };
        match kind {
            AdminKind::ServiceSet => {
                let title = std::mem::take(&mut self.pending_title);
                let (result, point) = match &completed {
                    Some(value) => admin::change_from_json(value, admin::service_step_from),
                    None => (Err(stopped.into()), None),
                };
                if let Some(tx) = self.service_tx.take() {
                    let _ = tx.send(ServiceJob::Set(title, result, point));
                }
            }
            AdminKind::TaskSet => {
                let title = std::mem::take(&mut self.pending_title);
                let (result, point) = match &completed {
                    Some(value) => admin::change_from_json(value, admin::task_step_from),
                    None => (Err(stopped.into()), None),
                };
                if let Some(tx) = self.task_tx.take() {
                    let _ = tx.send(TaskJob::Set(title, result, point));
                }
            }
            AdminKind::ServiceUndo => {
                let batch = std::mem::take(&mut self.service_batch);
                let mut remaining = std::mem::take(&mut self.service_rest);
                let tally = match &completed {
                    Some(value) => admin::undo_batch_from_json(value, saved_undo::service_from),
                    None => unchanged(batch),
                };
                let failed = tally.kept.len();
                remaining.extend(tally.kept.into_iter().rev());
                if let Some(tx) = self.service_tx.take() {
                    let _ = tx.send(ServiceJob::Undo {
                        remaining,
                        done: tally.done,
                        failed,
                        refused: tally.refused,
                    });
                }
            }
            AdminKind::TaskUndo => {
                let batch = std::mem::take(&mut self.task_batch);
                let mut remaining = std::mem::take(&mut self.task_rest);
                let tally = match &completed {
                    Some(value) => admin::undo_batch_from_json(value, saved_undo::task_from),
                    None => unchanged(batch),
                };
                let failed = tally.kept.len();
                remaining.extend(tally.kept.into_iter().rev());
                if let Some(tx) = self.task_tx.take() {
                    let _ = tx.send(TaskJob::Undo {
                        remaining,
                        done: tally.done,
                        failed,
                        refused: tally.refused,
                    });
                }
            }
            _ => {}
        }
        self.refresh();
    }

    /// The helper's answer for a startup request: the same messages and undo steps the page used to
    /// produce when it changed the entry itself.
    fn finish_startup(&mut self, kind: AdminKind, outcome: AdminOutcome) {
        let value = match &outcome {
            AdminOutcome::Completed { json } => {
                serde_json::from_str::<serde_json::Value>(json).ok()
            }
            _ => None,
        };
        let stopped = match outcome {
            AdminOutcome::Cancelled => "Cancelled. Nothing was changed.".to_string(),
            AdminOutcome::Failed { reason } => reason,
            AdminOutcome::Completed { .. } => {
                "The administrator helper returned an unreadable answer.".to_string()
            }
        };
        match kind {
            AdminKind::StartupSet => {
                let (name, enable) = self.pending_startup.take().unwrap_or_default();
                let result = match &value {
                    Some(value) => admin::change_from_json(value, admin::startup_step_from).0,
                    None => Err(stopped.into()),
                };
                self.startup.message = Some(match result {
                    Ok(step) => {
                        self.startup
                            .steps
                            .push(saved_undo::startup_done(step, enable));
                        format!("{name} is now {}.", if enable { "on" } else { "off" })
                    }
                    Err(e) => {
                        let message = format!("Could not change {name}: {e}");
                        self.startup
                            .steps
                            .extend(e.recovery.map(saved_undo::recovery));
                        message
                    }
                });
            }
            AdminKind::StartupUndo => {
                let batch = std::mem::take(&mut self.startup_batch);
                let mut remaining = std::mem::take(&mut self.startup_rest);
                let tally = match &value {
                    Some(value) => admin::undo_batch_from_json(value, saved_undo::startup_from),
                    None => unchanged(batch),
                };
                let failed = tally.kept.len();
                remaining.extend(tally.kept.into_iter().rev());
                // Changes made on this page while the helper was open come after the rest.
                remaining.append(&mut self.startup.steps);
                self.startup.steps = remaining;
                // Steps whose entry was already gone were taken out before the request.
                let mut refused = std::mem::take(&mut self.startup_dropped);
                self.startup.message = Some(if value.is_none() {
                    with_refused(stopped, &refused)
                } else {
                    refused.extend(tally.refused);
                    startup_undo_summary(tally.done, failed, &refused)
                });
            }
            _ => {}
        }
        self.startup.started = false;
        self.save_undo();
    }

    fn registry_applied(&mut self, value: &serde_json::Value) {
        if let Some(text) = value["restore"].as_str() {
            self.restore.status = Some(text.to_owned());
        }
        if value["blocked"].as_bool() == Some(true) {
            self.message = Some("Nothing was changed.".to_string());
            return;
        }
        let run = admin::run_from_json(value);
        self.message = Some(summarize(&run));
        self.run_notes = run_notes(&run);
        if !run.undo.is_empty() {
            self.undo_runs.push(saved_undo::track_run(&run));
            self.save_undo();
        }
    }

    fn registry_undone(&mut self, value: &serde_json::Value) {
        let total = value["total"].as_u64().unwrap_or(0) as usize;
        let remaining = admin::steps_from_json(&value["remaining"]);
        let refused = admin::refused_from_json(value);
        self.message = Some(undo_summary(total, remaining.len(), &refused));
        self.undo_runs.pop();
        if !remaining.is_empty() {
            self.undo_runs.push(remaining);
        }
        self.run_notes.clear();
        self.save_undo();
    }

    /// Starts an apply. An item that writes the machine-wide registry, or a restore point, needs
    /// administrator rights and goes through the helper; everything else runs here as before.
    fn start_apply(&mut self, _ctx: &egui::Context, ids: &[&'static str]) {
        self.confirming = false;
        self.restore.status = None;
        if self.admin_pending.is_some() {
            return;
        }
        if admin::needs_helper(ids, self.restore.on) {
            self.send_admin(
                AdminKind::RegistryApply,
                admin::registry_apply_request(ids, self.restore.on),
            );
            return;
        }
        self.apply_selected(ids);
    }

    fn restore_poll(&mut self) {
        let Some(rx) = &self.restore.creating else {
            return;
        };
        let (status, ids) = match rx.try_recv() {
            Ok(done) => done,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => (
                RestoreStatus::Failed("the restore point stopped unexpectedly".into()),
                Vec::new(),
            ),
        };
        self.restore.creating = None;
        self.restore.status = Some(restore::status_text(&status));
        if restore::is_blocking(&status) {
            self.message = Some("Nothing was changed.".to_string());
        } else if !ids.is_empty() {
            self.apply_selected(&ids);
        }
    }

    fn refresh(&mut self) {
        self.statuses = plan::statuses(catalog::items(), &backend::WinRegistry, self.env);
        self.loaded = true;
        // Drop ticks on items that can no longer be changed.
        let statuses = &self.statuses;
        self.selected.retain(|id| {
            statuses
                .iter()
                .find(|s| s.id == *id)
                .is_some_and(|s| tickable(*s))
        });
    }

    fn status_of(&self, id: &str) -> Option<Status> {
        self.statuses.iter().find(|s| s.id == id).copied()
    }

    pub(super) fn page_ui(&mut self, ui: &mut egui::Ui, page: usize) {
        if page == 1 && !self.can_change_user_profile {
            style::note(ui, elevation::USER_PROFILE_BLOCKED);
            return;
        }
        if !self.loaded {
            self.refresh();
        }
        if !self.saved.loaded {
            self.load_saved_undo();
        }

        if page == 0 {
            self.restore_poll();
        }

        if page == 1 {
            self.apps_poll();
            if !self.apps.started {
                self.apps_refresh(ui.ctx());
            }
        }

        if page == 2 {
            self.startup_poll();
            if !self.startup.started {
                // Toggling an entry reloads the list; the boot log is only read again on Refresh.
                let read_cost = self.startup.cost.is_none();
                self.startup_refresh(ui.ctx(), read_cost);
            }
        }

        // A task or service change the helper answered while another page was open is picked up
        // (and saved) right away, not when that page is opened again.
        self.tasks_poll();
        self.services_poll();

        if page == 3 && !self.tasks.started {
            self.tasks_refresh(ui.ctx());
        }

        if page == 4 && !self.services.started {
            self.services_refresh(ui.ctx());
        }

        let panel = egui::Panel::right("debloat_inspector")
            .resizable(false)
            .exact_size(INSPECTOR_WIDTH)
            .frame(style::review_frame())
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| match page {
                        0 => self.inspector(ui),
                        1 => self.apps_inspector(ui),
                        2 => self.startup_inspector(ui),
                        3 => self.tasks_inspector(ui),
                        _ => self.services_inspector(ui),
                    });
            });
        toolboks_addon_sdk::tutorial::mark(ui, "inspector", panel.response.rect);

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 4.0;
                match page {
                    0 => toolboks_addon_sdk::tutorial::region(ui, "table", |ui| {
                        self.privacy_page(ui)
                    }),
                    1 => toolboks_addon_sdk::tutorial::region(ui, "table", |ui| self.apps_page(ui)),
                    2 => toolboks_addon_sdk::tutorial::region(ui, "table", |ui| {
                        self.startup_page(ui)
                    }),
                    3 => {
                        toolboks_addon_sdk::tutorial::region(ui, "table", |ui| self.tasks_page(ui))
                    }
                    _ => toolboks_addon_sdk::tutorial::region(ui, "table", |ui| {
                        self.services_page(ui)
                    }),
                }
            });
    }

    fn privacy_page(&mut self, ui: &mut egui::Ui) {
        let admin = if self.env.elevated {
            "Running as administrator"
        } else {
            "Not running as administrator"
        };
        let status = format!("{} \u{b7} {admin}", edition_label(self.env.edition));
        if style::page_head(ui, &status, true) {
            self.refresh();
        }
        if let Some(message) = &self.message {
            style::note(ui, message);
        }
        if self.restore.creating.is_some() {
            style::note(ui, "Creating restore point...");
        } else if let Some(line) = &self.restore.status {
            style::note(ui, line);
        }
        ui.add_space(GAP);

        let (mut safe, mut careful) = group_by_risk(catalog::items());
        if !self.can_change_user_profile {
            style::note(ui, elevation::USER_PROFILE_BLOCKED);
            let machine_only =
                |item: &&Item| item.settings.iter().all(|s| s.hive == Hive::LocalMachine);
            safe.retain(machine_only);
            careful.retain(machine_only);
        }
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if safe.is_empty() && careful.is_empty() {
                    ui.label(RichText::new("No items to show.").color(MUTED));
                    return;
                }
                for (title, note, list) in [
                    (
                        "Safe",
                        "Little or no visible effect when switched off.",
                        safe,
                    ),
                    (
                        "Careful",
                        "Can change how some Windows features behave. Read what could break first.",
                        careful,
                    ),
                ] {
                    if list.is_empty() {
                        continue;
                    }
                    style::section(ui, title, Some(note));
                    style::group_card(ui, |ui| {
                        for item in list {
                            self.item_row(ui, item);
                        }
                    });
                    ui.add_space(SPACE_M);
                }
                self.about_item(ui);
            });
    }

    fn item_row(&mut self, ui: &mut egui::Ui, item: &Item) {
        let status = self.status_of(item.id).unwrap_or(Status {
            id: item.id,
            state: State::Unknown,
            applicable: Applicable::Yes,
        });
        let can_tick = tickable(status);
        let focused = self.focus == Some(item.id);

        let chips = [
            style::evidence_chip(&item.evidence()),
            Chip::new(state_text(status), state_color(status)),
            risk_chip(item.risk, can_tick),
        ];
        let (response, body) = style::row(ui, focused, &chips);
        if response.clicked() {
            self.focus = Some(item.id);
        }

        let mut ticked = self.selected.contains(item.id);
        let dim = if can_tick { ui::TEXT } else { MUTED };
        let mut checkbox_clicked = false;
        style::row_body(ui, body, |ui| {
            ui.add_enabled_ui(can_tick, |ui| {
                checkbox_clicked = ui.add(egui::Checkbox::without_text(&mut ticked)).changed();
            });
            style::row_title(ui, item.title, dim);
        });
        if checkbox_clicked {
            self.focus = Some(item.id);
            if ticked {
                self.selected.insert(item.id);
            } else {
                self.selected.remove(item.id);
            }
        }
    }

    fn about_item(&self, ui: &mut egui::Ui) {
        style::panel_heading(ui, "About this item");
        let item = self
            .focus
            .and_then(|id| catalog::items().iter().find(|i| i.id == id));
        let Some(item) = item else {
            style::note(ui, "Click an item to see what it changes.");
            return;
        };
        ui.label(RichText::new(item.title).strong());
        ui.label(item.what_changes);
        style::field_label(ui, "What could break");
        ui.label(item.could_break);
        style::evidence_block(ui, &item.evidence());
        style::note(ui, reversibility(item));
    }

    fn inspector(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.spacing_mut().item_spacing.y = 4.0;
        style::panel_heading(ui, "Review");

        let ids: Vec<&'static str> = self.selected.iter().copied().collect();
        let review = plan::review(catalog::items(), &self.statuses, &ids);
        if review.changes == 0 {
            ui.label("No changes selected");
        } else {
            ui.label(RichText::new(plural(review.changes, "change", "changes")).strong());
            style::note(
                ui,
                &format!("{} safe, {} careful", review.safe, review.careful),
            );
        }
        if review.needs_admin > 0 {
            ui.add_space(GAP);
            ui.label(format!("{} need administrator rights", review.needs_admin));
            style::note(
                ui,
                "A separate administrator window shows exactly what will change. Confirm or cancel there.",
            );
        }

        style::divider(ui);
        let applicable = review.changes;
        if applicable == 0 {
            self.confirming = false;
        }
        let apply_label = format!("Apply {}", plural(applicable, "change", "changes"));
        let creating = self.restore.creating.is_some() || self.admin_pending.is_some();
        self.restore_switch(ui, creating, true);
        ui.add_space(GAP);
        if self.admin_pending.is_some() {
            style::note(ui, "Waiting for the administrator window...");
        } else if creating {
            style::note(ui, "Creating restore point...");
        } else if self.confirming {
            ui.label(format!(
                "Apply {}? You can undo them later, also after Toolboks closes.",
                plural(applicable, "change", "changes")
            ));
            ui.horizontal(|ui| {
                if ui.add(style::primary_button("Apply")).clicked() {
                    self.start_apply(&ctx, &ids);
                }
                if ui.button("Cancel").clicked() {
                    self.confirming = false;
                }
            });
        } else if ui
            .add_enabled(applicable > 0, style::primary_button(&apply_label))
            .clicked()
        {
            self.confirming = true;
        }
        for note in &self.run_notes {
            ui.add(egui::Label::new(RichText::new(note).small().color(AMBER)).wrap());
        }

        style::divider(ui);
        style::panel_heading(ui, "Undo");
        let can_undo = !self.undo_runs.is_empty();
        if ui
            .add_enabled(can_undo, egui::Button::new("Undo last run"))
            .clicked()
        {
            self.undo_last_run();
        }
        if can_undo && ui.link(RichText::new("Copy undo steps").small()).clicked() {
            ctx.copy_text(undo_text(&self.undo_runs));
        }
        style::note(ui, "Removed apps come back from the Microsoft Store.");
        self.saved_undo_panel(ui);
    }

    fn apply_selected(&mut self, ids: &[&str]) {
        self.confirming = false;
        let run = plan::apply(catalog::items(), ids, &mut backend::WinRegistry, self.env);
        self.message = Some(summarize(&run));
        self.run_notes = run_notes(&run);
        if !run.undo.is_empty() {
            self.undo_runs.push(saved_undo::track_run(&run));
            self.save_undo();
        }
        // Refreshing also clears the ticks of items that are now off.
        self.refresh();
    }

    fn undo_last_run(&mut self) {
        if self.admin_pending.is_some() {
            return;
        }
        if let Some(steps) = self.undo_runs.last() {
            if steps.iter().any(|op| op.step.hive == Hive::LocalMachine) {
                let request = admin::registry_undo_request(steps);
                self.send_admin(AdminKind::RegistryUndo, request);
                return;
            }
        }
        let Some(steps) = self.undo_runs.pop() else {
            return;
        };
        let total = steps.len();
        let tally = saved_undo::undo_steps(
            steps.into_iter().rev().collect(),
            |s| (s.step.hive, s.step.path, s.step.name),
            |s| saved_undo::undo_registry(s, &mut backend::WinRegistry),
        );
        let mut kept = tally.kept;
        kept.reverse();
        self.message = Some(undo_summary(total, kept.len(), &tally.refused));
        if !kept.is_empty() {
            self.undo_runs.push(kept);
        }
        self.run_notes.clear();
        self.save_undo();
        self.refresh();
    }
}

/// Where Debloater keeps its undo list, in plain words.
const SAVED_NOTE: &str = "Debloater saves its undo list on this PC, so you can undo after \
                          closing Toolboks or restarting Windows. It keeps only what undo needs: \
                          the earlier values of the settings, services, tasks and startup items \
                          it changed, and the names of apps it removed. It is kept in:";
const REFUSE_NOTE: &str = "An item that something else changed since is left as it is, and \
                           Debloater says which.";

fn unreadable_note(count: usize) -> String {
    format!(
        "{} could not be read by this version of Debloater (it no longer offers that item). \
         They are kept as they are and not used.",
        plural(count, "saved undo step", "saved undo steps")
    )
}

fn damaged_note(reason: &str, kept: bool) -> String {
    let copy = if kept {
        " A copy was kept as undo-damaged-1.jsonl in the folder below."
    } else {
        " It could not be moved aside, so it is left as it is and new changes are not saved \
         until you use \"Delete saved undo\"; until then they can be undone until Toolboks closes."
    };
    format!(
        "The saved undo list is damaged ({reason}), so it was not used and nothing was guessed \
         from it. Changes made earlier can still be switched back on these pages.{copy}"
    )
}

fn newer_note(version: u64) -> String {
    format!(
        "The saved undo list was made by a newer version of Debloater (format {version}), so this \
         version does not use or change it. Update Debloater to undo those changes. Changes you \
         make now can be undone until Toolboks closes."
    )
}

fn unavailable_note(error: &str) -> String {
    format!(
        "The saved undo list could not be read ({error}). It was left as it is. Changes you make \
         now can be undone until Toolboks closes."
    )
}

/// An undo batch the helper did not run: every step stays.
fn unchanged<T>(batch: Vec<T>) -> saved_undo::Tally<T> {
    saved_undo::Tally {
        done: 0,
        kept: batch,
        refused: Vec::new(),
    }
}

/// Older steps, then a batch that is out at the helper (newest first), then newer steps.
fn in_order<T: Clone>(rest: &[T], batch: &[T], steps: &[T]) -> Vec<T> {
    rest.iter()
        .chain(batch.iter().rev())
        .chain(steps)
        .cloned()
        .collect()
}

impl Debloater {
    /// Reads the saved undo list once, on the first draw (never in the administrator helper).
    fn load_saved_undo(&mut self) {
        let loaded = self.saved.store.load();
        self.use_loaded(loaded);
    }

    /// Takes in what was found on disk.
    fn use_loaded(&mut self, loaded: Loaded) {
        self.saved.loaded = true;
        match loaded {
            Loaded::Nothing => {}
            Loaded::List(list) => {
                if !list.unreadable.is_empty() {
                    self.saved.note = Some(unreadable_note(list.unreadable.len()));
                }
                let mut runs = list.registry_runs;
                runs.append(&mut self.undo_runs);
                self.undo_runs = runs;
                let mut steps = list.startup;
                steps.append(&mut self.startup.steps);
                self.startup.steps = steps;
                let mut steps = list.tasks;
                steps.append(&mut self.tasks.steps);
                self.tasks.steps = steps;
                let mut steps = list.services;
                steps.append(&mut self.services.steps);
                self.services.steps = steps;
                for id in list.removed_apps {
                    if !self.saved.removed_apps.contains(&id) {
                        self.saved.removed_apps.push(id);
                    }
                }
                self.saved.unreadable = list.unreadable;
            }
            Loaded::Damaged { reason, kept } => {
                // A damaged file that is still in place must never be replaced by a save.
                self.saved.paused = !kept;
                self.saved.note = Some(damaged_note(&reason, kept));
            }
            Loaded::Newer(version) => {
                self.saved.paused = true;
                self.saved.note = Some(newer_note(version));
            }
            Loaded::Unavailable(error) => {
                self.saved.paused = true;
                self.saved.note = Some(unavailable_note(&error));
            }
        }
    }

    /// Everything that can be undone now, including a batch that is out at the helper.
    fn undo_list(&self) -> UndoList {
        UndoList {
            registry_runs: self.undo_runs.clone(),
            startup: in_order(&self.startup_rest, &self.startup_batch, &self.startup.steps),
            tasks: in_order(&self.task_rest, &self.task_batch, &self.tasks.steps),
            services: in_order(
                &self.service_rest,
                &self.service_batch,
                &self.services.steps,
            ),
            removed_apps: self.saved.removed_apps.clone(),
            unreadable: self.saved.unreadable.clone(),
        }
    }

    /// Writes the undo list. Called after every change and every undo.
    fn save_undo(&mut self) {
        if !self.saved.loaded {
            // Never replace a saved list that was not read yet.
            self.load_saved_undo();
        }
        if self.saved.paused {
            return;
        }
        self.saved.error = self.saved.store.save(&self.undo_list()).err().map(|error| {
            format!("Could not save the undo list ({error}). Undo works until Toolboks closes.")
        });
    }

    /// "Delete saved undo": forgets every undo step, saved and in memory. The changes stay in
    /// Windows.
    fn delete_saved_undo(&mut self) {
        self.saved.confirming = false;
        if let Err(error) = self.saved.store.delete() {
            self.saved.error = Some(format!("Could not delete the saved undo list ({error})."));
            return;
        }
        self.undo_runs.clear();
        self.startup.steps.clear();
        self.tasks.steps.clear();
        self.services.steps.clear();
        self.saved.removed_apps.clear();
        self.saved.unreadable.clear();
        self.saved.paused = false;
        self.saved.note = None;
        self.saved.error = None;
        self.run_notes.clear();
        self.message = Some(
            "The saved undo list was deleted. The changes stay in Windows; you can still switch \
             items back on these pages."
                .to_string(),
        );
    }

    /// What is kept, where, and the "Delete saved undo" button. Shown under Undo on every page.
    fn saved_undo_panel(&mut self, ui: &mut egui::Ui) {
        style::note(ui, SAVED_NOTE);
        style::note(ui, &self.saved.store.dir().display().to_string());
        style::note(ui, REFUSE_NOTE);
        for line in [&self.saved.note, &self.saved.error].into_iter().flatten() {
            ui.add(egui::Label::new(RichText::new(line).small().color(AMBER)).wrap());
        }
        let busy = self.admin_pending.is_some()
            || self.tasks.working.is_some()
            || self.services.working.is_some()
            || self.apps.removing.is_some();
        let anything =
            !self.undo_list().is_empty() || self.saved.paused || self.saved.note.is_some();
        if busy || !anything {
            self.saved.confirming = false;
        }
        if self.saved.confirming {
            ui.label(
                "Delete the saved undo list? The changes stay in Windows, but Debloater can no \
                 longer undo them.",
            );
            ui.horizontal(|ui| {
                if ui.add(style::primary_button("Delete")).clicked() {
                    self.delete_saved_undo();
                }
                if ui.button("Cancel").clicked() {
                    self.saved.confirming = false;
                }
            });
        } else if ui
            .add_enabled(anything && !busy, egui::Button::new("Delete saved undo"))
            .clicked()
        {
            self.saved.confirming = true;
        }
    }
}

/// One line for the result of an apply run.
fn summarize(run: &plan::Run) -> String {
    let count =
        |f: fn(&plan::Outcome) -> bool| run.applied.iter().filter(|a| f(&a.outcome)).count();
    let done = count(|o| matches!(o, plan::Outcome::Done));
    let ineffective = count(|o| matches!(o, plan::Outcome::NotEffective));
    let failed = count(|o| matches!(o, plan::Outcome::Failed(_)));
    format!(
        "Done: {done}, restored after a mismatch: {ineffective}, \
         failed: {failed}"
    )
}

/// The reasons refused undo steps were left alone, after a summary line.
fn with_refused(line: String, refused: &[String]) -> String {
    if refused.is_empty() {
        line
    } else {
        format!("{line} Left alone: {}", refused.join(" "))
    }
}

fn undo_summary(total: usize, failed: usize, refused: &[String]) -> String {
    let done = total.saturating_sub(failed + refused.len());
    let line = if failed == 0 && refused.is_empty() {
        format!("Restored {}.", plural(total, "setting", "settings"))
    } else if failed == 0 {
        format!(
            "Restored {done} of {}.",
            plural(total, "setting", "settings")
        )
    } else {
        format!(
            "Restored {done} of {}, {failed} failed. Undo is still available.",
            plural(total, "setting", "settings")
        )
    };
    with_refused(line, refused)
}

fn title_of(id: &str) -> String {
    catalog::items()
        .iter()
        .find(|i| i.id == id)
        .map_or_else(|| id.to_string(), |i| i.title.to_string())
}

/// Plain-words lines for every item that did not end up fully done.
fn run_notes(run: &plan::Run) -> Vec<String> {
    run.applied
        .iter()
        .filter_map(|a| match &a.outcome {
            plan::Outcome::Done => None,
            plan::Outcome::NotEffective => Some(format!(
                "{}: Windows did not keep the change. The previous setting was restored.",
                title_of(a.id)
            )),
            plan::Outcome::Failed(e) => {
                Some(format!("{}: could not be changed ({e}).", title_of(a.id)))
            }
        })
        .collect()
}

fn hive_text(hive: Hive) -> &'static str {
    match hive {
        Hive::CurrentUser => "HKEY_CURRENT_USER",
        Hive::LocalMachine => "HKEY_LOCAL_MACHINE",
    }
}

/// A plain text list of what to put back by hand: item, setting path, previous value.
fn undo_text(runs: &[Vec<RegStep>]) -> String {
    let mut out = String::from("toolboks undo steps\n");
    for op in runs
        .iter()
        .rev()
        .flat_map(|steps| steps.iter().rev())
        .map(|step| &step.step)
    {
        let previous = match &op.previous {
            Some(RegValue::Dword(v)) => format!("number {v}"),
            Some(RegValue::Sz(s)) => format!("text \"{s}\""),
            None => "not set (delete the value)".to_string(),
        };
        out.push_str(&format!(
            "\n{}\n  {}\\{}\n  value: {}\n  previous: {previous}\n",
            title_of(op.id),
            hive_text(op.hive),
            op.path,
            op.name
        ));
    }
    out
}

const PROTECTED_NOTE: &str = "Store, Edge, Windows security, Photos, Calculator, Xbox services, \
                              and system components are never offered.";
const REINSTALL_NOTE: &str = "Removed apps can be reinstalled from the Microsoft Store. Removal applies to your account only.";

fn app_state_text(installed: bool) -> &'static str {
    if installed {
        "Installed"
    } else {
        "Not installed"
    }
}

/// Result line for a removal run, plus the failed apps with their reason. `title` maps an app id
/// to its display title.
fn app_summary(run: &AppRun, title: impl Fn(&str) -> String) -> String {
    let mut out = format!("Removed {}, failed {}", run.removed.len(), run.failed.len());
    for (id, reason) in &run.failed {
        out.push_str(&format!("\n{}: {reason}", title(id)));
    }
    out
}

fn app_title(id: &str) -> String {
    apps::curated()
        .iter()
        .find(|c| c.id == id)
        .map_or_else(|| id.to_string(), |c| c.title.to_string())
}

/// Opens the Microsoft Store start page.
fn open_store() {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = std::process::Command::new("cmd")
        .args(["/c", "start", "", "ms-windows-store://home"])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

impl Debloater {
    /// Starts reading the installed apps on a worker thread (no-op while one is running).
    fn apps_refresh(&mut self, ctx: &egui::Context) {
        self.apps.started = true;
        if self.apps.loading.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(apps::rows(&apps::WinPackages));
            ctx.request_repaint();
        });
        self.apps.loading = Some(rx);
    }

    /// Picks up finished worker results.
    fn apps_poll(&mut self) {
        if let Some(rx) = &self.apps.loading {
            match rx.try_recv() {
                Ok(result) => {
                    self.apps.loading = None;
                    match result {
                        Ok(rows) => {
                            self.apps.rows = rows;
                            self.apps.error = None;
                            let rows = &self.apps.rows;
                            self.apps.selected.retain(|id| {
                                rows.iter()
                                    .any(|r| r.entry.id == *id && r.installed.is_some())
                            });
                            // An app that is installed again no longer needs its reminder.
                            let installed = |id: &str| {
                                rows.iter()
                                    .any(|r| r.entry.id == id && r.installed.is_some())
                            };
                            if saved_undo::forget_reinstalled(
                                &mut self.saved.removed_apps,
                                installed,
                            ) {
                                self.save_undo();
                            }
                        }
                        Err(e) => self.apps.error = Some(e),
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.apps.loading = None;
                    self.apps.error = Some("Reading the apps stopped unexpectedly.".to_string());
                }
            }
        }
        if let Some(rx) = &self.apps.removing {
            match rx.try_recv() {
                Ok(run) => {
                    self.apps.removing = None;
                    self.apps.message = Some(app_summary(&run, app_title));
                    // Only the catalog id is kept: enough to say which app to get back.
                    if saved_undo::record_removed(&mut self.saved.removed_apps, &run) {
                        self.save_undo();
                    }
                    self.apps.last_run = Some(run);
                    self.apps.selected.clear();
                    self.apps.started = false; // reload right away
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.apps.removing = None;
                    self.apps.message = Some("Removal stopped unexpectedly.".to_string());
                    self.apps.started = false;
                }
            }
        }
    }

    fn apps_start_remove(&mut self, ctx: &egui::Context) {
        self.apps.confirming = false;
        if self.apps.removing.is_some() {
            return;
        }
        let rows = self.apps.rows.clone();
        let ids: Vec<&'static str> = self.apps.selected.iter().copied().collect();
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let run = apps::remove_apps(&rows, &ids, &mut apps::WinPackages);
            let _ = tx.send(run);
            ctx.request_repaint();
        });
        self.apps.removing = Some(rx);
    }

    fn apps_page(&mut self, ui: &mut egui::Ui) {
        let busy = self.apps.loading.is_some() || self.apps.removing.is_some();
        if style::page_head(ui, "Apps for your account", !busy) {
            self.apps_refresh(ui.ctx());
        }
        ui.add_space(GAP);

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if let Some(error) = &self.apps.error {
                    ui.label(error.as_str());
                } else if self.apps.loading.is_some() && self.apps.rows.is_empty() {
                    style::note(ui, "Collecting apps...");
                } else if self.apps.rows.is_empty() {
                    style::note(ui, "No apps to show.");
                }
                let rows = self.apps.rows.clone();
                if !rows.is_empty() {
                    style::group_card(ui, |ui| {
                        for row in &rows {
                            self.app_row(ui, row);
                        }
                    });
                }
                ui.add_space(SPACE_M);
                style::section(ui, "Protected by toolboks", Some(PROTECTED_NOTE));
                ui.add_space(SPACE_M);
                self.about_app(ui);
            });
    }

    fn app_row(&mut self, ui: &mut egui::Ui, row: &AppRow) {
        let id = row.entry.id;
        let installed = row.installed.is_some();
        let focused = self.apps.focus == Some(id);

        let chips = [
            style::evidence_chip(&row.entry.evidence()),
            Chip::new(app_state_text(installed), MUTED),
        ];
        let (response, body) = style::row(ui, focused, &chips);
        if response.clicked() {
            self.apps.focus = Some(id);
        }

        let mut ticked = self.apps.selected.contains(id);
        let text_color = if installed { ui::TEXT } else { MUTED };
        let mut checkbox_clicked = false;
        style::row_body(ui, body, |ui| {
            ui.add_enabled_ui(installed, |ui| {
                checkbox_clicked = ui.add(egui::Checkbox::without_text(&mut ticked)).changed();
            });
            style::row_title(ui, row.entry.title, text_color);
        });
        if checkbox_clicked {
            self.apps.focus = Some(id);
            if ticked {
                self.apps.selected.insert(id);
            } else {
                self.apps.selected.remove(id);
            }
        }
    }

    fn about_app(&self, ui: &mut egui::Ui) {
        style::panel_heading(ui, "About this app");
        let entry = self
            .apps
            .focus
            .and_then(|id| self.apps.rows.iter().find(|r| r.entry.id == id))
            .map(|r| r.entry);
        let Some(entry) = entry else {
            style::note(ui, "Click an app to see what removing it changes.");
            return;
        };
        ui.label(RichText::new(entry.title).strong());
        ui.label(entry.what_changes);
        style::field_label(ui, "What could break");
        ui.label(entry.could_break);
        style::evidence_block(ui, &entry.evidence());
    }

    fn apps_inspector(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.spacing_mut().item_spacing.y = 4.0;
        style::panel_heading(ui, "Review");

        let count = self.apps.selected.len();
        ui.label(RichText::new(plural(count, "app selected", "apps selected")).strong());

        ui.add_space(GAP);
        let removing = self.apps.removing.is_some();
        if count == 0 || removing {
            self.apps.confirming = false;
        }
        if self.apps.confirming {
            ui.label(format!(
                "Remove {} for your account? Removed apps can be reinstalled from the \
                 Microsoft Store.",
                plural(count, "app", "apps")
            ));
            ui.horizontal(|ui| {
                if ui.add(style::primary_button("Remove")).clicked() {
                    self.apps_start_remove(&ctx);
                }
                if ui.button("Cancel").clicked() {
                    self.apps.confirming = false;
                }
            });
        } else if ui
            .add_enabled(
                count > 0 && !removing,
                style::primary_button(&format!("Remove {}", plural(count, "app", "apps"))),
            )
            .clicked()
        {
            self.apps.confirming = true;
        }
        if removing {
            style::note(ui, "Removing...");
        }
        if let Some(message) = &self.apps.message {
            style::note(ui, message);
        }

        style::divider(ui);
        if !self.saved.removed_apps.is_empty() {
            style::panel_heading(ui, "Removed by Debloater");
            for id in &self.saved.removed_apps {
                ui.label(app_title(id));
            }
            style::note(
                ui,
                "To get one back, open the Microsoft Store, search for its name and install it.",
            );
        }
        if ui
            .link(RichText::new("Open Microsoft Store").small())
            .clicked()
        {
            open_store();
        }
        style::note(ui, REINSTALL_NOTE);
        style::divider(ui);
        style::panel_heading(ui, "Undo");
        self.saved_undo_panel(ui);
    }
}

const FILETIME_UNIX_EPOCH: u64 = 116_444_736_000_000_000;
const NEVER_TIP: &str = "Security and system items cannot be changed here";
const STARTUP_NOTE: &str = "Changes are the same switch Task Manager uses. You can always switch \
                            an app back on here or in Task Manager.";

/// Windows FILETIME (100 ns ticks since 1601) for a point in time.
fn filetime_at(time: std::time::SystemTime) -> u64 {
    let since_unix = time
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    FILETIME_UNIX_EPOCH + (since_unix.as_nanos() / 100) as u64
}

fn filetime_now() -> u64 {
    filetime_at(std::time::SystemTime::now())
}

/// Startup entries split by class: (safe to switch off, advanced, protected), order kept.
fn group_startup(entries: &[Entry]) -> [Vec<&Entry>; 3] {
    let mut groups: [Vec<&Entry>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    for e in entries {
        let slot = match e.class {
            Class::Suggest => 0,
            Class::Careful => 1,
            Class::Never => 2,
        };
        groups[slot].push(e);
    }
    groups
}

fn class_text(class: Class) -> &'static str {
    match class {
        Class::Suggest => "Safe",
        Class::Careful => "Careful",
        Class::Never => "Protected",
    }
}

fn class_color(class: Class) -> Color32 {
    match class {
        Class::Suggest => GREEN,
        Class::Careful => AMBER,
        Class::Never => MUTED,
    }
}

fn class_sentence(class: Class) -> &'static str {
    match class {
        Class::Suggest => "Fine to switch off. The app still works when you open it yourself.",
        Class::Careful => {
            "May affect your hardware or apps. Switch it off only if you know you do not need it."
        }
        Class::Never => "Security or system item. It cannot be changed here.",
    }
}

/// The row switch cannot be used: protected, or the source needs admin and we are not elevated.
fn needs_admin_now(entry: &Entry, elevated: bool) -> bool {
    entry.class != Class::Never && entry.source.needs_admin() && !elevated
}

fn publisher_text(entry: &Entry) -> &str {
    entry.publisher.as_deref().unwrap_or("Unknown publisher")
}

fn confirm_off_text(entry: &Entry) -> String {
    format!(
        "Switch off {}? It may stop {} features from working. You can switch it back on.",
        entry.name,
        publisher_text(entry)
    )
}

/// Undoes steps newest first through `undo_one`: the last one only, or all. Steps that fail stay
/// in the list (original order); refused steps are dropped. Returns (undone, failed, why each
/// refused step was left alone).
fn run_undo<T, K: PartialEq>(
    steps: &mut Vec<T>,
    all: bool,
    key_of: impl Fn(&T) -> K,
    undo_one: impl FnMut(&T) -> StepResult,
) -> (usize, usize, Vec<String>) {
    let take = if all { steps.len() } else { steps.len().min(1) };
    let batch: Vec<T> = steps.split_off(steps.len() - take);
    let tally = saved_undo::undo_steps(batch.into_iter().rev().collect(), key_of, undo_one);
    let mut kept = tally.kept;
    kept.reverse();
    let failed = kept.len();
    steps.extend(kept);
    (tally.done, failed, tally.refused)
}

/// How far back the boot log is read for the Startup page.
const COST_LOOKBACK_MS: i64 = 30 * 86_400_000;

/// The short chip on a row: what Windows measured, from the worst case it logged.
fn startup_cost_chip(s: &boot_perf::SlowSummary) -> String {
    match (s.worst_delay_ms, s.worst_total_ms) {
        (Some(delay), _) => format!("Slows start by {}", boot_perf::duration_text(delay)),
        (None, Some(total)) => format!("Took {} to start", boot_perf::duration_text(total)),
        (None, None) => "Named slow by Windows".to_string(),
    }
}

/// The inspector sentence: the measurement, or why there is none. An unreadable log is never
/// described as "not slow".
fn startup_cost_sentence(
    cost: Option<&StartupCost>,
    matched: Option<&boot_perf::SlowSummary>,
) -> String {
    let Some(cost) = cost else {
        return "Not read yet.".to_string();
    };
    match (&cost.access, matched) {
        (boot_perf::Access::Readable, Some(s)) => {
            let time = match (s.worst_delay_ms, s.worst_total_ms) {
                (Some(d), Some(t)) => format!(
                    "up to {} longer than Windows expected (took {})",
                    boot_perf::duration_text(d),
                    boot_perf::duration_text(t)
                ),
                (Some(d), None) => format!(
                    "up to {} longer than Windows expected",
                    boot_perf::duration_text(d)
                ),
                (None, Some(t)) => format!("took up to {}", boot_perf::duration_text(t)),
                (None, None) => "no time was recorded".to_string(),
            };
            format!(
                "Windows measured this as slow to start: {time}. Named {} in the last 30 days.",
                if s.times == 1 {
                    "once".to_string()
                } else {
                    format!("{} times", s.times)
                }
            )
        }
        (boot_perf::Access::Readable, None) if cost.incomplete => {
            "Windows has not named this item as slow in the part of its boot log that could be read. The log may be incomplete.".to_string()
        }
        (boot_perf::Access::Readable, None) => {
            "Windows has not named this item as slow in its boot log.".to_string()
        }
        (boot_perf::Access::Denied, _) => format!(
            "Not available: {}. Add-ons do not run as administrator. {}",
            boot_perf::ADMIN_ONLY,
            boot_perf::ALLOW_HOW
        ),
        (boot_perf::Access::Missing, _) => {
            "Not available: this PC has no boot timing log.".to_string()
        }
        (boot_perf::Access::Failed(why), _) => {
            format!("Not available: {}.", why.trim_end_matches('.'))
        }
    }
}

fn startup_undo_summary(done: usize, failed: usize, refused: &[String]) -> String {
    let line = if failed > 0 {
        format!("Undid {done}, {failed} failed. They stay in the undo list.")
    } else if done == 0 && !refused.is_empty() {
        "Nothing was undone.".to_string()
    } else {
        format!("Undid {}.", plural(done, "change", "changes"))
    };
    with_refused(line, refused)
}

impl Debloater {
    fn startup_refresh(&mut self, ctx: &egui::Context, read_cost: bool) {
        self.startup.started = true;
        if self.startup.loading.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        let can_change_user_profile = self.can_change_user_profile;
        std::thread::spawn(move || {
            let entries = startup::entries(&startup::WinStartup, &startup::publisher_of_command)
                .map(|mut entries| {
                    if !can_change_user_profile {
                        entries.retain(|entry| entry.source.needs_admin());
                    }
                    entries
                });
            let cost = read_cost.then(|| {
                let read = boot_perf::load(&boot_perf::WinBootSource, COST_LOOKBACK_MS);
                StartupCost {
                    access: read.access,
                    incomplete: read.log.incomplete,
                    slow: boot_perf::summarize(&read.log.slow),
                }
            });
            let _ = tx.send(StartupLoad { entries, cost });
            ctx.request_repaint();
        });
        self.startup.loading = Some(rx);
    }

    fn startup_poll(&mut self) {
        let Some(rx) = &self.startup.loading else {
            return;
        };
        match rx.try_recv() {
            Ok(load) => {
                self.startup.loading = None;
                if load.cost.is_some() {
                    self.startup.cost = load.cost;
                }
                match load.entries {
                    Ok(entries) => {
                        self.startup.entries = entries;
                        self.startup.error = None;
                    }
                    Err(e) => self.startup.error = Some(e),
                }
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.startup.loading = None;
                self.startup.error = Some("Reading the startup items stopped unexpectedly.".into());
            }
        }
    }

    /// What Windows measured for this entry, if the log was read and names exactly this program.
    fn startup_cost(&self, entry: &Entry) -> Option<&boot_perf::SlowSummary> {
        let cost = self.startup.cost.as_ref()?;
        if cost.access != boot_perf::Access::Readable {
            return None;
        }
        boot_perf::find_match(&cost.slow, &entry.name, &entry.command)
    }

    fn startup_find(&self, key: &(Source, String)) -> Option<&Entry> {
        self.startup
            .entries
            .iter()
            .find(|e| e.source == key.0 && e.name == key.1)
    }

    /// One registry write on the UI thread, then reload the list.
    fn startup_set(&mut self, entry: &Entry, enable: bool) {
        self.startup.pending = None;
        if entry.source.needs_admin() {
            if self.admin_pending.is_some() {
                return;
            }
            self.pending_startup = Some((entry.name.clone(), enable));
            self.send_admin(
                AdminKind::StartupSet,
                admin::startup_set_request(entry, enable),
            );
            return;
        }
        let result = startup::set_enabled(&mut startup::WinStartup, entry, enable, filetime_now());
        self.startup.message = Some(match result {
            Ok(step) => {
                self.startup
                    .steps
                    .push(saved_undo::startup_done(step, enable));
                format!(
                    "{} is now {}.",
                    entry.name,
                    if enable { "on" } else { "off" }
                )
            }
            Err(e) => {
                let message = format!("Could not change {}: {e}", entry.name);
                self.startup
                    .steps
                    .extend(e.recovery.map(saved_undo::recovery));
                message
            }
        });
        self.save_undo();
        self.startup.started = false;
    }

    fn startup_undo(&mut self, all: bool) {
        if self.admin_pending.is_some() {
            return;
        }
        // The steps this click would undo, newest last. If any of them is an all-users entry the
        // helper undoes the whole batch; otherwise it runs here as before.
        let take = if all {
            self.startup.steps.len()
        } else {
            self.startup.steps.len().min(1)
        };
        let from = self.startup.steps.len() - take;
        // A step whose entry is gone (the app was uninstalled) is dropped with the reason before
        // anything is sent, so it can never block the undo of the others, also after a restart.
        // If the list cannot be read, nothing is dropped and the undo itself checks each entry.
        let dropped = match startup::WinStartup.list() {
            Ok(entries) => saved_undo::drop_gone_startup(&mut self.startup.steps, from, |step| {
                entries
                    .iter()
                    .any(|e| e.source == step.step.source && e.name == step.step.name)
            }),
            Err(_) => Vec::new(),
        };
        if self.startup.steps.len() == from {
            if !dropped.is_empty() {
                self.startup.message = Some(startup_undo_summary(0, 0, &dropped));
                self.save_undo();
            }
            self.startup.pending = None;
            self.startup.started = false;
            return;
        }
        if self.startup.steps[from..]
            .iter()
            .any(|step| step.step.source.needs_admin())
        {
            let mut batch = self.startup.steps.split_off(from);
            batch.reverse();
            let request = admin::startup_undo_request(&batch);
            self.startup_batch = batch;
            self.startup_rest = std::mem::take(&mut self.startup.steps);
            self.startup_dropped = dropped;
            self.send_admin(AdminKind::StartupUndo, request);
            return;
        }
        let (done, failed, refused) = run_undo(
            &mut self.startup.steps,
            all,
            |step| (step.step.source, step.step.name.clone()),
            |step| saved_undo::undo_startup(step, &mut startup::WinStartup),
        );
        let mut reasons = dropped;
        reasons.extend(refused);
        self.startup.message = Some(startup_undo_summary(done, failed, &reasons));
        self.save_undo();
        self.startup.pending = None;
        self.startup.started = false;
    }

    fn startup_page(&mut self, ui: &mut egui::Ui) {
        if !self.can_change_user_profile {
            style::note(ui, elevation::USER_PROFILE_BLOCKED);
        }
        let admin = if self.env.elevated {
            "Running as administrator"
        } else {
            "Not running as administrator"
        };
        let status = format!("{} \u{b7} {admin}", edition_label(self.env.edition));
        let busy = self.startup.loading.is_some();
        if style::page_head(ui, &status, !busy) {
            self.startup_refresh(ui.ctx(), true);
        }
        if let Some(message) = &self.startup.message {
            style::note(ui, message);
        }
        ui.add_space(GAP);

        let mut action: Option<(Entry, bool)> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if let Some(error) = &self.startup.error {
                    ui.label(error.as_str());
                } else if self.startup.loading.is_some() && self.startup.entries.is_empty() {
                    style::note(ui, "Collecting startup items...");
                } else if self.startup.entries.is_empty() {
                    style::note(ui, "No startup items found.");
                }
                let entries = self.startup.entries.clone();
                let [safe, advanced, protected] = group_startup(&entries);
                if !safe.is_empty() {
                    style::section(ui, "Safe to switch off", None);
                    style::group_card(ui, |ui| {
                        for e in &safe {
                            action = action.take().or(self.startup_row(ui, e));
                        }
                    });
                    ui.add_space(SPACE_M);
                }
                if !advanced.is_empty() {
                    egui::CollapsingHeader::new(
                        RichText::new("Advanced: may affect your hardware or apps")
                            .size(ui::TEXT_SECTION)
                            .strong(),
                    )
                    .default_open(false)
                    .show(ui, |ui| {
                        style::group_card(ui, |ui| {
                            for e in &advanced {
                                action = action.take().or(self.startup_row(ui, e));
                            }
                        });
                    });
                    ui.add_space(SPACE_M);
                }
                if !protected.is_empty() {
                    style::section(ui, "Protected", Some(NEVER_TIP));
                    style::group_card(ui, |ui| {
                        for e in &protected {
                            action = action.take().or(self.startup_row(ui, e));
                        }
                    });
                    ui.add_space(SPACE_M);
                }
            });

        if let Some((entry, enable)) = action {
            self.startup.focus = Some((entry.source, entry.name.clone()));
            if !enable && entry.class == Class::Careful {
                self.startup.pending = Some((entry.source, entry.name.clone()));
            } else {
                self.startup_set(&entry, enable);
            }
        }
    }

    /// Draws one row; returns the requested new state if the switch was clicked.
    fn startup_row(&mut self, ui: &mut egui::Ui, entry: &Entry) -> Option<(Entry, bool)> {
        let key = (entry.source, entry.name.clone());
        let focused = self.startup.focus.as_ref() == Some(&key);
        let blocked = needs_admin_now(entry, self.machine_ok());
        let locked = entry.class == Class::Never;

        let mut chips = Vec::new();
        if blocked {
            chips.push(Chip::new("Needs admin", AMBER));
        }
        // The measured cost, only when Windows named this exact program as slow. No match: nothing.
        let cost = self.startup_cost(entry).map(startup_cost_chip);
        if let Some(text) = cost {
            chips.push(Chip::new(text, AMBER));
        }
        chips.push(Chip::new(class_text(entry.class), class_color(entry.class)));
        let (response, body) = style::row(ui, focused, &chips);
        if response.clicked() {
            self.startup.focus = Some(key);
        }

        let text_color = if locked || blocked { MUTED } else { ui::TEXT };
        let mut on = entry.enabled;
        let mut changed = false;
        style::row_body(ui, body, |ui| {
            ui.add_enabled_ui(!locked && !blocked, |ui| {
                let r = ui::toggle_switch(ui, &mut on);
                changed = r.changed();
                if locked {
                    r.on_disabled_hover_text(NEVER_TIP);
                }
            });
            style::row_title(ui, &entry.name, text_color);
            style::row_detail(ui, publisher_text(entry));
            if !blocked {
                style::row_detail(ui, entry.source.label());
            }
        });
        changed.then(|| (entry.clone(), on))
    }

    fn startup_inspector(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.spacing_mut().item_spacing.y = 4.0;

        // Inline confirm for a Careful entry.
        if let Some(key) = self.startup.pending.clone() {
            match self.startup_find(&key).cloned() {
                Some(entry) => {
                    style::panel_heading(ui, "Switch off?");
                    ui.label(confirm_off_text(&entry));
                    ui.horizontal(|ui| {
                        if ui.add(style::primary_button("Switch off")).clicked() {
                            self.startup_set(&entry, false);
                        }
                        if ui.button("Cancel").clicked() {
                            self.startup.pending = None;
                        }
                    });
                    style::divider(ui);
                }
                None => self.startup.pending = None,
            }
        }

        style::panel_heading(ui, "About this item");
        let focus = self
            .startup
            .focus
            .clone()
            .and_then(|k| self.startup_find(&k).cloned());
        match focus {
            None => {
                style::note(ui, "Click an item to see what it is.");
            }
            Some(e) => {
                ui.label(RichText::new(&e.name).strong());
                ui.add(egui::Label::new(RichText::new(&e.command).small()).wrap());
                style::field_label(ui, "Publisher");
                ui.label(publisher_text(&e));
                style::field_label(ui, "Source");
                ui.label(e.source.label());
                style::field_label(ui, "Startup cost");
                let cost = self.startup_cost(&e);
                ui.add(
                    egui::Label::new(startup_cost_sentence(self.startup.cost.as_ref(), cost))
                        .wrap(),
                );
                ui.add_space(4.0);
                ui.label(class_sentence(e.class));
            }
        }

        if self
            .startup
            .entries
            .iter()
            .any(|e| e.class != Class::Never && e.source.needs_admin())
        {
            style::divider(ui);
            ui.label("Items for all users need administrator rights.");
            style::note(
                ui,
                "A separate administrator window shows exactly what will change. Confirm or cancel there.",
            );
        }

        style::divider(ui);
        style::panel_heading(ui, "Undo");
        let count = self.startup.steps.len();
        ui.label(plural(count, "change to undo", "changes to undo"));
        let has = count > 0;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(has, egui::Button::new("Undo last change"))
                .clicked()
            {
                self.startup_undo(false);
            }
            if ui.add_enabled(has, egui::Button::new("Undo all")).clicked() {
                self.startup_undo(true);
            }
        });
        if has && ui.link(RichText::new("Copy undo steps").small()).clicked() {
            ctx.copy_text(startup::undo_text(&plain(&self.startup.steps)));
        }
        style::note(ui, STARTUP_NOTE);
        self.saved_undo_panel(ui);
    }
}

const TASKS_NEVER_NOTE: &str =
    "Update, security, repair and system maintenance tasks are never offered.";
const TASKS_NOTE: &str = "Switching a task off only pauses it. You can switch it back on here.";

/// Text for the small state column of a task row.
fn task_state_text(state: &TaskState) -> &'static str {
    match state {
        TaskState::Enabled => "On",
        TaskState::Disabled => "Off",
        TaskState::NotPresent => "Not on this PC",
        TaskState::Protected => "Protected by Windows",
        TaskState::Unknown(_) => "Unknown",
    }
}

/// Only tasks that exist and are readable have a usable switch.
fn task_switchable(state: &TaskState) -> bool {
    matches!(state, TaskState::Enabled | TaskState::Disabled)
}

/// Catalog joined with statuses by id; a task without a status reads as Unknown.
fn join_tasks(defs: &[TaskDef], statuses: &[tasks::TaskStatus]) -> Vec<(TaskDef, TaskState)> {
    defs.iter()
        .map(|d| {
            let state = statuses.iter().find(|s| s.id == d.id).map_or_else(
                || TaskState::Unknown("not read".into()),
                |s| s.state.clone(),
            );
            (*d, state)
        })
        .collect()
}

/// Joined rows split into (safe, advanced), order kept.
fn group_tasks(rows: &[(TaskDef, TaskState)]) -> [Vec<&(TaskDef, TaskState)>; 2] {
    let mut groups: [Vec<&(TaskDef, TaskState)>; 2] = [Vec::new(), Vec::new()];
    for row in rows {
        match row.0.risk {
            Risk::Safe => groups[0].push(row),
            Risk::Careful => groups[1].push(row),
        }
    }
    groups
}

fn confirm_task_text(def: &TaskDef) -> String {
    format!(
        "Switch off {}? {} You can switch it back on.",
        def.title, def.could_break
    )
}

impl Debloater {
    fn tasks_refresh(&mut self, ctx: &egui::Context) {
        self.tasks.started = true;
        if self.tasks.loading.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(tasks::statuses(&tasks::WinTasks));
            ctx.request_repaint();
        });
        self.tasks.loading = Some(rx);
    }

    fn tasks_poll(&mut self) {
        if let Some(rx) = &self.tasks.loading {
            match rx.try_recv() {
                Ok(list) => {
                    self.tasks.loading = None;
                    self.tasks.statuses = list;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.tasks.loading = None;
                    self.tasks.message = Some("Reading the tasks stopped unexpectedly.".into());
                }
            }
        }
        let Some(rx) = &self.tasks.working else {
            return;
        };
        let job = match rx.try_recv() {
            Ok(job) => Some(job),
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => None,
        };
        self.tasks.working = None;
        self.tasks.started = false; // reload right away
        self.tasks.message = Some(match job {
            Some(TaskJob::Set(title, result, point)) => {
                if let Some(status) = &point {
                    self.restore.tasks_done = restore::has_restore_point(status);
                    self.restore.status = Some(restore::status_text(status));
                }
                match result {
                    Ok(step) => {
                        let now = if step.was_enabled { "off" } else { "on" };
                        self.tasks.steps.push(saved_undo::task_done(step));
                        format!("{title} is now {now}.")
                    }
                    Err(e) => {
                        let message = format!("Could not change {title}: {e}");
                        self.tasks
                            .steps
                            .extend(e.recovery.map(saved_undo::recovery));
                        message
                    }
                }
            }
            Some(TaskJob::Undo {
                remaining,
                done,
                failed,
                refused,
            }) => {
                self.tasks.steps = remaining;
                startup_undo_summary(done, failed, &refused)
            }
            None => "The change stopped unexpectedly.".to_string(),
        });
        self.save_undo();
    }

    fn tasks_set(&mut self, _ctx: &egui::Context, def: TaskDef, enable: bool) {
        self.tasks.pending = None;
        if self.tasks.working.is_some() || self.admin_pending.is_some() {
            return;
        }
        let make_point = should_create(self.restore.on, true, self.restore.tasks_done);
        self.restore.status = None;
        let (tx, rx) = mpsc::channel();
        self.tasks.working = Some(rx);
        self.task_tx = Some(tx);
        self.pending_title = def.title.to_string();
        self.send_admin(
            AdminKind::TaskSet,
            admin::task_set_request(def.id, enable, make_point),
        );
    }

    fn tasks_undo(&mut self, _ctx: &egui::Context, all: bool) {
        self.tasks.pending = None;
        if self.tasks.working.is_some() || self.admin_pending.is_some() {
            return;
        }
        let mut steps = std::mem::take(&mut self.tasks.steps);
        let mut batch = Vec::new();
        run_undo(
            &mut steps,
            all,
            |step| step.step.path,
            |s| {
                batch.push(s.clone());
                StepResult::Done
            },
        );
        if batch.is_empty() {
            self.tasks.steps = steps;
            return;
        }
        let request = admin::task_undo_request(&batch);
        self.task_batch = batch;
        self.task_rest = steps;
        let (tx, rx) = mpsc::channel();
        self.tasks.working = Some(rx);
        self.task_tx = Some(tx);
        self.send_admin(AdminKind::TaskUndo, request);
    }

    fn tasks_busy(&self) -> bool {
        self.tasks.loading.is_some() || self.tasks.working.is_some()
    }

    fn tasks_page(&mut self, ui: &mut egui::Ui) {
        let admin = if self.env.elevated {
            "Running as administrator"
        } else {
            "Not running as administrator"
        };
        if style::page_head(ui, admin, !self.tasks_busy()) {
            self.tasks_refresh(ui.ctx());
        }
        if let Some(message) = &self.tasks.message {
            style::note(ui, message);
        }
        if let Some(line) = &self.restore.status {
            style::note(ui, line);
        }
        ui.add_space(GAP);

        let mut action: Option<(TaskDef, bool)> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let rows = join_tasks(tasks::catalog(), &self.tasks.statuses);
                if self.tasks.loading.is_some() && self.tasks.statuses.is_empty() {
                    style::note(ui, "Collecting tasks...");
                } else if rows.is_empty() {
                    style::note(ui, "No tasks to show.");
                }
                let [safe, advanced] = group_tasks(&rows);
                if !safe.is_empty() {
                    style::section(ui, "Safe to switch off", None);
                    style::group_card(ui, |ui| {
                        for (def, state) in safe {
                            action = action.take().or(self.task_row(ui, def, state));
                        }
                    });
                    ui.add_space(SPACE_M);
                }
                if !advanced.is_empty() {
                    egui::CollapsingHeader::new(
                        RichText::new("Advanced: may affect your hardware or apps")
                            .size(ui::TEXT_SECTION)
                            .strong(),
                    )
                    .default_open(false)
                    .show(ui, |ui| {
                        style::group_card(ui, |ui| {
                            for (def, state) in advanced {
                                action = action.take().or(self.task_row(ui, def, state));
                            }
                        });
                    });
                    ui.add_space(SPACE_M);
                }
                style::note(ui, TASKS_NEVER_NOTE);
            });

        if let Some((def, enable)) = action {
            self.tasks.focus = Some(def.id);
            if !enable && def.risk == Risk::Careful {
                self.tasks.pending = Some(def.id);
            } else {
                self.tasks_set(&ui.ctx().clone(), def, enable);
            }
        }
    }

    /// Draws one row; returns the requested new state if the switch was clicked.
    fn task_row(
        &mut self,
        ui: &mut egui::Ui,
        def: &TaskDef,
        state: &TaskState,
    ) -> Option<(TaskDef, bool)> {
        let focused = self.tasks.focus == Some(def.id);
        let switchable = task_switchable(state);
        let usable = switchable && !self.tasks_busy();

        let state_chip = {
            let color = if *state == TaskState::Disabled {
                GREEN
            } else {
                MUTED
            };
            Chip::new(task_state_text(state), color)
        };
        let chips = [
            style::evidence_chip(&def.evidence()),
            state_chip,
            risk_chip(def.risk, switchable),
        ];
        let (response, body) = style::row(ui, focused, &chips);
        if response.clicked() {
            self.tasks.focus = Some(def.id);
        }

        let text_color = if switchable { ui::TEXT } else { MUTED };
        let mut on = *state == TaskState::Enabled;
        let mut changed = false;
        style::row_body(ui, body, |ui| {
            ui.add_enabled_ui(usable, |ui| {
                changed = ui::toggle_switch(ui, &mut on).changed();
            });
            style::row_title(ui, def.title, text_color);
        });
        changed.then_some((*def, on))
    }

    fn tasks_inspector(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.spacing_mut().item_spacing.y = 4.0;

        // Inline confirm for a Careful task.
        if let Some(id) = self.tasks.pending {
            match tasks::catalog().iter().find(|d| d.id == id) {
                Some(def) => {
                    style::panel_heading(ui, "Switch off?");
                    ui.label(confirm_task_text(def));
                    ui.horizontal(|ui| {
                        if ui.add(style::primary_button("Switch off")).clicked() {
                            self.tasks_set(&ctx, *def, false);
                        }
                        if ui.button("Cancel").clicked() {
                            self.tasks.pending = None;
                        }
                    });
                    style::divider(ui);
                }
                None => self.tasks.pending = None,
            }
        }

        style::panel_heading(ui, "About this item");
        let def = self
            .tasks
            .focus
            .and_then(|id| tasks::catalog().iter().find(|d| d.id == id));
        match def {
            None => {
                style::note(ui, "Click an item to see what it changes.");
            }
            Some(def) => {
                ui.label(RichText::new(def.title).strong());
                ui.label(def.what_changes);
                style::field_label(ui, "What could break");
                ui.label(def.could_break);
                style::evidence_block(ui, &def.evidence());
                style::field_label(ui, "Task");
                ui.add(egui::Label::new(RichText::new(def.path).small()).wrap());
            }
        }

        style::divider(ui);
        ui.label("Changing a task needs administrator rights.");
        style::note(
            ui,
            "A separate administrator window shows exactly what will change. Confirm or cancel there.",
        );

        style::divider(ui);
        let busy = self.tasks_busy();
        self.restore_switch(ui, busy, true);
        if self.restore.on && self.machine_ok() && self.restore.tasks_done {
            style::note(ui, "A restore point was already made this session.");
        }

        style::divider(ui);
        style::panel_heading(ui, "Undo");
        let working = self.tasks.working.is_some();
        if working {
            style::note(ui, "Working...");
        }
        let count = self.tasks.steps.len();
        ui.label(plural(count, "change to undo", "changes to undo"));
        let has = count > 0 && !working;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(has, egui::Button::new("Undo last change"))
                .clicked()
            {
                self.tasks_undo(&ctx, false);
            }
            if ui.add_enabled(has, egui::Button::new("Undo all")).clicked() {
                self.tasks_undo(&ctx, true);
            }
        });
        if count > 0 && ui.link(RichText::new("Copy undo steps").small()).clicked() {
            ctx.copy_text(tasks::undo_text(&plain(&self.tasks.steps)));
        }
        style::note(ui, TASKS_NOTE);
        self.saved_undo_panel(ui);
    }
}

const SERVICES_NEVER_NOTE: &str =
    "Update, security, firewall and system services are never offered.";
const SERVICES_CAREFUL_NOTE: &str =
    "Careful items are set to Manual, not Disabled, so Windows can still start them when needed.";
const SERVICES_TAKES_EFFECT: &str =
    "This takes effect when Windows restarts or the service stops. \
                                     toolboks never stops a running service.";
const SERVICES_NOTE: &str = "You can also change a service back in Services in Windows.";
const SERVICES_CHANGED_TIP: &str = "Use Undo, or Services in Windows, to change it back";

/// Text for the small start type column of a service row.
fn service_state_text(state: &ServiceState) -> String {
    match state {
        ServiceState::Present { start, running, .. } => {
            let label = match start {
                services::StartType::Other(_) => "Unknown",
                other => other.label(),
            };
            if *running {
                format!("{label} (running)")
            } else {
                label.to_string()
            }
        }
        ServiceState::NotPresent => "Not on this PC".to_string(),
        ServiceState::Locked => "Protected by Windows".to_string(),
        ServiceState::Unknown(_) => "Unknown".to_string(),
    }
}

/// Only services that exist and are readable have a usable switch.
fn service_switchable(state: &ServiceState) -> bool {
    matches!(state, ServiceState::Present { .. })
}

/// The switch is on while the service still has its original (non-target) start type.
fn service_on(def: &ServiceDef, state: &ServiceState) -> bool {
    match state {
        ServiceState::Present { start, .. } => !def.is_off(*start),
        _ => true,
    }
}

/// Catalog joined with statuses by id; a service without a status reads as Unknown.
fn join_services(
    defs: &[ServiceDef],
    statuses: &[services::ServiceStatus],
) -> Vec<(ServiceDef, ServiceState)> {
    defs.iter()
        .map(|d| {
            let state = statuses.iter().find(|s| s.id == d.id).map_or_else(
                || ServiceState::Unknown("not read".into()),
                |s| s.state.clone(),
            );
            (*d, state)
        })
        .collect()
}

type ServiceRow = (ServiceDef, ServiceState);

/// Joined rows split into (safe, advanced), order kept.
fn group_services(rows: &[ServiceRow]) -> [Vec<&ServiceRow>; 2] {
    let mut groups: [Vec<&ServiceRow>; 2] = [Vec::new(), Vec::new()];
    for row in rows {
        match row.0.risk {
            Risk::Safe => groups[0].push(row),
            Risk::Careful => groups[1].push(row),
        }
    }
    groups
}

/// Undoes the newest step of one service. A step that fails stays in the list.
/// Returns (undone, failed).
fn undo_for_id(
    steps: &mut Vec<ServiceStep>,
    id: &str,
    undo_one: impl FnOnce(&ServiceStep) -> StepResult,
) -> (usize, usize) {
    let Some(index) = steps.iter().rposition(|s| s.step.id == id) else {
        return (0, 0);
    };
    if undo_one(&steps[index]) == StepResult::Done {
        steps.remove(index);
        (1, 0)
    } else {
        (0, 1)
    }
}

/// The engine's own steps, for its "Copy undo steps" text.
fn plain<T: Clone, V>(steps: &[saved_undo::Tracked<T, V>]) -> Vec<T> {
    steps.iter().map(|s| s.step.clone()).collect()
}

impl Debloater {
    fn services_refresh(&mut self, ctx: &egui::Context) {
        self.services.started = true;
        if self.services.loading.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(services::statuses(&services::WinServices));
            ctx.request_repaint();
        });
        self.services.loading = Some(rx);
    }

    fn services_poll(&mut self) {
        if let Some(rx) = &self.services.loading {
            match rx.try_recv() {
                Ok(list) => {
                    self.services.loading = None;
                    self.services.statuses = list;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.services.loading = None;
                    self.services.message =
                        Some("Reading the services stopped unexpectedly.".into());
                }
            }
        }
        let Some(rx) = &self.services.working else {
            return;
        };
        let job = match rx.try_recv() {
            Ok(job) => Some(job),
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => None,
        };
        self.services.working = None;
        self.services.started = false; // reload right away
        self.services.message = Some(match job {
            Some(ServiceJob::Set(title, result, point)) => {
                if let Some(status) = &point {
                    self.restore.services_done = restore::has_restore_point(status);
                    self.restore.status = Some(restore::status_text(status));
                }
                match result {
                    Ok(step) => {
                        self.services.steps.push(saved_undo::service_done(step));
                        format!("{title} is now switched off.")
                    }
                    Err(e) => {
                        let message = format!("Could not change {title}: {e}");
                        self.services
                            .steps
                            .extend(e.recovery.map(saved_undo::recovery));
                        message
                    }
                }
            }
            Some(ServiceJob::Undo {
                remaining,
                done,
                failed,
                refused,
            }) => {
                self.services.steps = remaining;
                startup_undo_summary(done, failed, &refused)
            }
            None => "The change stopped unexpectedly.".to_string(),
        });
        self.save_undo();
    }

    fn services_set_off(&mut self, _ctx: &egui::Context, def: ServiceDef) {
        self.services.pending = None;
        if self.services.working.is_some() || self.admin_pending.is_some() {
            return;
        }
        let make_point = should_create(self.restore.on, true, self.restore.services_done);
        self.restore.status = None;
        let (tx, rx) = mpsc::channel();
        self.services.working = Some(rx);
        self.service_tx = Some(tx);
        self.pending_title = def.title.to_string();
        self.send_admin(
            AdminKind::ServiceSet,
            admin::service_set_request(def.id, make_point),
        );
    }

    /// Undo everything (`all`), the newest step, or the newest step of one service (`only`). The
    /// steps are chosen here and undone by the helper, which asks for confirmation first.
    fn services_undo(&mut self, _ctx: &egui::Context, all: bool, only: Option<&'static str>) {
        self.services.pending = None;
        if self.services.working.is_some() || self.admin_pending.is_some() {
            return;
        }
        let mut steps = std::mem::take(&mut self.services.steps);
        let mut batch = Vec::new();
        let mut pick = |s: &ServiceStep| {
            batch.push(s.clone());
            StepResult::Done
        };
        match only {
            Some(id) => {
                undo_for_id(&mut steps, id, &mut pick);
            }
            None => {
                run_undo(&mut steps, all, |step| step.step.name, &mut pick);
            }
        }
        if batch.is_empty() {
            self.services.steps = steps;
            return;
        }
        let request = admin::service_undo_request(&batch);
        self.service_batch = batch;
        self.service_rest = steps;
        let (tx, rx) = mpsc::channel();
        self.services.working = Some(rx);
        self.service_tx = Some(tx);
        self.send_admin(AdminKind::ServiceUndo, request);
    }

    fn services_busy(&self) -> bool {
        self.services.loading.is_some() || self.services.working.is_some()
    }

    fn services_page(&mut self, ui: &mut egui::Ui) {
        let admin = if self.env.elevated {
            "Running as administrator"
        } else {
            "Not running as administrator"
        };
        if style::page_head(ui, admin, !self.services_busy()) {
            self.services_refresh(ui.ctx());
        }
        if let Some(message) = &self.services.message {
            style::note(ui, message);
        }
        if let Some(line) = &self.restore.status {
            style::note(ui, line);
        }
        ui.add_space(GAP);

        let mut action: Option<(ServiceDef, bool)> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let rows = join_services(services::catalog(), &self.services.statuses);
                if self.services.loading.is_some() && self.services.statuses.is_empty() {
                    style::note(ui, "Collecting services...");
                } else if rows.is_empty() {
                    style::note(ui, "No services to show.");
                }
                let [safe, advanced] = group_services(&rows);
                if !safe.is_empty() {
                    style::section(ui, "Safe to switch off", None);
                    style::group_card(ui, |ui| {
                        for (def, state) in safe {
                            action = action.take().or(self.service_row(ui, def, state));
                        }
                    });
                    ui.add_space(SPACE_M);
                }
                if !advanced.is_empty() {
                    egui::CollapsingHeader::new(
                        RichText::new("Advanced: may affect your hardware or apps")
                            .size(ui::TEXT_SECTION)
                            .strong(),
                    )
                    .default_open(false)
                    .show(ui, |ui| {
                        style::note(ui, SERVICES_CAREFUL_NOTE);
                        style::group_card(ui, |ui| {
                            for (def, state) in advanced {
                                action = action.take().or(self.service_row(ui, def, state));
                            }
                        });
                    });
                    ui.add_space(SPACE_M);
                }
                style::note(ui, SERVICES_NEVER_NOTE);
            });

        if let Some((def, enable)) = action {
            self.services.focus = Some(def.id);
            if enable {
                self.services_undo(&ui.ctx().clone(), false, Some(def.id));
            } else {
                self.services.pending = Some(def.id);
            }
        }
    }

    /// Draws one row; returns the requested new state if the switch was clicked.
    fn service_row(
        &mut self,
        ui: &mut egui::Ui,
        def: &ServiceDef,
        state: &ServiceState,
    ) -> Option<(ServiceDef, bool)> {
        let focused = self.services.focus == Some(def.id);
        let switchable = service_switchable(state);
        let on = service_on(def, state);
        let has_step = self.services.steps.iter().any(|s| s.step.id == def.id);
        let can_revert = on || has_step;
        let usable = switchable && !self.services_busy() && can_revert;

        let chips = [
            style::evidence_chip(&def.evidence()),
            Chip::new(service_state_text(state), MUTED),
            risk_chip(def.risk, switchable),
        ];
        let (response, body) = style::row(ui, focused, &chips);
        if response.clicked() {
            self.services.focus = Some(def.id);
        }

        let text_color = if switchable { ui::TEXT } else { MUTED };
        let tip = if !switchable {
            None
        } else if !can_revert {
            Some(SERVICES_CHANGED_TIP)
        } else {
            None
        };
        let mut on_now = on;
        let mut changed = false;
        style::row_body(ui, body, |ui| {
            ui.add_enabled_ui(usable, |ui| {
                let r = ui::toggle_switch(ui, &mut on_now);
                changed = r.changed();
                if let Some(tip) = tip {
                    r.on_disabled_hover_text(tip);
                }
            });
            style::row_title(ui, def.title, text_color);
        });
        changed.then_some((*def, on_now))
    }

    fn services_inspector(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.spacing_mut().item_spacing.y = 4.0;

        // Inline confirm, always, for Safe and Careful services.
        if let Some(id) = self.services.pending {
            match services::catalog().iter().find(|d| d.id == id) {
                Some(def) => {
                    style::panel_heading(ui, "Switch off?");
                    ui.label(format!("Switch off {}? {}", def.title, def.could_break));
                    style::note(ui, SERVICES_TAKES_EFFECT);
                    ui.horizontal(|ui| {
                        if ui.add(style::primary_button("Switch off")).clicked() {
                            self.services_set_off(&ctx, *def);
                        }
                        if ui.button("Cancel").clicked() {
                            self.services.pending = None;
                        }
                    });
                    style::divider(ui);
                }
                None => self.services.pending = None,
            }
        }

        style::panel_heading(ui, "About this item");
        let def = self
            .services
            .focus
            .and_then(|id| services::catalog().iter().find(|d| d.id == id));
        match def {
            None => {
                style::note(ui, "Click an item to see what it changes.");
            }
            Some(def) => {
                ui.label(RichText::new(def.title).strong());
                ui.label(def.what_changes);
                style::field_label(ui, "What could break");
                ui.label(def.could_break);
                style::evidence_block(ui, &def.evidence());
                style::field_label(ui, "Service name");
                ui.add(egui::Label::new(RichText::new(def.name).small()).wrap());
            }
        }

        style::divider(ui);
        ui.label("Changing a service needs administrator rights.");
        style::note(
            ui,
            "A separate administrator window shows exactly what will change. Confirm or cancel there.",
        );

        style::divider(ui);
        let busy = self.services_busy();
        self.restore_switch(ui, busy, true);
        if self.restore.on && self.machine_ok() && self.restore.services_done {
            style::note(ui, "A restore point was already made this session.");
        }

        style::divider(ui);
        style::panel_heading(ui, "Undo");
        let working = self.services.working.is_some();
        if working {
            style::note(ui, "Working...");
        }
        let count = self.services.steps.len();
        ui.label(plural(count, "change to undo", "changes to undo"));
        let has = count > 0 && !working;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(has, egui::Button::new("Undo last change"))
                .clicked()
            {
                self.services_undo(&ctx, false, None);
            }
            if ui.add_enabled(has, egui::Button::new("Undo all")).clicked() {
                self.services_undo(&ctx, true, None);
            }
        });
        if count > 0 && ui.link(RichText::new("Copy undo steps").small()).clicked() {
            ctx.copy_text(services::undo_text(&plain(&self.services.steps)));
        }
        style::note(ui, SERVICES_NOTE);
        self.saved_undo_panel(ui);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Edition;
    use crate::startup::UndoStep;

    fn task_def(id: &'static str, risk: Risk) -> TaskDef {
        TaskDef {
            id,
            path: "\\T",
            title: id,
            what_changes: "",
            could_break: "It may stop X.",
            risk,
        }
    }

    #[test]
    fn should_create_needs_switch_admin_and_no_earlier_point() {
        assert!(should_create(true, true, false));
        assert!(!should_create(false, true, false));
        assert!(!should_create(true, false, false));
        assert!(!should_create(true, true, true));
    }

    #[test]
    fn tasks_join_group_and_labels() {
        let defs = [
            task_def("a", Risk::Careful),
            task_def("b", Risk::Safe),
            task_def("c", Risk::Safe),
        ];
        let statuses = [
            tasks::TaskStatus {
                id: "b",
                state: TaskState::Enabled,
            },
            tasks::TaskStatus {
                id: "a",
                state: TaskState::Protected,
            },
        ];
        let rows = join_tasks(&defs, &statuses);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].1, TaskState::Protected);
        assert_eq!(rows[1].1, TaskState::Enabled);
        assert!(matches!(rows[2].1, TaskState::Unknown(_)));
        let [safe, adv] = group_tasks(&rows);
        assert_eq!(safe.iter().map(|r| r.0.id).collect::<Vec<_>>(), ["b", "c"]);
        assert_eq!(adv.len(), 1);
        let [s, a] = group_tasks(&[]);
        assert!(s.is_empty() && a.is_empty());
        assert!(join_tasks(&[], &statuses).is_empty());

        assert_eq!(task_state_text(&TaskState::Enabled), "On");
        assert_eq!(task_state_text(&TaskState::Disabled), "Off");
        assert_eq!(task_state_text(&TaskState::NotPresent), "Not on this PC");
        assert_eq!(
            task_state_text(&TaskState::Protected),
            "Protected by Windows"
        );
        assert_eq!(task_state_text(&TaskState::Unknown("x".into())), "Unknown");
        assert!(task_switchable(&TaskState::Disabled));
        assert!(!task_switchable(&TaskState::Protected));
        assert!(!task_switchable(&TaskState::NotPresent));
        assert!(confirm_task_text(&defs[0]).contains("It may stop X."));
    }

    #[test]
    fn run_undo_works_for_task_steps() {
        let step = |id: &'static str| tasks::UndoStep {
            id,
            path: "\\T",
            was_enabled: true,
        };
        let mut steps = vec![step("a"), step("b")];
        let (done, failed, _) = run_undo(
            &mut steps,
            true,
            |step| step.path,
            |s| {
                if s.id == "a" {
                    StepResult::Failed("no".into())
                } else {
                    StepResult::Done
                }
            },
        );
        assert_eq!((done, failed), (1, 1));
        assert_eq!(steps[0].id, "a");
    }

    fn entry(name: &str, class: Class, source: Source) -> Entry {
        Entry {
            source,
            name: name.into(),
            command: String::new(),
            enabled: true,
            publisher: Some("Acme".into()),
            class,
        }
    }

    fn slow(
        kind: boot_perf::SlowKind,
        name: &str,
        delay: Option<u64>,
        total: Option<u64>,
    ) -> boot_perf::SlowItem {
        boot_perf::SlowItem {
            kind,
            logged_ms: 1_762_761_600_000,
            name: name.into(),
            friendly_name: None,
            path: None,
            company: None,
            total_ms: total,
            delay_ms: delay,
        }
    }

    fn cost(access: boot_perf::Access, items: &[boot_perf::SlowItem]) -> StartupCost {
        StartupCost {
            access,
            incomplete: false,
            slow: boot_perf::summarize(items),
        }
    }

    #[test]
    fn startup_cost_shows_only_for_an_exact_match() {
        let c = cost(
            boot_perf::Access::Readable,
            &[
                slow(
                    boot_perf::SlowKind::App,
                    "Spotify.exe",
                    Some(14_000),
                    Some(21_000),
                ),
                slow(
                    boot_perf::SlowKind::App,
                    "Spotify.exe",
                    Some(9_000),
                    Some(12_000),
                ),
            ],
        );
        let mut hit = entry("Spotify", Class::Suggest, Source::UserRun);
        hit.command = r#""C:\Users\me\Spotify.exe" --autostart"#.into();
        let found = boot_perf::find_match(&c.slow, &hit.name, &hit.command).unwrap();
        assert_eq!(startup_cost_chip(found), "Slows start by 14 s");
        let sentence = startup_cost_sentence(Some(&c), Some(found));
        assert!(
            sentence.contains("up to 14 s longer than Windows expected (took 21 s)"),
            "{sentence}"
        );
        assert!(sentence.contains("Named 2 times"), "{sentence}");

        // A different program: nothing, never a guess.
        let mut other = entry("Spotify Helper", Class::Suggest, Source::UserRun);
        other.command = r"C:\x\SpotifyHelper.exe".into();
        assert!(boot_perf::find_match(&c.slow, &other.name, &other.command).is_none());
        assert_eq!(
            startup_cost_sentence(Some(&c), None),
            "Windows has not named this item as slow in its boot log."
        );
    }

    #[test]
    fn startup_cost_chip_without_a_delay() {
        let s =
            boot_perf::summarize(&[slow(boot_perf::SlowKind::Service, "svc", None, Some(9_000))]);
        assert_eq!(startup_cost_chip(&s[0]), "Took 9.0 s to start");
        let s = boot_perf::summarize(&[slow(boot_perf::SlowKind::Service, "svc", None, None)]);
        assert_eq!(startup_cost_chip(&s[0]), "Named slow by Windows");
    }

    #[test]
    fn an_unreadable_boot_log_is_never_described_as_not_slow() {
        for access in [
            boot_perf::Access::Denied,
            boot_perf::Access::Missing,
            boot_perf::Access::Failed("Windows could not read the log (error 87)".into()),
        ] {
            let c = cost(access, &[]);
            let text = startup_cost_sentence(Some(&c), None);
            assert!(text.starts_with("Not available"), "{text}");
            assert!(!text.contains("not named"), "{text}");
        }
        assert_eq!(startup_cost_sentence(None, None), "Not read yet.");
    }

    #[test]
    fn a_denied_boot_log_says_administrators_only_and_how_to_allow_it() {
        let c = cost(boot_perf::Access::Denied, &[]);
        let text = startup_cost_sentence(Some(&c), None);
        assert!(
            text.starts_with(
                "Not available: Windows only lets administrators read the boot log on this PC."
            ),
            "{text}"
        );
        assert!(text.contains(boot_perf::ALLOW_HOW), "{text}");
    }

    #[test]
    fn an_incomplete_boot_log_says_it_may_be_incomplete() {
        let mut c = cost(boot_perf::Access::Readable, &[]);
        c.incomplete = true;
        let text = startup_cost_sentence(Some(&c), None);
        assert!(text.contains("may be incomplete"), "{text}");
        assert!(!text.starts_with("Windows has not named this item as slow in its boot log"));
    }

    #[test]
    fn startup_groups_by_class_keeping_order() {
        let list = [
            entry("a", Class::Careful, Source::UserRun),
            entry("b", Class::Suggest, Source::UserRun),
            entry("c", Class::Never, Source::UserRun),
            entry("d", Class::Suggest, Source::UserRun),
        ];
        let [safe, adv, prot] = group_startup(&list);
        assert_eq!(
            safe.iter().map(|e| &e.name[..]).collect::<Vec<_>>(),
            ["b", "d"]
        );
        assert_eq!(adv.len(), 1);
        assert_eq!(prot[0].name, "c");
        let [s, a, p] = group_startup(&[]);
        assert!(s.is_empty() && a.is_empty() && p.is_empty());
    }

    #[test]
    fn startup_labels() {
        assert_eq!(class_text(Class::Never), "Protected");
        let mut e = entry("Foo", Class::Careful, Source::MachineRun);
        assert_eq!(
            confirm_off_text(&e),
            "Switch off Foo? It may stop Acme features from working. You can switch it back on."
        );
        e.publisher = None;
        assert_eq!(publisher_text(&e), "Unknown publisher");
        assert!(needs_admin_now(&e, false));
        assert!(!needs_admin_now(&e, true));
        assert!(!needs_admin_now(
            &entry("x", Class::Suggest, Source::UserRun),
            false
        ));
        assert_eq!(startup_undo_summary(2, 0, &[]), "Undid 2 changes.");
        assert!(startup_undo_summary(2, 1, &[]).starts_with("Undid 2, 1 failed"));
        let refused = ["Foo was changed by something else.".to_string()];
        assert_eq!(
            startup_undo_summary(0, 0, &refused),
            "Nothing was undone. Left alone: Foo was changed by something else."
        );
    }

    #[test]
    fn filetime_known_points() {
        assert_eq!(filetime_at(std::time::UNIX_EPOCH), 116_444_736_000_000_000);
        let one_sec = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1);
        assert_eq!(filetime_at(one_sec), 116_444_736_000_000_000 + 10_000_000);
        assert!(filetime_now() > 116_444_736_000_000_000);
    }

    #[test]
    fn run_undo_newest_first_and_keeps_failures() {
        let step = |n: &str| UndoStep {
            source: Source::UserRun,
            name: n.into(),
            previous: None,
        };
        let mut steps = vec![step("a"), step("b"), step("c")];
        let mut order = Vec::new();
        let (done, failed, _) = run_undo(
            &mut steps,
            true,
            |step| step.name.clone(),
            |s| {
                order.push(s.name.clone());
                if s.name == "b" {
                    StepResult::Failed("no".into())
                } else {
                    StepResult::Done
                }
            },
        );
        assert_eq!(order, ["c", "b", "a"]);
        assert_eq!((done, failed), (2, 1));
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].name, "b");

        let mut steps = vec![step("a"), step("b")];
        let (done, failed, _) = run_undo(
            &mut steps,
            false,
            |step| step.name.clone(),
            |_| StepResult::Done,
        );
        assert_eq!((done, failed), (1, 0));
        assert_eq!(steps[0].name, "a");
        assert_eq!(
            run_undo(
                &mut Vec::<u8>::new(),
                true,
                |step| *step,
                |_| StepResult::Done
            ),
            (0, 0, Vec::new())
        );

        // A refused step leaves the list with its reason; the others are undone.
        let mut steps = vec![step("a"), step("b")];
        let (done, failed, refused) = run_undo(
            &mut steps,
            true,
            |step| step.name.clone(),
            |s| match s.name.as_str() {
                "b" => StepResult::Refused("b was changed".into()),
                _ => StepResult::Done,
            },
        );
        assert_eq!(
            (done, failed, refused),
            (1, 0, vec!["b was changed".to_string()])
        );
        assert!(steps.is_empty());
    }

    #[test]
    fn undo_does_not_restore_older_values_after_a_newer_step_failed() {
        let step = |name: &str, previous| UndoStep {
            source: Source::UserRun,
            name: name.into(),
            previous,
        };
        let mut steps = vec![
            step("App", None),
            step("Other", None),
            step("App", Some(vec![2])),
        ];
        let mut attempted = Vec::new();
        let (done, failed, _) = run_undo(
            &mut steps,
            true,
            |s| (s.source, s.name.clone()),
            |s| {
                attempted.push(s.name.clone());
                if s.name == "App" {
                    StepResult::Failed("Windows rejected the restore".into())
                } else {
                    StepResult::Done
                }
            },
        );
        assert_eq!(attempted, ["App", "Other"]);
        assert_eq!((done, failed), (1, 2));
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].previous, None);
        assert_eq!(steps[1].previous, Some(vec![2]));
        assert_eq!(
            run_undo(
                &mut steps,
                true,
                |s| (s.source, s.name.clone()),
                |_| StepResult::Done
            ),
            (2, 0, Vec::new())
        );
    }

    fn item(id: &'static str, group: Group, risk: Risk) -> Item {
        Item {
            id,
            group,
            title: id,
            what_changes: "",
            could_break: "",
            risk,
            min_edition: Edition::Home,
            settings: &[],
        }
    }

    fn status(state: State, applicable: Applicable) -> Status {
        Status {
            id: "x",
            state,
            applicable,
        }
    }

    #[test]
    fn state_text_covers_every_case() {
        assert_eq!(state_text(status(State::Active, Applicable::Yes)), "Now on");
        assert_eq!(
            state_text(status(State::Off, Applicable::Yes)),
            "Already off"
        );
        assert_eq!(
            state_text(status(State::Active, Applicable::NeedsAdmin)),
            "Needs admin"
        );
        assert_eq!(
            state_text(status(State::Active, Applicable::EditionNotSupported)),
            "Not on this edition"
        );
        assert_eq!(
            state_text(status(State::Unknown, Applicable::Yes)),
            "Unknown"
        );
    }

    #[test]
    fn only_active_supported_items_are_tickable() {
        assert!(tickable(status(State::Active, Applicable::Yes)));
        assert!(tickable(status(State::Active, Applicable::NeedsAdmin)));
        assert!(!tickable(status(State::Off, Applicable::Yes)));
        assert!(!tickable(status(
            State::Active,
            Applicable::EditionNotSupported
        )));
        assert!(!tickable(status(State::Unknown, Applicable::Yes)));
    }

    #[test]
    fn group_by_risk_splits_and_skips_apps() {
        let items = [
            item("a", Group::PrivacyAds, Risk::Careful),
            item("b", Group::PrivacyAds, Risk::Safe),
            item("c", Group::Apps, Risk::Safe),
            item("d", Group::PrivacyAds, Risk::Safe),
        ];
        let (safe, careful) = group_by_risk(&items);
        assert_eq!(safe.iter().map(|i| i.id).collect::<Vec<_>>(), ["b", "d"]);
        assert_eq!(careful.iter().map(|i| i.id).collect::<Vec<_>>(), ["a"]);
        let (s, c) = group_by_risk(&[]);
        assert!(s.is_empty() && c.is_empty());
    }

    fn run_with(outcomes: &[plan::Outcome]) -> plan::Run {
        plan::Run {
            applied: outcomes
                .iter()
                .map(|o| plan::Applied {
                    id: "x",
                    outcome: o.clone(),
                })
                .collect(),
            undo: Vec::new(),
        }
    }

    #[test]
    fn summarize_counts_outcomes() {
        use plan::Outcome::*;
        let run = run_with(&[Done, Done, NotEffective, Failed("no".into())]);
        assert_eq!(
            summarize(&run),
            "Done: 2, restored after a mismatch: 1, failed: 1"
        );
        assert_eq!(run_notes(&run).len(), 2);
        assert!(summarize(&plan::Run::default()).starts_with("Done: 0"));
    }

    #[test]
    fn undo_summary_wording() {
        assert_eq!(undo_summary(3, 0, &[]), "Restored 3 settings.");
        assert_eq!(undo_summary(1, 0, &[]), "Restored 1 setting.");
        assert!(undo_summary(3, 1, &[]).starts_with("Restored 2 of 3 settings, 1 failed"));
        let refused = ["Ads was changed by something else.".to_string()];
        assert_eq!(
            undo_summary(3, 0, &refused),
            "Restored 2 of 3 settings. Left alone: Ads was changed by something else."
        );
    }

    #[test]
    fn undo_text_lists_previous_values() {
        let runs = [
            vec![saved_undo::recovery(plan::UndoOp {
                id: "unknown-id",
                hive: Hive::CurrentUser,
                path: r"Software\T",
                name: "A",
                previous: Some(RegValue::Dword(1)),
            })],
            vec![saved_undo::recovery(plan::UndoOp {
                id: "unknown-id",
                hive: Hive::LocalMachine,
                path: r"Software\T",
                name: "B",
                previous: None,
            })],
        ];
        let text = undo_text(&runs);
        assert!(text.contains("unknown-id"));
        assert!(text.contains(r"HKEY_CURRENT_USER\Software\T"));
        assert!(text.contains("value: A"));
        assert!(text.contains("previous: number 1"));
        assert!(text.contains("HKEY_LOCAL_MACHINE"));
        assert!(text.contains("previous: not set"));
        assert!(text.find("value: B").unwrap() < text.find("value: A").unwrap());
    }

    #[test]
    fn app_summary_lists_failures() {
        let run = AppRun {
            removed: vec![("a", "A_1".into()), ("b", "B_1".into())],
            failed: vec![("c", "denied".into())],
        };
        let text = app_summary(&run, |id| id.to_uppercase());
        assert_eq!(text, "Removed 2, failed 1\nC: denied");
        assert_eq!(
            app_summary(&AppRun::default(), |id| id.into()),
            "Removed 0, failed 0"
        );
        assert_eq!(app_state_text(true), "Installed");
        assert_eq!(app_state_text(false), "Not installed");
    }

    fn svc_def(id: &'static str, risk: Risk, target: services::StartType) -> ServiceDef {
        ServiceDef {
            id,
            name: id,
            title: id,
            what_changes: "",
            could_break: "",
            risk,
            target,
        }
    }

    fn present(start: services::StartType, running: bool) -> ServiceState {
        ServiceState::Present {
            start,
            running,
            active_dependents: Vec::new(),
        }
    }

    #[test]
    fn services_join_group_and_state_text() {
        use services::StartType::*;
        let defs = [
            svc_def("a", Risk::Careful, Manual),
            svc_def("b", Risk::Safe, Disabled),
            svc_def("c", Risk::Safe, Disabled),
        ];
        let statuses = [
            services::ServiceStatus {
                id: "b",
                state: present(Auto, true),
            },
            services::ServiceStatus {
                id: "a",
                state: ServiceState::Locked,
            },
        ];
        let rows = join_services(&defs, &statuses);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].1, ServiceState::Locked);
        assert!(matches!(rows[2].1, ServiceState::Unknown(_)));
        let [safe, adv] = group_services(&rows);
        assert_eq!(safe.iter().map(|r| r.0.id).collect::<Vec<_>>(), ["b", "c"]);
        assert_eq!(adv.len(), 1);
        assert!(join_services(&[], &statuses).is_empty());

        assert_eq!(service_state_text(&present(Auto, false)), "Automatic");
        assert_eq!(
            service_state_text(&present(Manual, true)),
            "Manual (running)"
        );
        assert_eq!(service_state_text(&present(Disabled, false)), "Disabled");
        assert_eq!(service_state_text(&present(Other(1), false)), "Unknown");
        assert_eq!(
            service_state_text(&ServiceState::NotPresent),
            "Not on this PC"
        );
        assert_eq!(
            service_state_text(&ServiceState::Locked),
            "Protected by Windows"
        );
        assert_eq!(
            service_state_text(&ServiceState::Unknown("x".into())),
            "Unknown"
        );
        assert!(service_switchable(&present(Auto, false)));
        assert!(!service_switchable(&ServiceState::Locked));
    }

    #[test]
    fn services_on_off_derivation() {
        use services::StartType::*;
        let safe = svc_def("s", Risk::Safe, Disabled);
        let careful = svc_def("c", Risk::Careful, Manual);
        assert!(service_on(&safe, &present(Auto, true)));
        assert!(service_on(&safe, &present(Manual, false)));
        assert!(!service_on(&safe, &present(Disabled, false)));
        assert!(service_on(&careful, &present(Auto, false)));
        assert!(!service_on(&careful, &present(Manual, false)));
        assert!(!service_on(&careful, &present(Disabled, false)));
        assert!(service_on(&safe, &ServiceState::NotPresent));
    }

    #[test]
    fn undo_for_id_takes_newest_matching_step() {
        use services::StartType::*;
        let step = |id: &'static str, previous| {
            saved_undo::recovery(services::UndoStep {
                id,
                name: id,
                previous,
            })
        };
        let mut steps = vec![step("a", Auto), step("b", Auto), step("a", Manual)];
        let mut seen = None;
        let r = undo_for_id(&mut steps, "a", |s| {
            seen = Some(s.step.previous);
            StepResult::Done
        });
        assert_eq!(r, (1, 0));
        assert_eq!(seen, Some(Manual));
        assert_eq!(steps.len(), 2);
        assert_eq!(
            undo_for_id(&mut steps, "b", |_| StepResult::Failed("no".into())),
            (0, 1)
        );
        assert_eq!(steps.len(), 2);
        assert_eq!(undo_for_id(&mut steps, "zzz", |_| StepResult::Done), (0, 0));
    }

    #[test]
    fn every_listed_entry_shows_a_badge_and_a_note_and_nothing_is_ticked() {
        let page = Debloater::default();
        assert!(page.selected.is_empty());
        assert!(page.apps.selected.is_empty());
        let mut infos: Vec<(&str, toolboks_debloat_engine::model::EvidenceInfo)> = Vec::new();
        infos.extend(catalog::items().iter().map(|i| (i.id, i.evidence())));
        infos.extend(apps::curated().iter().map(|a| (a.id, a.evidence())));
        infos.extend(tasks::catalog().iter().map(|t| (t.id, t.evidence())));
        infos.extend(services::catalog().iter().map(|s| (s.id, s.evidence())));
        assert_eq!(infos.len(), 27 + 13 + 18 + 20);
        for (id, info) in infos {
            assert!(!style::evidence_chip(&info).text.is_empty(), "{id}");
            assert!(!info.note.is_empty(), "{id}");
            assert!(!style::source_status(&info).is_empty(), "{id}");
            // The host says an entry with no recorded evidence is "Unproven" with this note.
            assert_ne!(
                info.note, "No evidence has been recorded for this entry.",
                "{id} has no evidence entry"
            );
        }
    }

    #[test]
    fn unproven_entries_exist_and_stay_unticked() {
        let unproven = |level: toolboks_debloat_engine::model::Evidence| {
            level == toolboks_debloat_engine::model::Evidence::Unproven
        };
        let ids: Vec<&str> = catalog::items()
            .iter()
            .filter(|i| unproven(i.evidence().level))
            .map(|i| i.id)
            .collect();
        assert!(ids.contains(&"game-bar-capture"));
        let page = Debloater::default();
        assert!(ids.iter().all(|id| !page.selected.contains(id)));
    }

    fn page_with_store(dir: &std::path::Path) -> Debloater {
        Debloater {
            saved: crate::SavedState::new(dir.to_path_buf()),
            ..Debloater::default()
        }
    }

    #[test]
    fn the_page_never_overwrites_a_newer_list_and_reloads_its_own_after_a_restart() {
        let dir = std::env::temp_dir().join(format!(
            "toolboks-debloater-page-undo-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("undo.jsonl");
        let newer: &[u8] = b"{\"format\":\"toolboks.debloater.undo\",\"version\":9}\n{}";
        std::fs::write(&file, newer).unwrap();

        let mut page = page_with_store(&dir);
        page.load_saved_undo();
        assert!(page.saved.paused);
        assert!(page
            .saved
            .note
            .as_deref()
            .unwrap()
            .contains("newer version"));
        page.saved.removed_apps.push("clipchamp");
        page.save_undo();
        assert_eq!(
            std::fs::read(&file).unwrap(),
            newer,
            "a newer list was replaced"
        );

        // "Delete saved undo" removes it and saving works again.
        page.delete_saved_undo();
        assert!(!file.exists());
        assert!(page.saved.removed_apps.is_empty() && !page.saved.paused);
        page.saved.removed_apps.push("clipchamp");
        page.save_undo();
        assert!(file.exists());

        // A new process, before its first draw: a save reads the file first, never replaces it.
        let mut again = page_with_store(&dir);
        let task = tasks::catalog().first().unwrap();
        again
            .tasks
            .steps
            .push(saved_undo::task_done(tasks::UndoStep {
                id: task.id,
                path: task.path,
                was_enabled: true,
            }));
        again.save_undo();
        let mut third = page_with_store(&dir);
        third.load_saved_undo();
        assert_eq!(third.saved.removed_apps, vec!["clipchamp"]);
        assert_eq!(third.tasks.steps.len(), 1);
        assert!(third.saved.note.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_list_that_stayed_in_place_is_never_overwritten() {
        let dir = std::env::temp_dir().join(format!(
            "toolboks-debloater-page-damaged-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("undo.jsonl");
        std::fs::write(&file, b"damaged").unwrap();

        let mut page = page_with_store(&dir);
        page.use_loaded(saved_undo::Loaded::Damaged {
            reason: "its header cannot be read".into(),
            kept: false,
        });
        assert!(page.saved.paused);
        assert!(page
            .saved
            .note
            .as_deref()
            .unwrap()
            .contains("could not be moved aside"));
        page.saved.removed_apps.push("clipchamp");
        page.save_undo();
        assert_eq!(std::fs::read(&file).unwrap(), b"damaged");

        // Moved aside: saving goes on.
        let mut moved = page_with_store(&dir);
        moved.use_loaded(saved_undo::Loaded::Damaged {
            reason: "its header cannot be read".into(),
            kept: true,
        });
        assert!(!moved.saved.paused);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plural_and_edition_labels() {
        assert_eq!(plural(1, "change", "changes"), "1 change");
        assert_eq!(plural(3, "change", "changes"), "3 changes");
        assert_eq!(edition_label(Edition::Pro), "Windows Pro");
        assert_eq!(edition_label(Edition::Unknown), "Windows");
    }
}
