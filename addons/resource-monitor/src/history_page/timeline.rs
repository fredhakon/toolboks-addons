//! The timeline: one lane per resource, gaps labelled, the cursor and the selection, with
//! mouse (hover, click, drag to select, drag the time axis to pan, wheel to zoom) and keyboard
//! (arrows move the cursor, Shift+arrows extend the selection, Escape clears).

use egui::{Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use toolboks_shared::format::rate;
use toolboks_shared::ui::{
    ACCENT, BAR_BG, BAR_DIVIDER, BORDER, CARD_RADIUS, DATA_DISK, DATA_GPU, DATA_MEMORY,
    DATA_NETWORK, DATA_SECOND, MUTED, PANEL_BG, TEXT, TEXT_CAPTION,
};

use super::time::{self, SEC};
use super::view::{Marks, View};
use crate::recording::disk::Level;
use crate::recording::query::{Answer, Bin};
use crate::recording::record::MarkerKind;

/// Height of the time axis strip (drag it to pan).
pub const AXIS_H: f32 = 22.0;
/// Width of the lane name column.
const LABEL_W: f32 = 74.0;
pub const LANE_MIN_H: f32 = 40.0;
pub const LANE_MAX_H: f32 = 64.0;
/// Below this many pixels a drag on the lanes is a click.
const CLICK_SLOP: f32 = 4.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Unit {
    Percent,
    /// Memory in use: shown as a share of `mem_total` when known.
    Memory,
    Rate,
}

pub struct Lane {
    pub name: &'static str,
    pub unit: Unit,
    /// Index into the system totals, and colour; the second line is optional.
    pub first: (usize, Color32, &'static str),
    pub second: Option<(usize, Color32, &'static str)>,
}

pub const LANES: [Lane; 5] = [
    Lane {
        name: "CPU",
        unit: Unit::Percent,
        first: (0, ACCENT, "CPU"),
        second: None,
    },
    Lane {
        name: "Memory",
        unit: Unit::Memory,
        first: (1, DATA_MEMORY, "Memory"),
        second: None,
    },
    Lane {
        name: "GPU",
        unit: Unit::Percent,
        first: (2, DATA_GPU, "GPU"),
        second: None,
    },
    Lane {
        name: "Disk",
        unit: Unit::Rate,
        first: (6, DATA_DISK, "Read"),
        second: Some((7, DATA_SECOND, "Write")),
    },
    Lane {
        name: "Network",
        unit: Unit::Rate,
        first: (8, DATA_NETWORK, "Down"),
        second: Some((9, DATA_SECOND, "Up")),
    },
];

/// A value as the page shows it.
pub fn value_text(unit: Unit, v: f64, mem_total: Option<u64>) -> String {
    match unit {
        Unit::Percent => format!("{v:.0} %"),
        Unit::Memory => match mem_total {
            Some(t) if t > 0 => format!(
                "{} ({:.0} %)",
                toolboks_shared::format::size_plain(v as u64),
                v / t as f64 * 100.0
            ),
            _ => toolboks_shared::format::size_plain(v as u64),
        },
        Unit::Rate => rate(v),
    }
}

/// How the level of detail of a point reads.
pub fn detail_text(level: Option<Level>) -> &'static str {
    match level {
        Some(Level::Fine) => "every 5 s",
        Some(Level::Minute) => "per-minute average",
        Some(Level::Day) => "5-minute average",
        None => "not recorded",
    }
}

#[derive(Clone, Copy, Debug)]
enum DragKind {
    Pan { view: View, x: f32 },
    Select { t: i64, x: f32 },
}

/// Drag state kept between frames.
#[derive(Clone, Copy, Debug, Default)]
pub struct Drag(Option<DragKind>);

/// What the timeline shows instead of curves.
pub struct Empty<'a> {
    pub title: &'a str,
    pub body: &'a str,
}

pub struct Timeline<'a> {
    pub curve: Option<&'a Answer>,
    pub mem_total: Option<u64>,
    pub empty: Option<Empty<'a>>,
    pub now: i64,
    pub lane_h: f32,
}

/// The bin of `curve` holding `t`.
pub fn bin_at(curve: &[Bin], t: i64) -> Option<&Bin> {
    curve.iter().find(|b| b.start_ms <= t && t < b.end_ms)
}

