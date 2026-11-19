//! Running jobs: one card each with requester, kind, command, elapsed, workers, a progress bar for every kind of job,
//! failures as they appear and a confirmed Kill.
use super::{command_label, fmt_dur, job_bar, short_path, theme, RunRow, Snapshot};
use crate::history::kind_str;
use crate::progress::JobProgress;
use crate::scheduler::Ticket;
use crate::state::Shared;
use egui::{Align, Layout, RichText, Ui};
use std::collections::HashMap;
use theme::{AMBER, ORANGE, TEXT2, TEXT3};

pub(crate) fn show(
    ui: &mut Ui,
    snap: &Snapshot,
    progress: &HashMap<Ticket, JobProgress>,
    kill_pending: &mut Option<Ticket>,
    shared: &Shared,
) {
    if snap.running.is_empty() {
        ui.label(RichText::new("Nothing is running.").color(TEXT3));
        return;
    }
    for pair in snap.running.chunks(2) {
        ui.columns(2, |cols| {
            for (col, r) in cols.iter_mut().zip(pair) {
                card(col, r, progress.get(&r.ticket), kill_pending, shared);
            }
        });
        ui.add_space(-4.0);
    }
}

fn card(
    ui: &mut Ui,
    r: &RunRow,
    p: Option<&JobProgress>,
    kill_pending: &mut Option<Ticket>,
    shared: &Shared,
) {
    let kind = kind_str(r.kind);
    let color = theme::kind_color(kind);
    let bar = job_bar(r, p);
    theme::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 10.0;
        ui.horizontal(|ui| {
            ui.label(theme::strong(&r.requester).size(14.0));
            ui.label(RichText::new(kind).size(12.0).color(color));
            if r.stalled {
                ui.label(RichText::new("stalled").size(12.0).color(AMBER))
                    .on_hover_text(
                        "No new PASS/FAIL line in the jest log for the stall time (Settings)",
                    );
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let mut w = format!(
                    "{} worker{}",
                    r.workers,
                    if r.workers == 1 { "" } else { "s" }
                );
                if r.cpu_mask != 0 {
                    // The worker count is fixed when the job starts; the cores shrink and grow as other jobs come and go.
                    w.push_str(&format!(
                        " \u{B7} {} cores ({})",
                        r.cpu_mask.count_ones(),
                        core_ranges(r.cpu_mask)
                    ));
                }
                ui.label(theme::mono(format!("{} \u{B7} {}", fmt_dur(r.elapsed), w)).color(TEXT2));
            });
        });
        command_label(ui, &r.command, ui.available_width());
        match bar.frac {
            Some(f) => {
                theme::bar(ui, Some(f), color, 6.0);
            }
            None => {
                ui.horizontal(|ui| {
                    theme::spinner(ui, ui.style().spacing.interact_size.y);
                    ui.label(theme::mono(&bar.detail));
                    ui.label(
                        RichText::new("no suite count or history average yet")
                            .size(11.0)
                            .color(TEXT3),
                    );
                });
            }
        }
        ui.horizontal(|ui| {
            if bar.frac.is_some() {
                ui.label(theme::mono(&bar.detail));
            }
            if let Some(p) = p.filter(|p| !p.failing.is_empty()) {
                failures(ui, p);
            }
        });
        ui.horizontal(|ui| {
            if let Some(l) = r.load {
                ui.label(
                    theme::mono(format!("CPU {:.0}%", l.cpu))
                        .size(11.0)
                        .color(TEXT3),
                );
                ui.label(
                    theme::mono(format!("RAM {}", fmt_mb(l.ram_mb)))
                        .size(11.0)
                        .color(TEXT3),
                );
            }
            if r.below_normal {
                ui.label(RichText::new("low priority").size(11.0).color(TEXT3));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                kill(ui, r, kill_pending, shared)
            });
        });
    });
}

/// The first failing suite by name; the rest on hover.
fn failures(ui: &mut Ui, p: &JobProgress) {
    let first = short_path(&p.failing[0]);
    let more = if p.failing.len() > 1 {
        format!(" +{} more", p.failing.len() - 1)
    } else {
        String::new()
    };
    let text = format!("{} failed: {first}{more}", p.failing.len());
    ui.add(egui::Label::new(RichText::new(text).size(12.0).color(ORANGE)).truncate())
        .on_hover_ui(|ui| {
            for f in p.failing.iter().take(15) {
                ui.label(theme::mono(f).color(ORANGE));
            }
            if p.failing.len() > 15 {
                ui.label(RichText::new(format!("and {} more", p.failing.len() - 15)).color(TEXT3));
            }
        });
}

fn kill(ui: &mut Ui, r: &RunRow, kill_pending: &mut Option<Ticket>, shared: &Shared) {
    if *kill_pending == Some(r.ticket) {
        let keep = theme::small_button(ui, "Keep");
        if theme::clicked(ui, &keep, ("keep", r.ticket)) {
            *kill_pending = None;
        }
        let yes = theme::small_button(ui, RichText::new("Kill").color(ORANGE));
        if theme::clicked(ui, &yes, ("kill-yes", r.ticket)) {
            shared.kill(r.ticket);
            *kill_pending = None;
        }
        ui.label(
            RichText::new("Kill this job and all its processes?")
                .size(12.0)
                .color(TEXT2),
        );
    } else if r.stalled {
        // A stalled job gets a one-click kill: the guard already asked by toast.
        let b = theme::small_button(ui, RichText::new("Kill stalled").color(ORANGE))
            .on_hover_text("Ends the whole process tree now");
        if theme::clicked(ui, &b, ("kill-stalled", r.ticket)) {
            shared.kill(r.ticket);
        }
    } else {
        let b = theme::small_button(ui, "Kill").on_hover_text("Ends the whole process tree");
        if theme::clicked(ui, &b, ("kill", r.ticket)) {
            *kill_pending = Some(r.ticket);
        }
    }
}

/// "16-31" or "2-9,12": the set bits of an affinity mask as ranges.
fn core_ranges(mask: u64) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut k = 0;
    while k < 64 {
        if mask >> k & 1 == 0 {
            k += 1;
            continue;
        }
        let start = k;
        while k < 64 && mask >> k & 1 == 1 {
            k += 1;
        }
        out.push(if k - 1 == start {
            start.to_string()
        } else {
            format!("{start}-{}", k - 1)
        });
    }
    out.join(",")
}

fn fmt_mb(mb: f32) -> String {
    if mb >= 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else {
        format!("{mb:.0} MB")
    }
}
