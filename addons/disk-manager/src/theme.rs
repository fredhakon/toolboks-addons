//! Disk Manager helpers using the pinned shared design tokens.
use egui::{Color32, RichText, Vec2};

pub use toolboks_shared::ui::{
    ACCENT, BORDER as HAIRLINE, MUTED, PANEL_BG as CARD, TEXT, WINDOW_BG as WINDOW,
};
use toolboks_shared::ui::{DATA_CPU, DATA_DISK, DATA_GPU, DATA_MEMORY, DATA_NETWORK, DATA_SECOND};

/// Shared data colours, in the same order as the original space map.
pub const DATA: [Color32; 6] = [
    DATA_CPU,
    DATA_MEMORY,
    DATA_GPU,
    DATA_DISK,
    DATA_NETWORK,
    DATA_SECOND,
];

pub const SPACE_S: f32 = 4.0;
pub const SPACE_M: f32 = 8.0;
pub const SPACE_L: f32 = 12.0;
pub const SPACE_XL: f32 = 16.0;

/// Card: 10 px radius, 1 px hairline, no shadow, no coloured edge.
pub fn card() -> egui::Frame {
    egui::Frame::new()
        .fill(CARD)
        .stroke(egui::Stroke::new(1.0, HAIRLINE))
        .corner_radius(10)
        .inner_margin(12)
}

/// Page title: 18 px, once per page.
pub fn title(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).size(18.0).strong());
}

/// Section heading inside a card: 14 px.
pub fn section(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).size(14.0).strong());
}

/// Muted body text.
pub fn weak(ui: &mut egui::Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).color(MUTED));
}

/// Muted 11 px caption.
pub fn caption(ui: &mut egui::Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).size(11.0).color(MUTED));
}

/// Inline stat: value in strong text, then a small muted label.
pub fn stat(ui: &mut egui::Ui, value: impl Into<String>, label: &str) {
    ui.label(RichText::new(value.into()).strong());
    ui.label(RichText::new(label).size(11.0).color(MUTED));
    ui.add_space(SPACE_M);
}

/// Primary button: accent fill, one per view.
pub fn primary(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(label).color(WINDOW).strong())
            .fill(ACCENT)
            .corner_radius(6)
            .min_size(Vec2::new(75.0, 28.0)),
    )
}

/// Secondary button: outlined, no fill.
pub fn outlined(label: &str) -> egui::Button<'_> {
    egui::Button::new(label)
        .fill(Color32::TRANSPARENT)
        .stroke(egui::Stroke::new(1.0, HAIRLINE))
        .corner_radius(6)
}

/// One-line, muted, dismissible banner. Returns true when dismissed.
pub fn banner(ui: &mut egui::Ui, text: &str) -> bool {
    let mut dismissed = false;
    card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_wrapped(|ui| {
            weak(ui, text);
            if ui.small_button("Dismiss").clicked() {
                dismissed = true;
            }
        });
    });
    dismissed
}

/// Table header cell text: 11 px muted, sentence case.
pub fn header_cell(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).size(11.0).color(MUTED));
}

pub fn number_cell(ui: &mut egui::Ui, contents: impl FnOnce(&mut egui::Ui)) {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), contents);
}