/// Draws the timeline and applies the input to `view`, `marks` and `drag`. Returns the
/// rectangle of the lanes (for tests).
pub fn show(
    ui: &mut egui::Ui,
    t: &Timeline,
    view: &mut View,
    marks: &mut Marks,
    drag: &mut Drag,
) -> Rect {
    let width = ui.available_width();
    let height = t.lane_h * LANES.len() as f32 + AXIS_H + 2.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click_and_drag());
    toolboks_addon_sdk::tutorial::mark(ui, "history-timeline", rect);
    let inner = rect.shrink(1.0);
    let lanes = Rect::from_min_max(
        Pos2::new(inner.left() + LABEL_W, inner.top()),
        Pos2::new(inner.right() - 8.0, inner.bottom() - AXIS_H),
    );
    let axis = Rect::from_min_max(
        Pos2::new(lanes.left(), lanes.bottom()),
        Pos2::new(lanes.right(), inner.bottom()),
    );
    let span = view.span().max(1);
    let t_of = |x: f32, v: &View| {
        v.from + ((x - lanes.left()) / lanes.width().max(1.0) * v.span() as f32) as i64
    };
    // One bin: the step the arrow keys move by.
    let step = t
        .curve
        .and_then(|a| a.curve.first())
        .map(|b| b.end_ms - b.start_ms)
        .unwrap_or(span / 100)
        .max(5 * SEC);

    // --- input --------------------------------------------------------------------------
    if resp.clicked() || resp.drag_started() {
        resp.request_focus();
    }
    if resp.drag_started() {
        let origin = ui.input(|i| i.pointer.press_origin());
        if let Some(p) = origin.or(resp.interact_pointer_pos()) {
            drag.0 = Some(if p.y >= axis.top() {
                DragKind::Pan {
                    view: *view,
                    x: p.x,
                }
            } else {
                DragKind::Select {
                    t: t_of(p.x, view),
                    x: p.x,
                }
            });
        }
    }
    if let (Some(kind), Some(p)) = (drag.0, resp.interact_pointer_pos()) {
        match kind {
            DragKind::Pan { view: start, x } => {
                let ms = ((x - p.x) / lanes.width().max(1.0) * start.span() as f32) as i64;
                *view = start;
                view.pan(ms, t.now);
            }
            DragKind::Select { t: a, x } => {
                if (p.x - x).abs() >= CLICK_SLOP {
                    marks.drag(a, t_of(p.x.clamp(lanes.left(), lanes.right()), view));
                }
            }
        }
    }
    if resp.drag_stopped() {
        if let (Some(DragKind::Select { t: a, x }), Some(p)) = (drag.0, resp.interact_pointer_pos())
        {
            if (p.x - x).abs() < CLICK_SLOP {
                marks.click(a);
            }
        }
        drag.0 = None;
    }
    if resp.clicked() {
        if let Some(p) = resp.interact_pointer_pos().filter(|p| p.y < axis.top()) {
            marks.click(t_of(p.x, view));
        }
    }
    if resp.hovered() {
        let dy = ui.input(|i| i.smooth_scroll_delta.y);
        if dy != 0.0 {
            let at = resp.hover_pos().map(|p| t_of(p.x, view)).unwrap_or(view.to);
            view.zoom(at, (-dy as f64 * 0.003).exp(), t.now);
        }
    }
    if resp.has_focus() {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                resp.id,
                egui::EventFilter {
                    horizontal_arrows: true,
                    escape: true,
                    ..Default::default()
                },
            )
        });
        let keys = ui.input_mut(|i| {
            use egui::{Key, Modifiers};
            [
                // Shift first: the plain pattern would also match a shifted key.
                (i.consume_key(Modifiers::SHIFT, Key::ArrowLeft), -1, true),
                (i.consume_key(Modifiers::SHIFT, Key::ArrowRight), 1, true),
                (i.consume_key(Modifiers::NONE, Key::ArrowLeft), -1, false),
                (i.consume_key(Modifiers::NONE, Key::ArrowRight), 1, false),
                (i.consume_key(Modifiers::NONE, Key::Escape), 0, false),
            ]
        });
        for (pressed, dir, extend) in keys {
            if pressed && dir == 0 {
                marks.clear();
            } else if pressed {
                marks.arrow(dir, extend, step, view, t.now);
            }
        }
    }
    resp.widget_info(|| {
        egui::WidgetInfo::labeled(egui::Role::Group, true, "Resource history timeline")
    });

    // --- drawing ------------------------------------------------------------------------
    let span = view.span().max(1);
    let x_of = |ms: i64| lanes.left() + (ms - view.from) as f32 / span as f32 * lanes.width();
    if !ui.is_rect_visible(rect) {
        return lanes;
    }
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, CARD_RADIUS, PANEL_BG);
    let focus = resp.has_focus();
    let stroke = if focus {
        ui.visuals().selection.stroke
    } else {
        Stroke::new(1.0, BORDER)
    };
    painter.rect_stroke(rect, CARD_RADIUS, stroke, egui::StrokeKind::Inside);
    painter.rect_filled(
        Rect::from_min_max(Pos2::new(inner.left(), axis.top()), inner.right_bottom()),
        egui::CornerRadius {
            nw: 0,
            ne: 0,
            sw: CARD_RADIUS - 1,
            se: CARD_RADIUS - 1,
        },
        BAR_BG,
    );
    let caption = FontId::proportional(TEXT_CAPTION);
    let body = FontId::proportional(12.0);

    // Grid and axis labels.
    let min_gap = (span as f64 * 90.0 / lanes.width().max(1.0) as f64) as i64;
    let (ticks, dated) = time::ticks(view.from, view.to, min_gap);
    for tick in ticks {
        let x = x_of(tick);
        painter.vline(x, lanes.y_range(), Stroke::new(1.0, BAR_DIVIDER));
        let label = if dated {
            time::day(tick)
        } else {
            time::hm(tick)
        };
        painter.text(
            Pos2::new(x, axis.center().y),
            egui::Align2::CENTER_CENTER,
            label,
            caption.clone(),
            MUTED,
        );
    }

    let lane_rect = |i: usize| {
        Rect::from_min_max(
            Pos2::new(inner.left(), lanes.top() + i as f32 * t.lane_h),
            Pos2::new(lanes.right(), lanes.top() + (i + 1) as f32 * t.lane_h),
        )
    };
    let bins: &[Bin] = t.curve.map(|a| a.curve.as_slice()).unwrap_or(&[]);
    for (i, lane) in LANES.iter().enumerate() {
        let r = lane_rect(i);
        if i > 0 {
            painter.hline(r.x_range(), r.top(), Stroke::new(1.0, BAR_DIVIDER));
        }
        let max = lane_max(lane, bins, t.mem_total);
        painter.text(
            Pos2::new(r.left() + 10.0, r.top() + 5.0),
            egui::Align2::LEFT_TOP,
            lane.name,
            body.clone(),
            TEXT,
        );
        if t.empty.is_none() {
            let scale = match lane.unit {
                Unit::Memory if t.mem_total.is_some() => "100 %".to_string(),
                unit => value_text(unit, max, None),
            };
            painter.text(
                Pos2::new(r.left() + 10.0, r.bottom() - 4.0),
                egui::Align2::LEFT_BOTTOM,
                scale,
                caption.clone(),
                MUTED,
            );
        }
        let plot = Rect::from_min_max(
            Pos2::new(lanes.left(), r.top() + 4.0),
            Pos2::new(lanes.right(), r.bottom() - 2.0),
        );
        let lines = std::iter::once(lane.first).chain(lane.second);
        for (n, (idx, color, _)) in lines.enumerate() {
            paint_line(&painter, plot, bins, idx, max, color, n == 0, &x_of);
        }
    }

    // Gaps: hatched and labelled, never a flat line.
    let gap_label = |a: i64, b: i64| {
        let asleep = t.curve.is_some_and(|ans| {
            ans.markers.iter().any(|m| {
                matches!(m.kind, MarkerKind::Gap { since_utc_ms } if since_utc_ms < b && m.utc_ms > a)
            })
        });
        if asleep {
            "Not recorded: the PC was asleep or busy"
        } else {
            "Toolboks was not running"
        }
    };
    let mut run: Option<(i64, i64)> = None;
    let flush = |run: Option<(i64, i64)>| {
        if let Some((a, b)) = run {
            let r = Rect::from_min_max(
                Pos2::new(x_of(a).max(lanes.left()), lanes.top()),
                Pos2::new(x_of(b).min(lanes.right()), lanes.bottom()),
            );
            hatch(&painter, r);
            let label = gap_label(a, b);
            let g = painter.layout_no_wrap(label.to_string(), caption.clone(), MUTED);
            if g.size().x + 12.0 < r.width() {
                let pos = r.center() - g.size() / 2.0;
                painter.rect_filled(
                    Rect::from_min_size(pos - Vec2::new(4.0, 2.0), g.size() + Vec2::new(8.0, 4.0)),
                    4,
                    PANEL_BG,
                );
                painter.galley(pos, g, MUTED);
            }
        }
    };
    if t.empty.is_none() {
        for b in bins
            .iter()
            .filter(|b| b.end_ms > view.from && b.start_ms < view.to)
        {
            if b.is_gap() {
                run = Some(match run {
                    Some((a, _)) => (a, b.end_ms),
                    None => (b.start_ms, b.end_ms),
                });
            } else {
                flush(run.take());
            }
        }
        flush(run);
    }

    // Empty states.
    if let Some(e) = &t.empty {
        let c = lanes.center();
        painter.rect_filled(lanes, 0, PANEL_BG.gamma_multiply(0.9));
        painter.text(
            c - Vec2::new(0.0, 10.0),
            egui::Align2::CENTER_CENTER,
            e.title,
            FontId::proportional(14.0),
            TEXT,
        );
        painter.text(
            c + Vec2::new(0.0, 12.0),
            egui::Align2::CENTER_CENTER,
            e.body,
            caption.clone(),
            MUTED,
        );
    }

    // Selection and cursor.
    if let Some((a, b)) = marks.selection {
        let r = Rect::from_min_max(
            Pos2::new(x_of(a).max(lanes.left()), lanes.top()),
            Pos2::new(x_of(b).min(lanes.right()), lanes.bottom()),
        );
        if r.width() > 0.0 {
            painter.rect_filled(r, 0, ACCENT.gamma_multiply(0.14));
            for x in [r.left(), r.right()] {
                painter.vline(x, r.y_range(), Stroke::new(1.0, ACCENT));
            }
        }
    }
    if let Some(c) = marks.cursor.filter(|c| (view.from..view.to).contains(c)) {
        let x = x_of(c);
        painter.vline(x, lanes.y_range(), Stroke::new(1.5, TEXT));
        painter.text(
            Pos2::new(x, axis.center().y),
            egui::Align2::CENTER_CENTER,
            time::hms(c),
            caption.clone(),
            TEXT,
        );
    }

    // Hover: a guide line and the values at that moment.
    if drag.0.is_none() && t.empty.is_none() {
        if let Some(p) = resp.hover_pos().filter(|p| lanes.contains(*p)) {
            painter.vline(
                p.x,
                lanes.y_range(),
                Stroke::new(1.0, Color32::from_white_alpha(70)),
            );
            let at = t_of(p.x, view);
            let bin = bin_at(bins, at).cloned();
            let mem_total = t.mem_total;
            let now = t.now;
            let seconds = view.span() <= 2 * time::HOUR;
            resp.on_hover_ui_at_pointer(|ui| {
                ui.label(
                    egui::RichText::new(time::moment(at, now, seconds))
                        .small()
                        .weak(),
                );
                readings_ui(ui, bin.as_ref(), mem_total);
            });
        }
    }
    lanes
}

