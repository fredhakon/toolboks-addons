//! Drawing helpers for the Debloater pages: list rows, status chips, section headings, notes.
//! Presentation only; tokens come from `crate::ui`.

use egui::{self, Align2, Color32, Rect, RichText, Sense, Stroke, StrokeKind, Vec2};

use toolboks_debloat_engine::model::{Evidence, EvidenceInfo};
use toolboks_shared::ui::{
    row_rule, AMBER, BAR_DIVIDER, BORDER, CARD_PAD, CARD_RADIUS, CONTROL_RADIUS, GAP, GREEN, MUTED,
    PANEL_BG, RED, SELECTED_BG, SPACE_XS, TEXT, TEXT_CAPTION, TEXT_SECTION,
};

pub const ROW_HEIGHT: f32 = 30.0;
const CHIP_PAD_X: f32 = 7.0;
const CHIP_HEIGHT: f32 = 18.0;
const CHIP_GAP: f32 = 4.0;
const ROW_PAD: f32 = 8.0;

/// A status chip: a word, coloured. The word carries the meaning, the colour only backs it up.
#[derive(Clone)]
pub struct Chip {
    pub text: String,
    pub color: Color32,
}

impl Chip {
    pub fn new(text: impl Into<String>, color: Color32) -> Self {
        Self {
            text: text.into(),
            color,
        }
    }
}

/// The colour of an evidence badge. The word carries the meaning, the colour only backs it up.
pub fn evidence_color(level: Evidence) -> Color32 {
    match level {
        Evidence::Measured => GREEN,
        Evidence::Mixed => AMBER,
        Evidence::Unproven => RED,
        Evidence::NotSpeed => MUTED,
    }
}

/// The small badge on a list row.
pub fn evidence_chip(info: &EvidenceInfo) -> Chip {
    Chip::new(info.level.label(), evidence_color(info.level))
}

/// What the source line says about how far the source was checked.
pub fn source_status(info: &EvidenceInfo) -> &'static str {
    match (info.source, info.source_checked) {
        (None, _) => "Source: none found.",
        (Some(_), true) => "We read this source against this claim.",
        (Some(source), false) if source.starts_with("https://") => {
            "The link exists; we did not check it against this claim."
        }
        (Some(_), false) => "Not checked against this claim.",
    }
}

/// The evidence part of an "About this item" panel: badge, one sentence, source.
pub fn evidence_block(ui: &mut egui::Ui, info: &EvidenceInfo) {
    field_label(ui, "Evidence");
    ui.label(
        RichText::new(info.level.label())
            .strong()
            .color(evidence_color(info.level)),
    );
    ui.add(egui::Label::new(info.note).wrap());
    match info.source {
        Some(source) if source.starts_with("https://") => {
            ui.hyperlink_to("Open the source", source);
        }
        Some(source) => note(ui, &format!("Source: {source}")),
        None => {}
    }
    note(ui, source_status(info));
}

/// A section heading (14, strong) with an optional one-line muted note below it.
pub fn section(ui: &mut egui::Ui, title: &str, note: Option<&str>) {
    ui.add_space(SPACE_XS);
    ui.label(RichText::new(title).size(TEXT_SECTION).strong().color(TEXT));
    if let Some(note) = note {
        self::note(ui, note);
    }
    ui.add_space(SPACE_XS);
}

/// A small heading inside the review panel.
pub fn panel_heading(ui: &mut egui::Ui, title: &str) {
    ui.label(RichText::new(title).size(TEXT_SECTION).strong().color(TEXT));
}

/// Muted caption text that wraps.
pub fn note(ui: &mut egui::Ui, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).size(TEXT_CAPTION).color(MUTED)).wrap());
}

/// A muted label for a field name in the detail panel.
pub fn field_label(ui: &mut egui::Ui, text: &str) {
    ui.add_space(SPACE_XS);
    ui.label(RichText::new(text).size(TEXT_CAPTION).color(MUTED));
}

/// A rounded card holding a group of rows, with the hairline border.
pub fn group_card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(PANEL_BG)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CARD_RADIUS)
        .inner_margin(egui::Margin::symmetric(CARD_PAD as i8, 2))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            add(ui);
        });
}

/// The frame of the review panel on the right.
pub fn review_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(PANEL_BG)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CARD_RADIUS)
        .inner_margin(CARD_PAD + 2.0)
        .outer_margin(egui::Margin {
            left: GAP as i8,
            ..Default::default()
        })
}

/// A hairline between blocks of the review panel.
pub fn divider(ui: &mut egui::Ui) {
    ui.add_space(GAP);
    let y = ui.cursor().top();
    let x = ui.available_rect_before_wrap().x_range();
    ui.painter().hline(x, y, Stroke::new(1.0, BORDER));
    ui.add_space(GAP);
}

fn chip_width(ui: &egui::Ui, chip: &Chip) -> f32 {
    let font = egui::TextStyle::Small.resolve(ui.style());
    let galley = ui
        .painter()
        .layout_no_wrap(chip.text.clone(), font, chip.color);
    galley.size().x + 2.0 * CHIP_PAD_X
}

