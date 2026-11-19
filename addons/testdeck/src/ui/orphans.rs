//! Orphans tab: node (jest, tsc, next) and tsc processes with no ticket and no parent. Scans run off the UI
//! thread, on demand and every 30 s while the tab is open. Nothing is killed without a click.
use super::{command_label, fmt_dur};
use crate::orphans::{self as finder, Orphan};
use crate::state::Shared;
use egui::RichText;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const RESCAN: Duration = Duration::from_secs(30);

#[derive(Default)]
struct Inner {
    rows: Vec<Orphan>,
    scanned_at: Option<Instant>,
    busy: bool,
    note: String,
}

#[derive(Default)]
pub(crate) struct State {
    inner: Arc<Mutex<Inner>>,
}

impl State {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Kills `kill` (when given) and rescans, on a background thread; ignored while one runs.
    fn spawn(&self, shared: &Shared, kill: Option<Orphan>) {
        {
            let mut i = self.lock();
            if i.busy {
                return;
            }
            i.busy = true;
        }
        let (inner, shared) = (Arc::clone(&self.inner), shared.clone());
        std::thread::spawn(move || {
            let note = kill.map(|o| {
                if finder::kill(&shared, o.pid, o.start_unix) {
                    format!("Killed pid {} and its child processes.", o.pid)
                } else {
                    format!(
                        "pid {} is gone or no longer an orphan; nothing was killed.",
                        o.pid
                    )
                }
            });
            let rows = finder::scan(&shared);
            let mut i = inner.lock().unwrap_or_else(|e| e.into_inner());
            i.rows = rows;
            i.scanned_at = Some(Instant::now());
            i.busy = false;
            if let Some(n) = note {
                i.note = n;
            }
        });
    }
}

pub(crate) fn show(ui: &mut egui::Ui, st: &mut State, shared: &Shared) {
    let due = {
        let i = st.lock();
        !i.busy && i.scanned_at.is_none_or(|t| t.elapsed() >= RESCAN)
    };
    if due {
        st.spawn(shared, None);
    }
    let (rows, busy, scanned_at, note) = {
        let i = st.lock();
        (i.rows.clone(), i.busy, i.scanned_at, i.note.clone())
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new("Orphans").heading());
        ui.weak(rows.len().to_string());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add_enabled(!busy, egui::Button::new("Scan now"))
                .clicked()
            {
                st.spawn(shared, None);
            }
            if busy {
                ui.spinner();
            } else if let Some(t) = scanned_at {
                ui.weak(format!(
                    "scanned {} ago",
                    fmt_dur(t.elapsed().as_secs_f64())
                ));
            }
        });
    });
    ui.weak(
        "node processes running jest, tsc or next, and tsc.exe, that belong to no running job and whose parent \
         process is gone. Testdeck never kills them by itself.",
    );
    if !note.is_empty() {
        ui.label(&note);
    }
    ui.add_space(6.0);
    if rows.is_empty() {
        ui.weak(if scanned_at.is_some() {
            "No orphans found."
        } else {
            "Scanning..."
        });
        return;
    }
    let mut kill = None;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            egui::Grid::new("orphans")
                .striped(true)
                .num_columns(4)
                .spacing([14.0, 6.0])
                .show(ui, |ui| {
                    ui.strong("PID");
                    ui.strong("Age");
                    ui.strong("Command line");
                    ui.label("");
                    ui.end_row();
                    for o in &rows {
                        ui.monospace(o.pid.to_string());
                        ui.label(fmt_dur(o.age_secs as f64));
                        command_label(ui, if o.cmd.is_empty() { &o.name } else { &o.cmd }, 600.0);
                        let text = RichText::new("Kill").color(ui.visuals().error_fg_color);
                        if ui
                            .add_enabled(!busy, egui::Button::new(text))
                            .on_hover_text("Ends this process and its children")
                            .clicked()
                        {
                            kill = Some(o.clone());
                        }
                        ui.end_row();
                    }
                });
        });
    if kill.is_some() {
        st.spawn(shared, kill);
    }
}