/// The readings of one point, one row per line, for the hover read-out.
pub fn readings_ui(ui: &mut egui::Ui, bin: Option<&Bin>, mem_total: Option<u64>) {
    let Some(bin) = bin.filter(|b| !b.is_gap()) else {
        ui.label("Not recorded");
        return;
    };
    for lane in &LANES {
        for (idx, color, label) in std::iter::once(lane.first).chain(lane.second) {
            let Some(s) = bin.totals[idx] else { continue };
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 5.0;
                let (dot, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                ui.painter().circle_filled(dot.center(), 3.5, color);
                ui.label(label);
                ui.label(
                    egui::RichText::new(value_text(lane.unit, s.avg as f64, mem_total)).strong(),
                );
                if bin.detail != Some(Level::Fine) && s.peak > s.avg {
                    ui.label(
                        egui::RichText::new(format!(
                            "peak {}",
                            value_text(lane.unit, s.peak as f64, mem_total)
                        ))
                        .weak(),
                    );
                }
            });
        }
    }
    let recorded = bin.covered_secs / ((bin.end_ms - bin.start_ms) as f64 / 1000.0);
    let mut note = detail_text(bin.detail).to_string();
    if recorded < 0.95 {
        note.push_str(&format!(
            ", {:.0} % of this point recorded",
            recorded * 100.0
        ));
    }
    ui.label(egui::RichText::new(note).small().weak());
}

