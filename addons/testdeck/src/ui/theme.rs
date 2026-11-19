//! Testdeck's drawing helpers on the Toolboks look: every colour comes from `toolboks_shared::ui` (the host's fonts,
//! sizes and tokens are applied once by `style_once`), so the pages never carry a hex value of their own. The kind
//! colours are the host's data colours, without the accent (that one marks selection).
use egui::{Color32, Frame, Response, RichText, Sense, Stroke, Ui, Vec2};
use toolboks_shared::ui as tb;

pub(crate) use tb::{AMBER, BORDER, GREEN, RED, TEXT};
/// The card fill.
pub(crate) const PANEL: Color32 = tb::PANEL_BG;
/// Secondary and tertiary text are one muted tone in Toolboks.
pub(crate) const TEXT2: Color32 = tb::MUTED;
pub(crate) const TEXT3: Color32 = tb::MUTED;
/// Warnings that need action (stalled jobs, orphans, failures).
pub(crate) const ORANGE: Color32 = tb::RED;
/// The unfilled part of a bar.
pub(crate) const TRACK: Color32 = tb::BORDER;
/// Dark ink for text drawn on a filled kind colour.
pub(crate) const INK: Color32 = tb::WINDOW_BG;
/// A finished job that failed, as a muted block on the slot timeline.
pub(crate) const FAILED_BLOCK: Color32 = Color32::from_rgb(0x4a, 0x22, 0x20);

pub(crate) fn kind_color(kind: &str) -> Color32 {
    match kind {
        "jest" => tb::DATA_DISK,
        "build" => tb::DATA_NETWORK,
        "gate" => tb::DATA_MEMORY,
        "tsc" => tb::DATA_GPU,
        _ => tb::DATA_SECOND,
    }
}

/// The kind colour at about a fifth strength over a panel: the unfilled part of a bar or block.
pub(crate) fn dim(c: Color32) -> Color32 {
    let mix = |a: u8, b: u8| (f32::from(a) * 0.2 + f32::from(b) * 0.8).round() as u8;
    Color32::from_rgb(
        mix(c.r(), PANEL.r()),
        mix(c.g(), PANEL.g()),
        mix(c.b(), PANEL.b()),
    )
}

pub(crate) fn ok_color() -> Color32 {
    GREEN
}

/// Headings and names.
pub(crate) fn strong(text: impl Into<String>) -> RichText {
    RichText::new(text).strong().color(TEXT)
}

/// Monospace at the body size: tabular digits for every number that changes.
pub(crate) fn mono(text: impl Into<String>) -> RichText {
    RichText::new(text).monospace().size(tb::TEXT_BODY)
}

/// Section title.
pub(crate) fn title(ui: &mut Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .size(tb::TEXT_SECTION)
            .strong()
            .color(TEXT),
    );
}

/// The Toolboks card frame (fill, border, radius, padding), for cards whose width follows the content.
pub(crate) fn card() -> Frame {
    Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(tb::CARD_RADIUS)
        .inner_margin(tb::CARD_PAD)
}

/// A plain host-styled button.
pub(crate) fn small_button(ui: &mut Ui, text: impl Into<egui::WidgetText>) -> Response {
    ui.button(text)
}

/// egui's spinner drawn at 10 frames a second. `egui::Spinner` asks for a repaint every frame, which keeps the add-on
/// redrawing at the display rate for as long as a job has no bar.
pub(crate) fn spinner(ui: &mut Ui, size: f32) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    if ui.is_rect_visible(rect) {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(100));
        let time = ui.input(|i| i.time);
        let (start, sweep) = (
            time * std::f64::consts::TAU,
            240f64.to_radians() * time.sin(),
        );
        let radius = rect.height() / 2.0 - 2.0;
        let points: Vec<egui::Pos2> = (0..20)
            .map(|i| {
                let (sin, cos) = (start + sweep * f64::from(i) / 20.0).sin_cos();
                rect.center() + radius * egui::vec2(cos as f32, sin as f32)
            })
            .collect();
        ui.painter().add(egui::Shape::line(
            points,
            Stroke::new(3.0_f32, ui.visuals().strong_text_color()),
        ));
    }
    response
}

/// Whether a button drawn on the live pages was clicked, read through a fixed id. Auto ids there shift when an earlier
/// widget comes or goes between frames, and a press and release on different ids is lost (seen on Kill).
pub(crate) fn clicked(ui: &Ui, r: &Response, id: impl egui::AsIdSalt) -> bool {
    r.enabled()
        && (r.clicked()
            || ui
                .interact(r.rect, egui::Id::NULL.with(id), Sense::click())
                .clicked())
}

/// A flat bar: `Some(fraction)` fills from the left, `None` is an empty track.
pub(crate) fn bar(ui: &mut Ui, frac: Option<f32>, color: Color32, height: f32) -> Response {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
    paint_bar(ui, rect, frac, color, TRACK);
    resp
}

pub(crate) fn paint_bar(
    ui: &Ui,
    rect: egui::Rect,
    frac: Option<f32>,
    color: Color32,
    track: Color32,
) {
    let p = ui.painter();
    let r = (rect.height() / 2.0).floor();
    p.rect_filled(rect, r, track);
    if let Some(f) = frac {
        let mut fill = rect;
        fill.set_width((rect.width() * f.clamp(0.0, 1.0)).max(if f > 0.0 { r * 2.0 } else { 0.0 }));
        p.rect_filled(fill, r, color);
    }
}

/// A small coloured kind tag.
pub(crate) fn kind_tag(ui: &mut Ui, kind: &str) {
    ui.label(
        RichText::new(kind)
            .size(tb::TEXT_BODY)
            .color(kind_color(kind)),
    );
}
