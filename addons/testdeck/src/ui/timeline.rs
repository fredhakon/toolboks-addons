//! The slot timeline: one lane per slot, the last 30 minutes as muted blocks, running jobs as coloured blocks with a
//! progress fill, a "now" line, and the queued jobs as dashed blocks where their ETA puts them.
use super::{job_bar, theme, QueueRow, RunRow, Snapshot};
use crate::history::{kind_str, JobRow};
use crate::progress::JobProgress;
use crate::scheduler::Ticket;
use egui::{Align2, Color32, FontFamily, FontId, Id, Rect, Sense, Shape, Stroke, Ui};
use std::collections::HashMap;
use theme::{FAILED_BLOCK, INK, PANEL, TEXT, TEXT2, TEXT3, TRACK};

const PAST: f64 = 30.0 * 60.0;
const FUTURE: f64 = 15.0 * 60.0;
const LABEL_W: f32 = 64.0;
const LANE_H: f32 = 34.0;
const MAX_LANES: usize = 8;
/// Used for a planned block when history has no average for the command.
const GUESS_SECS: f64 = 120.0;

enum Block<'a> {
    Done(&'a JobRow),
    Running(&'a RunRow),
    Planned(&'a QueueRow),
}

struct Item<'a> {
    /// Seconds relative to now (negative = past).
    start: f64,
    end: f64,
    block: Block<'a>,
}

pub(crate) fn show(
    ui: &mut Ui,
    snap: &Snapshot,
    progress: &HashMap<Ticket, JobProgress>,
    history: &[JobRow],
) {
    let items = items(snap, history);
    let lanes = lane_assignment(&items, (snap.config.slots as usize).clamp(1, MAX_LANES));
    let lane_count = (snap.config.slots as usize)
        .max(lanes.iter().copied().max().map_or(0, |m| m + 1))
        .clamp(1, MAX_LANES);

    ui.horizontal(|ui| {
        theme::title(ui, "Slots");
        ui.label(
            egui::RichText::new("last 30 min, then the plan")
                .size(12.0)
                .color(TEXT3),
        );
    });
    ui.add_space(-6.0);
    let width = ui.available_width();
    let height = 20.0 + lane_count as f32 * LANE_H + (lane_count as f32 - 1.0) * 8.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), Sense::hover());
    let track_x = rect.left() + LABEL_W;
    let track_w = (rect.width() - LABEL_W).max(10.0);
    let x_of = |t: f64| track_x + ((t + PAST) / (PAST + FUTURE)) as f32 * track_w;
    let p = ui.painter();
    let mono = FontId::new(11.0, FontFamily::Monospace);

    // Axis: -30m at the left edge, "now" at the line, +15m at the right edge.
    let axis_y = rect.top() + 6.0;
    p.text(
        egui::pos2(track_x, axis_y),
        Align2::LEFT_CENTER,
        "-30m",
        mono.clone(),
        TEXT3,
    );
    p.text(
        egui::pos2(x_of(0.0), axis_y),
        Align2::CENTER_CENTER,
        "now",
        mono.clone(),
        TEXT2,
    );
    p.text(
        egui::pos2(track_x + track_w, axis_y),
        Align2::RIGHT_CENTER,
        "+15m",
        mono,
        TEXT3,
    );

    let lane_rect = |i: usize| {
        let top = rect.top() + 20.0 + i as f32 * (LANE_H + 8.0);
        Rect::from_min_size(egui::pos2(track_x, top), egui::vec2(track_w, LANE_H))
    };
    for i in 0..lane_count {
        let r = lane_rect(i);
        p.text(
            egui::pos2(rect.left(), r.center().y),
            Align2::LEFT_CENTER,
            format!("Slot {}", i + 1),
            FontId::new(12.0, FontFamily::Proportional),
            TEXT2,
        );
        p.rect_filled(r, 6.0, PANEL);
    }

    for (n, (item, lane)) in items.iter().zip(&lanes).enumerate() {
        let lane = (*lane).min(lane_count - 1);
        let l = lane_rect(lane);
        let x0 = x_of(item.start.max(-PAST)).max(l.left());
        let x1 = x_of(item.end.min(FUTURE)).min(l.right());
        if x1 <= x0 {
            continue;
        }
        let b = Rect::from_min_max(
            egui::pos2(x0, l.top() + 4.0),
            egui::pos2(x1.max(x0 + 3.0), l.bottom() - 4.0),
        );
        match &item.block {
            Block::Done(j) => {
                let failed = j.failed.is_some_and(|f| f > 0)
                    || j.exit_code.is_some_and(|c| c != 0)
                    || j.end_reason != "released";
                p.rect_filled(b, 4.0, if failed { FAILED_BLOCK } else { TRACK });
                let result = if failed { "failed" } else { "ok" };
                let tip = format!(
                    "{} \u{B7} {} \u{B7} {} \u{B7} {}",
                    j.requester,
                    j.kind,
                    super::fmt_dur(j.duration_secs),
                    result
                );
                ui.interact(b, Id::NULL.with(("slot-done", n)), Sense::hover())
                    .on_hover_text(tip);
            }
            Block::Running(r) => {
                let color = theme::kind_color(kind_str(r.kind));
                let bar = job_bar(r, progress.get(&r.ticket));
                p.rect_filled(b, 4.0, theme::dim(color));
                let mut fill = b;
                if let Some(f) = bar.frac {
                    fill.set_width(b.width() * f);
                    p.rect_filled(fill, 4.0, color);
                }
                label(
                    p,
                    b,
                    fill,
                    &format!("{} \u{B7} {}", r.requester, kind_str(r.kind)),
                    color,
                );
                ui.interact(b, Id::NULL.with(("slot-run", n)), Sense::hover())
                    .on_hover_text(format!("{} \u{B7} {}", r.command, bar.detail));
            }
            Block::Planned(q) => {
                for s in Shape::dashed_line(
                    &closed(b),
                    Stroke::new(1.0_f32, Color32::from_rgb(0x4A, 0x4C, 0x52)),
                    3.0,
                    3.0,
                ) {
                    p.add(s);
                }
                p.with_clip_rect(b.shrink2(egui::vec2(2.0, 0.0))).text(
                    egui::pos2(b.left() + 6.0, b.center().y),
                    Align2::LEFT_CENTER,
                    &q.requester,
                    FontId::new(12.0, FontFamily::Proportional),
                    TEXT3,
                );
                ui.interact(b, Id::NULL.with(("slot-plan", n)), Sense::hover())
                    .on_hover_text(format!("{} \u{B7} planned from the ETA", q.command));
            }
        }
    }

    // The "now" line crosses every lane.
    let nx = x_of(0.0);
    ui.painter().line_segment(
        [
            egui::pos2(nx, rect.top() + 15.0),
            egui::pos2(nx, rect.bottom()),
        ],
        Stroke::new(2.0_f32, TEXT),
    );
}