/// One list row: selected fill, hover fill, hairline below, a focus ring, and the chips on the
/// right (last chip rightmost). Returns the click response and the free rect on the left for the
/// row's own controls and text.
pub fn row(ui: &mut egui::Ui, focused: bool, chips: &[Chip]) -> (egui::Response, Rect) {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_HEIGHT), Sense::click());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        if focused {
            painter.rect_filled(rect, CONTROL_RADIUS, SELECTED_BG);
        } else if response.hovered() {
            painter.rect_filled(rect, CONTROL_RADIUS, BAR_DIVIDER);
        }
        if !focused && !response.hovered() {
            row_rule(painter, rect);
        }
        if response.has_focus() {
            painter.rect_stroke(
                rect,
                CONTROL_RADIUS,
                ui.visuals().selection.stroke,
                StrokeKind::Inside,
            );
        }
    }

    let mut right = rect.right() - ROW_PAD;
    for chip in chips.iter().rev() {
        let width = chip_width(ui, chip);
        let chip_rect = Rect::from_min_size(
            egui::pos2(right - width, rect.center().y - CHIP_HEIGHT / 2.0),
            Vec2::new(width, CHIP_HEIGHT),
        );
        let painter = ui.painter();
        painter.rect_filled(chip_rect, 9.0, chip.color.gamma_multiply(0.14));
        painter.text(
            chip_rect.center(),
            Align2::CENTER_CENTER,
            chip.text.as_str(),
            egui::TextStyle::Small.resolve(ui.style()),
            chip.color,
        );
        right = chip_rect.left() - CHIP_GAP;
    }

    let body = Rect::from_min_max(
        egui::pos2(rect.left() + ROW_PAD, rect.top()),
        egui::pos2((right - ROW_PAD).max(rect.left() + ROW_PAD), rect.bottom()),
    );
    (response, body)
}

/// Runs `add` left to right, vertically centred, inside the row body.
pub fn row_body(ui: &mut egui::Ui, body: Rect, add: impl FnOnce(&mut egui::Ui)) {
    ui.scope_builder(egui::UiBuilder::new().max_rect(body), |ui| {
        ui.set_clip_rect(body.intersect(ui.clip_rect()));
        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), add);
    });
}

/// Row title text.
pub fn row_title(ui: &mut egui::Ui, text: &str, color: Color32) {
    ui.add(
        egui::Label::new(RichText::new(text).color(color))
            .selectable(false)
            .truncate(),
    );
}

/// Muted detail text in a row.
pub fn row_detail(ui: &mut egui::Ui, text: &str) {
    ui.add(
        egui::Label::new(RichText::new(text).size(TEXT_CAPTION).color(MUTED))
            .selectable(false)
            .truncate(),
    );
}

/// An accent-filled primary button; one per view.
pub fn primary_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text).color(Color32::WHITE))
        .fill(toolboks_shared::ui::HOST_ACCENT_FILL)
}

/// The line at the top of a page: a muted status on the left, Refresh on the right. Returns true
/// when Refresh was clicked.
pub fn page_head(ui: &mut egui::Ui, status: &str, can_refresh: bool) -> bool {
    let mut clicked = false;
    ui.horizontal(|ui| {
        ui.label(RichText::new(status).size(TEXT_CAPTION).color(MUTED));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            clicked = ui
                .add_enabled(can_refresh, egui::Button::new("Refresh"))
                .clicked();
        });
    });
    clicked
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(level: Evidence, source: Option<&'static str>, checked: bool) -> EvidenceInfo {
        EvidenceInfo {
            level,
            note: "A note.",
            source,
            source_checked: checked,
        }
    }

    #[test]
    fn the_badge_word_is_the_evidence_label() {
        for level in [
            Evidence::Measured,
            Evidence::Mixed,
            Evidence::Unproven,
            Evidence::NotSpeed,
        ] {
            assert_eq!(evidence_chip(&info(level, None, false)).text, level.label());
        }
        assert_eq!(Evidence::NotSpeed.label(), "Not a speed tweak");
        assert_ne!(
            evidence_color(Evidence::Unproven),
            evidence_color(Evidence::NotSpeed)
        );
    }

    #[test]
    fn the_source_line_never_claims_more_than_was_checked() {
        let link = "https://learn.microsoft.com/windows/privacy/x";
        assert_eq!(
            source_status(&info(Evidence::Unproven, None, false)),
            "Source: none found."
        );
        assert_eq!(
            source_status(&info(Evidence::NotSpeed, Some(link), false)),
            "The link exists; we did not check it against this claim."
        );
        assert_eq!(
            source_status(&info(
                Evidence::NotSpeed,
                Some("docs/R.md section 4"),
                false
            )),
            "Not checked against this claim."
        );
        assert_eq!(
            source_status(&info(Evidence::NotSpeed, Some(link), true)),
            "We read this source against this claim."
        );
    }
}