/// The top of a lane's scale.
fn lane_max(lane: &Lane, bins: &[Bin], mem_total: Option<u64>) -> f64 {
    match lane.unit {
        Unit::Percent => 100.0,
        Unit::Memory => match mem_total {
            Some(t) if t > 0 => t as f64,
            _ => nice(peak_of(lane, bins), 1024.0 * 1024.0 * 1024.0),
        },
        Unit::Rate => nice(peak_of(lane, bins), 1024.0 * 1024.0),
    }
}

fn peak_of(lane: &Lane, bins: &[Bin]) -> f64 {
    std::iter::once(lane.first)
        .chain(lane.second)
        .flat_map(|(idx, _, _)| bins.iter().filter_map(move |b| b.totals[idx]))
        .map(|s| s.peak as f64)
        .fold(0.0, f64::max)
}

fn nice(max: f64, floor: f64) -> f64 {
    crate::overview::nice_ceiling(max as f32, floor as f32) as f64
}

/// One line of a lane: the average with the area under it tinted (first line only), and a
/// faint peak line where the points are averages of several readings.
#[allow(clippy::too_many_arguments)]
fn paint_line(
    painter: &egui::Painter,
    plot: Rect,
    bins: &[Bin],
    idx: usize,
    max: f64,
    color: Color32,
    fill: bool,
    x_of: &dyn Fn(i64) -> f32,
) {
    let y = |v: f32| {
        plot.bottom() - (v as f64 / max.max(f64::EPSILON)).clamp(0.0, 1.0) as f32 * plot.height()
    };
    let mut avg: Vec<Pos2> = Vec::new();
    let mut peak: Vec<Pos2> = Vec::new();
    let flush = |avg: &mut Vec<Pos2>, peak: &mut Vec<Pos2>| {
        if avg.len() > 1 {
            if peak.len() > 1 {
                painter.add(egui::Shape::line(
                    std::mem::take(peak),
                    Stroke::new(1.0, color.gamma_multiply(0.45)),
                ));
            }
            painter.add(egui::Shape::line(
                std::mem::take(avg),
                Stroke::new(1.5, color),
            ));
        } else if let Some(p) = avg.first() {
            painter.circle_filled(*p, 1.5, color);
        }
        avg.clear();
        peak.clear();
    };
    for b in bins {
        let (x0, x1) = (x_of(b.start_ms), x_of(b.end_ms));
        if x1 < plot.left() || x0 > plot.right() {
            continue;
        }
        let Some(s) = b.totals[idx].filter(|_| !b.is_gap()) else {
            flush(&mut avg, &mut peak);
            continue;
        };
        let (x0, x1) = (x0.max(plot.left()), x1.min(plot.right()));
        let ya = y(s.avg);
        if fill {
            painter.rect_filled(
                Rect::from_min_max(Pos2::new(x0, ya), Pos2::new(x1, plot.bottom())),
                0,
                color.gamma_multiply(0.16),
            );
        }
        let xm = (x0 + x1) / 2.0;
        avg.push(Pos2::new(xm, ya));
        if b.detail != Some(Level::Fine) {
            peak.push(Pos2::new(xm, y(s.peak)));
        }
    }
    flush(&mut avg, &mut peak);
}

/// Diagonal hatching over `r`.
fn hatch(painter: &egui::Painter, r: Rect) {
    if r.width() <= 0.0 {
        return;
    }
    painter.rect_filled(r, 0, BAR_BG.gamma_multiply(0.6));
    let p = painter.with_clip_rect(r.intersect(painter.clip_rect()));
    let stroke = Stroke::new(1.0, BORDER);
    let mut x = r.left() - r.height();
    while x < r.right() {
        p.line_segment(
            [Pos2::new(x, r.bottom()), Pos2::new(x + r.height(), r.top())],
            stroke,
        );
        x += 8.0;
    }
}
