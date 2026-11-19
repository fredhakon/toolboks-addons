//! AI setup page (PROMPT.md, Phase 3 item 3): health of the gate scripts and the Claude hook, install/repair with
//! backups, rollback of the last install, and the copy-ready instruction text for agents.
use crate::integration::{self, Dirs, Health, Level};
use crate::state::Shared;
use egui::RichText;
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

/// One source for the page and the docs.
const SNIPPET: &str = include_str!("../../docs/AGENTS-SNIPPET.md");

const HOW: &str = "Agents never talk to Testdeck directly. They run tests and builds through the bash scripts in \
~/.claude/gate (jt.sh, heavy.sh, gate.sh); slots.sh asks this app for a slot over 127.0.0.1 and waits in its queue, \
and falls back to the old file queue when the app is not running, so nothing hangs. Claude Code also has a \
PreToolUse hook: it moves a raw heavy command into heavy.sh and asks Testdeck first whether the command can run now \
(foreground, 10-minute timeout) or will queue (background, the agent is notified when it ends). Codex, Gemini and \
other agents follow the instruction text below.";

#[derive(Default)]
pub(crate) struct State {
    health: Option<Health>,
    pending: Option<Receiver<Health>>,
    /// The install plan awaiting confirmation.
    confirm: Option<Vec<String>>,
    messages: Vec<String>,
}

pub(crate) fn show(ui: &mut egui::Ui, st: &mut State, shared: &Shared) {
    if let Some(rx) = &st.pending {
        match rx.try_recv() {
            Ok(h) => {
                st.health = Some(h);
                st.pending = None;
            }
            Err(_) => ui.ctx().request_repaint_after(Duration::from_millis(200)),
        }
    }
    if st.health.is_none() && st.pending.is_none() {
        refresh(st, shared);
    }
    let dirs = Dirs::from_env(shared.data_dir());

    egui::ScrollArea::vertical().show(ui, |ui| {
        ui.heading("AI setup");
        ui.label(format!("Gate folder {}   Claude folder {}", dirs.gate.display(), dirs.claude.display()));
        ui.add_space(6.0);
        if let Some(h) = &st.health {
            egui::Grid::new("ai_health").num_columns(3).spacing([12.0, 4.0]).show(ui, |ui| {
                for r in &h.rows {
                    let color = match r.level {
                        Level::Ok => super::theme::GREEN,
                        Level::Warn => super::theme::AMBER,
                        Level::Bad => super::theme::RED,
                    };
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                    ui.painter().circle_filled(rect.center(), 5.0, color);
                    ui.label(RichText::new(&r.label).strong());
                    ui.label(&r.reason);
                    ui.end_row();
                }
            });
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let busy = st.pending.is_some();
            if ui.add_enabled(!busy, egui::Button::new("Check again")).clicked() {
                refresh(st, shared);
            }
            let plan = st.health.as_ref().map(|h| h.plan.clone()).unwrap_or_default();
            let label = if plan.is_empty() { "Install / repair (nothing to do)" } else { "Install / repair…" };
            if ui.add_enabled(!busy && !plan.is_empty(), egui::Button::new(label)).clicked() {
                st.confirm = Some(plan);
            }
            if ui.add_enabled(!busy, egui::Button::new("Roll back last install")).clicked() {
                st.messages = integration::rollback(&dirs).unwrap_or_else(|e| vec![e]);
                refresh(st, shared);
            }
            if busy {
                ui.spinner();
            }
        });
        // Inline, not a floating window: the add-on draws only inside its page.
        if let Some(plan) = st.confirm.clone() {
            super::theme::card().show(ui, |ui| {
                super::theme::title(ui, "Install / repair");
                ui.label("These files change. Every replaced file is backed up first; Roll back undoes this install.");
                for p in &plan {
                    ui.label(RichText::new(format!("- {p}")).monospace());
                }
                ui.horizontal(|ui| {
                    if ui.button("Install").clicked() {
                        st.messages = match integration::install(&dirs) {
                            Ok(lines) if lines.is_empty() => vec!["nothing changed".into()],
                            Ok(lines) => lines,
                            Err(e) => vec![format!("install failed: {e}")],
                        };
                        st.confirm = None;
                        refresh(st, shared);
                    }
                    if ui.button("Cancel").clicked() {
                        st.confirm = None;
                    }
                });
            });
        }
        for m in &st.messages {
            ui.label(RichText::new(m).monospace().small());
        }

        ui.add_space(12.0);
        ui.heading("How agents talk to Testdeck");
        ui.label(HOW);

        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.heading("Instructions for CLAUDE.md / AGENTS.md");
            if ui.button("Copy").clicked() {
                ui.ctx().copy_text(SNIPPET.into());
            }
        });
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(RichText::new(SNIPPET).monospace());
        });
    });
}

/// Runs the health check off the UI thread (it starts bash for the fallback check).
fn refresh(st: &mut State, shared: &Shared) {
    let port = shared.lock().config.port;
    let dirs = Dirs::from_env(shared.data_dir());
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let _ = tx.send(integration::check(&dirs, port));
    });
    st.pending = Some(rx);
}
