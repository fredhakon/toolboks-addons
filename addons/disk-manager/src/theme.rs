//! Disk Manager helpers using the pinned shared design tokens.
use egui::{Color32, RichText, Vec2};

pub use toolboks_shared::ui::{ACCENT, BORDER as HAIRLINE, MUTED, PANEL_BG as CARD, TEXT};
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
pub const CONTROL_BORDER: Color32 = Color32::from_rgb(0x3a, 0x3d, 0x46);

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
        egui::Button::new(RichText::new(label).color(TEXT).strong())
            .fill(toolboks_shared::ui::HOST_ACCENT_FILL)
            .corner_radius(6)
            .min_size(Vec2::new(75.0, 28.0)),
    )
}

/// Secondary button: outlined, no fill.
pub fn outlined(label: &str) -> egui::Button<'_> {
    egui::Button::new(label)
        .fill(Color32::TRANSPARENT)
        .stroke(egui::Stroke::new(1.0, CONTROL_BORDER))
        .corner_radius(6)
        .min_size(Vec2::new(0.0, 28.0))
}

/// Quiet, labelled elsewhere: capacity and share bars never imply a health score.
pub fn bar(ui: &mut egui::Ui, fraction: f32, color: Color32, height: f32) {
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), height),
        egui::Sense::hover(),
    );
    ui.painter().rect_filled(rect, 3, HAIRLINE);
    let fill = egui::Rect::from_min_size(
        rect.min,
        Vec2::new(rect.width() * fraction.clamp(0.0, 1.0), rect.height()),
    );
    ui.painter().rect_filled(fill, 3, color);
}

pub fn selectable_card(ui: &egui::Ui, response: &egui::Response, selected: bool, label: &str) {
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::Role::Button, response.enabled(), selected, label)
    });
    if response.has_focus() || response.hovered() {
        ui.painter().rect_stroke(
            response.rect,
            10,
            egui::Stroke::new(1.0, if response.has_focus() { ACCENT } else { MUTED }),
            egui::StrokeKind::Inside,
        );
    }
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
