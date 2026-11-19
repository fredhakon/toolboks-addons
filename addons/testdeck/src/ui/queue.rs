//! The queue: position, requester, kind, waited, when it should start, priority, reorder and cancel. A gate that is
//! held says why instead of showing a start time.
use super::{fmt_dur, theme, Snapshot};
use crate::history::kind_str;
use crate::state::Shared;
use egui::{RichText, Ui};
use theme::{TEXT2, TEXT3};

pub(crate) fn show(ui: &mut Ui, snap: &Snapshot, shared: &Shared) {
    theme::title(ui, "Queue");
    if snap.queue.is_empty() {
        let text = if snap.paused {
            "The queue is empty and paused."
        } else {
            "The queue is empty."
        };
        ui.label(RichText::new(text).color(TEXT3));
        return;
    }
    let last = snap.queue.len() - 1;
    egui::Grid::new("queue")
        .num_columns(8)
        .spacing([10.0, 0.0])
        .min_row_height(30.0)
        .show(ui, |ui| {
            for h in [
                "#",
                "Requester",
                "Kind",
                "Waited",
                "Starts",
                "Priority",
                "",
                "",
            ] {
                ui.label(RichText::new(h).size(12.0).color(TEXT3));
            }
            ui.end_row();
            for (i, q) in snap.queue.iter().enumerate() {
                ui.label(theme::mono((i + 1).to_string()));
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.add(
                        egui::Label::new(RichText::new(&q.requester))
                            .wrap_mode(egui::TextWrapMode::Extend),
                    )
                    .on_hover_text(&q.command);
                    if let Some(h) = &q.held {
                        ui.label(RichText::new(h).size(11.0).color(TEXT3));
                    }
                });
                ui.label(
                    RichText::new(kind_str(q.kind))
                        .size(12.0)
                        .color(theme::kind_color(kind_str(q.kind))),
                );
                ui.label(theme::mono(fmt_dur(q.waited)));
                ui.label(
                    theme::mono(
                        q.eta
                            .map_or("-".to_string(), |e| format!("~{}", fmt_dur(e))),
                    )
                    .color(TEXT2),
                );
                let mut p = q.priority;
                if ui
                    .add(egui::DragValue::new(&mut p).range(-100..=100).speed(0.2))
                    .on_hover_text("Higher goes first; equal priority keeps arrival order")
                    .changed()
                {
                    shared.set_priority(q.ticket, p);
                }
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let up = ui
                        .add_enabled(
                            i > 0,
                            egui::Button::new("\u{2191}").min_size(egui::vec2(24.0, 24.0)),
                        )
                        .on_hover_text("Move up");
                    if theme::clicked(ui, &up, ("up", q.ticket)) {
                        shared.move_ticket(q.ticket, true);
                    }
                    let down = ui
                        .add_enabled(
                            i < last,
                            egui::Button::new("\u{2193}").min_size(egui::vec2(24.0, 24.0)),
                        )
                        .on_hover_text("Move down");
                    if theme::clicked(ui, &down, ("down", q.ticket)) {
                        shared.move_ticket(q.ticket, false);
                    }
                });
                let cancel = theme::small_button(ui, "Cancel")
                    .on_hover_text("The client gets a cancelled answer and exits");
                if theme::clicked(ui, &cancel, ("cancel", q.ticket)) {
                    shared.cancel(q.ticket);
                }
                ui.end_row();
            }
        });
}
