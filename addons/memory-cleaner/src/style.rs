//! Small pieces shared by the memory pages, built on the tokens of `crate::ui`.

use egui::{self, Align, Color32, Layout, RichText, Vec2};

use toolboks_shared::ui::{card, CONTROL_RADIUS, RED, TEXT_CAPTION, TEXT_SECTION};

/// A card as wide as the space left, at least `min_height` tall.
pub fn panel(ui: &mut egui::Ui, min_height: f32, add: impl FnOnce(&mut egui::Ui)) {
    let width = ui.available_width();
    card(ui, width, min_height, add);
}

/// A section title: 14, semibold, sentence case.
pub fn section(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).size(TEXT_SECTION).strong());
}

/// A section title with a muted note at the right.
pub fn section_with_note(ui: &mut egui::Ui, text: &str, note: &str) {
    ui.horizontal(|ui| {
        section(ui, text);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(note).weak());
        });
    });
}

/// A muted label at the left and its value at the right.
pub fn stat_row(ui: &mut egui::Ui, label: &str, value: impl Into<String>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).weak());
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(value.into());
        });
    });
}

/// The primary button: accent fill, full width, one per view.
pub fn primary_button(ui: &mut egui::Ui, enabled: bool, label: &str) -> egui::Response {
    let button = egui::Button::new(RichText::new(label).strong().color(Color32::WHITE))
        .fill(toolboks_shared::ui::HOST_ACCENT_FILL)
        .min_size(Vec2::new(ui.available_width(), 28.0))
        .corner_radius(CONTROL_RADIUS);
    ui.add_enabled(enabled, button)
}

/// The evidence badge shown next to an action that claims no benefit (issue #33).
pub const UNPROVEN: &str = "Unproven";

/// Trim, from the one benchmark we ran (docs/RESEARCH-memory-cleaning.md, issue #32).
pub const TRIM_EVIDENCE: &str = "On one 32 GB PC a test found no gain beyond the noise and apps were slower to read their memory back afterwards, so no benefit is shown.";

/// The cache purge was not measured: it needs an administrator confirmation.
pub const PURGE_EVIDENCE: &str =
    "Not measured yet, because it needs administrator confirmation, so no benefit is shown.";

/// A small "Unproven" badge and one plain sentence under an action.
pub fn unproven_note(ui: &mut egui::Ui, sentence: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(UNPROVEN)
                .size(TEXT_CAPTION)
                .strong()
                .color(RED),
        );
        ui.label(RichText::new(sentence).size(TEXT_CAPTION).weak());
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sentences_say_no_benefit_is_shown_and_claim_none() {
        for sentence in [TRIM_EVIDENCE, PURGE_EVIDENCE] {
            assert!(sentence.ends_with('.'));
            assert!(sentence.contains("no benefit is shown"));
            assert_eq!(sentence.matches(". ").count(), 0, "one sentence");
            for word in ["faster", "frees", "boost", "improves"] {
                assert!(!sentence.contains(word), "{word}");
            }
        }
        assert_eq!(UNPROVEN, "Unproven");
    }
}