/// The block label: dark ink over the filled part of the bar, light text over the rest.
fn label(p: &egui::Painter, b: Rect, fill: Rect, text: &str, kind_color: Color32) {
    let font = FontId::new(12.0, FontFamily::Name("semibold".into()));
    let pos = egui::pos2(b.left() + 8.0, b.center().y);
    p.with_clip_rect(fill)
        .text(pos, Align2::LEFT_CENTER, text, font.clone(), INK);
    let rest = Rect::from_min_max(egui::pos2(fill.right(), b.top()), b.right_bottom());
    p.with_clip_rect(rest)
        .text(pos, Align2::LEFT_CENTER, text, font, kind_color);
}

fn closed(r: Rect) -> [egui::Pos2; 5] {
    [
        r.left_top(),
        r.right_top(),
        r.right_bottom(),
        r.left_bottom(),
        r.left_top(),
    ]
}

/// History of the last 30 minutes, the running jobs, and the queue placed by ETA.
fn items<'a>(snap: &'a Snapshot, history: &'a [JobRow]) -> Vec<Item<'a>> {
    let mut v: Vec<Item> = history
        .iter()
        .filter_map(|j| {
            let end = j.ended_unix - snap.now_unix;
            (end > -PAST && end <= 0.0).then_some(Item {
                start: end - j.duration_secs,
                end,
                block: Block::Done(j),
            })
        })
        .collect();
    for r in &snap.running {
        // The block spans the expected run; a job past its average gets a short tail.
        let expected = r
            .expected_secs
            .filter(|e| *e > r.elapsed)
            .unwrap_or(r.elapsed + 30.0);
        v.push(Item {
            start: -r.elapsed,
            end: -r.elapsed + expected,
            block: Block::Running(r),
        });
    }
    for q in &snap.queue {
        if let Some(eta) = q.eta {
            let len = q.expected_secs.filter(|e| *e > 0.0).unwrap_or(GUESS_SECS);
            if eta < FUTURE {
                v.push(Item {
                    start: eta,
                    end: eta + len,
                    block: Block::Planned(q),
                });
            }
        }
    }
    v
}

/// First-fit lane per item (items come in history, running, planned order; sorted by start inside the pass).
fn lane_assignment(items: &[Item], min_lanes: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by(|a, b| items[*a].start.total_cmp(&items[*b].start));
    let mut lane_end: Vec<f64> = vec![f64::NEG_INFINITY; min_lanes];
    let mut out = vec![0; items.len()];
    for i in order {
        let it = &items[i];
        let planned = matches!(it.block, Block::Planned(_));
        let lane = lane_end
            .iter()
            .position(|e| *e <= it.start + 0.5)
            .unwrap_or_else(|| {
                if planned {
                    // A plan never opens a lane beyond the slots: it queues behind the lane that frees first.
                    (0..min_lanes)
                        .min_by(|a, b| lane_end[*a].total_cmp(&lane_end[*b]))
                        .unwrap_or(0)
                } else if lane_end.len() < MAX_LANES {
                    lane_end.push(f64::NEG_INFINITY);
                    lane_end.len() - 1
                } else {
                    0
                }
            });
        lane_end[lane] = it.end;
        out[i] = lane;
    }
    out
}
