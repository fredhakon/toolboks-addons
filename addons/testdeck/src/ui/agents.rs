//! Agents: one row per requester seen running, queued or in the last 24 hours of history: what it holds or waits
//! for, and its last result. A click on a name opens History filtered by it.
use super::{fmt_dur, job_bar, theme, Snapshot};
use crate::history::{kind_str, JobRow};
use crate::progress::JobProgress;
use crate::scheduler::Ticket;
use egui::{RichText, Ui};
use std::collections::HashMap;
use theme::{GREEN, ORANGE, TEXT2, TEXT3};

const WINDOW_SECS: f64 = 24.0 * 3600.0;

/// What history says about one requester over the last 24 hours (rows arrive newest first).
struct Last {
    kind: String,
    ended_unix: f64,
    exit_code: Option<i32>,
    end_reason: String,
    passed: Option<u32>,
    failed: Option<u32>,
    runs: u32,
    bad_runs: u32,
}

impl Last {
    fn is_bad(&self) -> bool {
        self.failed.is_some_and(|f| f > 0)
            || self.exit_code.is_some_and(|c| c != 0)
            || self.end_reason != "released"
    }
}

#[derive(Default)]
pub(crate) struct State {
    last: HashMap<String, Last>,
}

impl State {
    pub(crate) fn refresh(&mut self, rows: &[JobRow], now_unix: f64) {
        self.last.clear();
        for r in rows
            .iter()
            .filter(|r| now_unix - r.ended_unix <= WINDOW_SECS)
        {
            let bad = r.failed.is_some_and(|f| f > 0)
                || r.exit_code.is_some_and(|c| c != 0)
                || r.end_reason != "released";
            let e = self
                .last
                .entry(r.requester.clone())
                .or_insert_with(|| Last {
                    kind: r.kind.clone(),
                    ended_unix: r.ended_unix,
                    exit_code: r.exit_code,
                    end_reason: r.end_reason.clone(),
                    passed: r.passed,
                    failed: r.failed,
                    runs: 0,
                    bad_runs: 0,
                });
            e.runs += 1;
            e.bad_runs += u32::from(bad);
        }
    }
}

/// Returns the requester whose name was clicked.
pub(crate) fn show(
    ui: &mut Ui,
    st: &mut State,
    snap: &Snapshot,
    progress: &HashMap<Ticket, JobProgress>,
) -> Option<String> {
    let mut names: Vec<&str> = st.last.keys().map(String::as_str).collect();
    for n in snap
        .running
        .iter()
        .map(|r| r.requester.as_str())
        .chain(snap.queue.iter().map(|q| q.requester.as_str()))
    {
        if !names.contains(&n) {
            names.push(n);
        }
    }
    let busy = |n: &str| {
        (
            snap.running.iter().any(|r| r.requester == n),
            snap.queue.iter().any(|q| q.requester == n),
        )
    };
    names.sort_by(|a, b| {
        let key = |n: &str| {
            (
                !busy(n).0,
                !busy(n).1,
                std::cmp::Reverse(st.last.get(n).map_or(0, |l| l.ended_unix as i64)),
            )
        };
        key(a).cmp(&key(b)).then(a.cmp(b))
    });

    theme::title(ui, "Agents");
    ui.label(
        RichText::new(
            "Every requester seen in the last 24 hours. Click a name to see its jobs in History.",
        )
        .size(12.0)
        .color(TEXT3),
    );
    ui.add_space(8.0);
    if names.is_empty() {
        ui.label(RichText::new("No agent has asked for a slot yet.").color(TEXT3));
        return None;
    }
    let mut clicked = None;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            egui::Grid::new("agents")
                .num_columns(4)
                .spacing([18.0, 0.0])
                .min_row_height(30.0)
                .show(ui, |ui| {
                    for h in ["Agent", "Holds or waits for", "Last result", "Last 24 h"] {
                        ui.label(RichText::new(h).size(12.0).color(TEXT3));
                    }
                    ui.end_row();
                    for name in names {
                        if ui
                            .add(egui::Button::new(theme::strong(name)).frame(false))
                            .on_hover_text("Show in History")
                            .clicked()
                        {
                            clicked = Some(name.to_string());
                        }
                        ui.horizontal(|ui| now_cell(ui, name, snap, progress));
                        ui.horizontal(|ui| last_cell(ui, st.last.get(name), snap.now_unix));
                        ui.horizontal(|ui| match st.last.get(name) {
                            Some(l) => {
                                let text = if l.bad_runs > 0 {
                                    format!("{} runs, {} failed", l.runs, l.bad_runs)
                                } else {
                                    format!("{} runs", l.runs)
                                };
                                ui.label(theme::mono(text).color(if l.bad_runs > 0 {
                                    ORANGE
                                } else {
                                    TEXT2
                                }));
                            }
                            None => {
                                ui.label(RichText::new("-").color(TEXT3));
                            }
                        });
                        ui.end_row();
                    }
                });
        });
    clicked
}

fn now_cell(ui: &mut Ui, name: &str, snap: &Snapshot, progress: &HashMap<Ticket, JobProgress>) {
    ui.spacing_mut().item_spacing.x = 14.0;
    {
        let mut any = false;
        for r in snap.running.iter().filter(|r| r.requester == name) {
            any = true;
            let kind = kind_str(r.kind);
            let bar = job_bar(r, progress.get(&r.ticket));
            ui.label(
                RichText::new(format!(
                    "holds a slot: {kind} {} \u{B7} {}",
                    fmt_dur(r.elapsed),
                    bar.short
                ))
                .size(12.0)
                .color(theme::kind_color(kind)),
            );
        }
        for (i, q) in snap
            .queue
            .iter()
            .enumerate()
            .filter(|(_, q)| q.requester == name)
        {
            any = true;
            let wait = q
                .held
                .clone()
                .or_else(|| q.eta.map(|e| format!("starts in ~{}", fmt_dur(e))))
                .unwrap_or_else(|| format!("waited {}", fmt_dur(q.waited)));
            ui.label(
                RichText::new(format!(
                    "waits: {} #{} \u{B7} {wait}",
                    kind_str(q.kind),
                    i + 1
                ))
                .size(12.0)
                .color(TEXT2),
            );
        }
        if !any {
            ui.label(RichText::new("idle").size(12.0).color(TEXT3));
        }
    }
}

fn last_cell(ui: &mut Ui, last: Option<&Last>, now_unix: f64) {
    let Some(l) = last else {
        ui.label(RichText::new("no run in 24 h").size(12.0).color(TEXT3));
        return;
    };
    let result = if l.end_reason != "released" {
        l.end_reason.clone()
    } else {
        l.exit_code
            .map_or("exit unknown".to_string(), |c| format!("exit {c}"))
    };
    let counts = match (l.passed, l.failed) {
        (Some(p), Some(f)) => format!(" \u{B7} {p} passed, {f} failed"),
        _ => String::new(),
    };
    let text = format!(
        "{} {result}{counts} \u{B7} {}",
        l.kind,
        ago(now_unix - l.ended_unix)
    );
    ui.label(theme::mono(text).color(if l.is_bad() { ORANGE } else { GREEN }));
}

fn ago(secs: f64) -> String {
    if secs < 60.0 {
        "just now".into()
    } else if secs < 3600.0 {
        format!("{}m ago", (secs / 60.0) as u64)
    } else {
        format!("{}h ago", (secs / 3600.0) as u64)
    }
}
