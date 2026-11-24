//! Disk page: the folders jest and `jt.sh` leave behind (see `folders.rs`), their sizes, and a Clean up button that
//! asks Confirm / Cancel first. Measuring and deleting run off the UI thread; nothing is deleted without a click
//! (the optional automatic cleanup is a config flag, off by default).
use super::{fmt_dur, theme};
use crate::folders::{self, Folder, Kind, GB};
use crate::history::History;
use crate::state::Shared;
use egui::RichText;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const RESCAN: Duration = Duration::from_secs(120);

#[derive(Clone)]
struct Row {
    f: Folder,
    /// Why a cleanup must keep it right now, if it must.
    keep: Option<&'static str>,
    /// A cleanup removes it without a tick.
    auto: bool,
}

#[derive(Default)]
struct Inner {
    rows: Vec<Row>,
    jobs: Vec<folders::JobPath>,
    scanned_at: Option<Instant>,
    busy: bool,
    note: String,
}

#[derive(Default)]
pub(crate) struct State {
    inner: Arc<Mutex<Inner>>,
    ticked: HashSet<PathBuf>,
    /// The folders the Confirm / Cancel list asks about.
    confirm: Option<Vec<Folder>>,
}

pub(crate) fn fmt_bytes(b: u64) -> String {
    let (gb, mb) = (b as f64 / GB as f64, b as f64 / (1u64 << 20) as f64);
    if gb >= 1.0 {
        format!("{gb:.2} GB")
    } else if mb >= 1.0 {
        format!("{mb:.1} MB")
    } else {
        format!("{} KB", b.div_ceil(1024))
    }
}

fn lock(inner: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    inner.lock().unwrap_or_else(|e| e.into_inner())
}

/// Deletes `todo` (when given), then measures everything again, on a background thread. Ignored while one runs.
fn spawn(st: &State, shared: &Shared, history: &Arc<History>, todo: Option<Vec<Folder>>) {
    {
        let mut i = lock(&st.inner);
        if i.busy {
            return;
        }
        i.busy = true;
    }
    let (inner, shared, history) = (Arc::clone(&st.inner), shared.clone(), Arc::clone(history));
    std::thread::spawn(move || {
        let jobs = history.job_paths();
        let note = todo.map(|todo| {
            let out = folders::with_ctx(&shared, &jobs, |_, ctx| {
                folders::clean(&todo.iter().collect::<Vec<_>>(), ctx)
            });
            let kept = out.failed.first().map_or(String::new(), |(_, why)| {
                format!("; {} kept or failed ({why})", out.failed.len())
            });
            format!(
                "Freed {} from {} folder(s){kept}.",
                fmt_bytes(out.freed),
                out.removed.len()
            )
        });
        let found = folders::scan(&shared, &jobs);
        let rows = folders::with_ctx(&shared, &jobs, |_, ctx| {
            found
                .into_iter()
                .map(|f| Row {
                    keep: folders::guard(&f, ctx).err(),
                    auto: folders::auto_safe(&f, ctx),
                    f,
                })
                .collect()
        });
        let mut i = lock(&inner);
        (i.rows, i.jobs, i.scanned_at, i.busy) = (rows, jobs, Some(Instant::now()), false);
        if let Some(n) = note {
            i.note = n;
        }
    });
}

pub(crate) fn show(
    ui: &mut egui::Ui,
    st: &mut State,
    shared: &Shared,
    history: &Arc<History>,
    limit_gb: f32,
) {
    let due = {
        let i = lock(&st.inner);
        !i.busy && i.scanned_at.is_none_or(|t| t.elapsed() >= RESCAN)
    };
    if due {
        spawn(st, shared, history, None);
    }
    let (rows, jobs, busy, scanned_at, note) = {
        let i = lock(&st.inner);
        (
            i.rows.clone(),
            i.jobs.clone(),
            i.busy,
            i.scanned_at,
            i.note.clone(),
        )
    };
    let total: u64 = rows.iter().map(|r| r.f.size).sum();
    let partial = rows.iter().any(|r| r.f.partial);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    st.ticked.retain(|p| rows.iter().any(|r| &r.f.path == p));

    ui.horizontal(|ui| {
        ui.label(RichText::new("Disk").heading());
        ui.weak(format!(
            "{} jest folders, {}{}",
            rows.len(),
            fmt_bytes(total),
            if partial { " or more" } else { "" }
        ));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let can = !busy && st.confirm.is_none();
            if ui.add_enabled(can, egui::Button::new("Refresh")).clicked() {
                spawn(st, shared, history, None);
            }
            if ui.add_enabled(can, egui::Button::new("Clean up")).clicked() {
                let all: Vec<Folder> = rows.iter().map(|r| r.f.clone()).collect();
                let picked = folders::with_ctx(shared, &jobs, |_, ctx| {
                    folders::plan(&all, &st.ticked, ctx)
                        .into_iter()
                        .cloned()
                        .collect::<Vec<_>>()
                });
                if picked.is_empty() {
                    lock(&st.inner).note =
                        "Nothing to clean up: every folder is in use or too new.".into();
                } else {
                    st.confirm = Some(picked);
                }
            }
            if busy {
                ui.spinner();
            } else if let Some(t) = scanned_at {
                ui.weak(format!(
                    "measured {} ago",
                    fmt_dur(t.elapsed().as_secs_f64())
                ));
            }
        });
    });
    ui.weak(
        "jt-* run folders and jest caches in %TEMP%, and the jest caches of the projects the history knows. A cleanup \
         removes finished jt-* folders over an hour old and caches no running or queued job uses, plus the folders you \
         tick. Junctions are removed, never followed.",
    );
    if total > (f64::from(limit_gb.max(0.0)) * GB as f64) as u64 {
        ui.label(
            RichText::new(format!(
                "Jest folders use {}, above the {limit_gb:.0} GB limit (Settings).",
                fmt_bytes(total)
            ))
            .color(theme::AMBER),
        );
    }
    if !note.is_empty() {
        ui.label(&note);
    }
    confirm_box(ui, st, shared, history);
    ui.add_space(6.0);
    if rows.is_empty() {
        ui.weak(if scanned_at.is_some() {
            "No jest folders found."
        } else {
            "Measuring..."
        });
        return;
    }
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            egui::Grid::new("disk")
                .striped(true)
                .num_columns(7)
                .spacing([14.0, 6.0])
                .show(ui, |ui| {
                    for h in [
                        "",
                        "Folder",
                        "Size",
                        "Files",
                        "Last write",
                        "Belongs to",
                        "Cleanup",
                    ] {
                        ui.strong(h);
                    }
                    ui.end_row();
                    for r in &rows {
                        row(ui, st, r, now);
                    }
                });
        });
}

fn row(ui: &mut egui::Ui, st: &mut State, r: &Row, now: u64) {
    let f = &r.f;
    let mut on = st.ticked.contains(&f.path);
    let tickable = r.keep.is_none() && !r.auto;
    if ui
        .add_enabled(tickable, egui::Checkbox::without_text(&mut on))
        .changed()
    {
        if on {
            st.ticked.insert(f.path.clone());
        } else {
            st.ticked.remove(&f.path);
        }
    }
    super::command_label(ui, &f.path.to_string_lossy(), 380.0);
    ui.label(if f.link {
        "link".to_string()
    } else {
        format!("{}{}", fmt_bytes(f.size), if f.partial { "+" } else { "" })
    });
    ui.label(f.files.to_string());
    ui.label(if f.newest_unix == 0 {
        "?".to_string()
    } else {
        format!("{} ago", fmt_dur(now.saturating_sub(f.newest_unix) as f64))
    });
    super::command_label(ui, &f.owner, 260.0);
    let kind = if f.kind == Kind::Slot {
        "run folder"
    } else {
        "cache"
    };
    match (r.keep, r.auto) {
        (Some(why), _) => ui.label(RichText::new(format!("keep: {why}")).color(theme::AMBER)),
        (None, true) => ui.label(format!("removed ({kind})")),
        (None, false) => ui.weak(format!("only if ticked ({kind})")),
    };
    ui.end_row();
}

/// The Confirm / Cancel list with the sizes; Confirm deletes on a background thread.
fn confirm_box(ui: &mut egui::Ui, st: &mut State, shared: &Shared, history: &Arc<History>) {
    let Some(list) = &st.confirm else { return };
    let mut decision = None;
    theme::card().show(ui, |ui| {
        let total: u64 = list.iter().map(|f| f.size).sum();
        ui.label(
            RichText::new(format!(
                "Delete {} folder(s), about {}?",
                list.len(),
                fmt_bytes(total)
            ))
            .strong(),
        );
        egui::ScrollArea::vertical()
            .id_salt("disk-confirm")
            .max_height(160.0)
            .show(ui, |ui| {
                for f in list {
                    ui.monospace(format!("{}  {}", fmt_bytes(f.size), f.path.display()));
                }
            });
        ui.horizontal(|ui| {
            if ui.button("Confirm").clicked() {
                decision = Some(true);
            }
            if ui.button("Cancel").clicked() {
                decision = Some(false);
            }
        });
    });
    match decision {
        Some(true) => {
            let todo = st.confirm.take();
            st.ticked.clear();
            spawn(st, shared, history, todo);
        }
        Some(false) => st.confirm = None,
        None => {}
    }
}
