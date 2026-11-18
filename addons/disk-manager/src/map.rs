//! Space map: a squarified treemap that always fits the width it is given (no sideways
//! scrolling). Tile areas are proportional to bytes. Entries too small to read are grouped
//! into one "N smaller items" tile, and empty (0-byte) entries get no tile at all.
use crate::category::Category;
use crate::scanner::Node;
use egui::{Rect, Vec2};
const GAP: f32 = 3.0;
/// The smallest tile side that still shows a readable size label.
const MIN_SIDE: f32 = 40.0;
/// More tiles than this stop being readable at any width.
const MAX_TILES: usize = 80;
struct Entry {
    id: usize,
    name: String,
    bytes: u64,
    category: Category,
}
/// The entries of one folder, ready to be placed at any size.
pub struct MapLayout {
    /// Non-empty entries, largest first.
    entries: Vec<Entry>,
    /// Entries that hold 0 bytes; they are counted, not drawn.
    empty: usize,
}
#[derive(Clone, Copy, Debug, PartialEq)]
enum Target {
    /// An index into `MapLayout::entries`.
    Entry(usize),
    /// Entries too small to draw on their own.
    Group { count: usize, bytes: u64 },
}
struct Placed {
    target: Target,
    rect: Rect,
}
impl MapLayout {
    #[cfg(test)]
    pub fn new(nodes: &[&Node]) -> Self {
        Self::with_tree(nodes, &[])
    }
    pub fn with_tree(nodes: &[&Node], all: &[Node]) -> Self {
        let categories = crate::category::folders(nodes, all);
        let mut entries: Vec<Entry> = nodes
            .iter()
            .filter(|n| n.bytes > 0)
            .map(|n| Entry {
                id: n.id,
                name: n.name.clone(),
                bytes: n.bytes,
                category: categories[&n.id],
            })
            .collect();
        entries.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.id.cmp(&b.id)));
        Self {
            empty: nodes.len() - entries.len(),
            entries,
        }
    }
    /// Tiles for a map of `size`, in the map's own coordinates (top left is 0, 0).
    fn place(&self, size: Vec2) -> Vec<Placed> {
        let sizes: Vec<u64> = self.entries.iter().map(|e| e.bytes).collect();
        let area = (size.x + GAP) as f64 * (size.y + GAP) as f64;
        let (shown, rest) = plan(&sizes, area);
        let mut targets: Vec<Target> = (0..shown).map(Target::Entry).collect();
        let mut weights: Vec<f64> = sizes[..shown].iter().map(|&b| b as f64).collect();
        if let Some(rest_area) = rest {
            let shown_bytes: f64 = weights.iter().sum();
            // In byte units, so the shown tiles keep their exact ratios.
            let weight = rest_area * shown_bytes / (area - rest_area).max(1.0);
            let left = &self.entries[shown..];
            targets.push(if left.len() == 1 {
                Target::Entry(shown)
            } else {
                Target::Group {
                    count: left.len(),
                    bytes: left.iter().map(|e| e.bytes).sum(),
                }
            });
            weights.push(weight);
        }
        let outer = Rect::from_min_size(egui::pos2(0.0, 0.0), size).expand(GAP / 2.0);
        squarify(&weights, outer)
            .into_iter()
            .zip(targets)
            .map(|(rect, target)| Placed {
                target,
                rect: rect.shrink(GAP / 2.0),
            })
            .collect()
    }
}
/// How many entries (largest first) get their own tile in `area`, and the area of the tile
/// that holds the rest. Shown tiles stay proportional; the rest tile is at least one readable
/// tile. When only one entry is left over, that tile is the entry itself.
fn plan(sizes: &[u64], area: f64) -> (usize, Option<f64>) {
    let n = sizes.len();
    if n == 0 || area <= 0.0 {
        return (0, None);
    }
    let min = ((MIN_SIDE + GAP) as f64).powi(2);
    let total: f64 = sizes.iter().map(|&b| b as f64).sum();
    let mut shown_bytes: f64 = sizes[..n.min(MAX_TILES)].iter().map(|&b| b as f64).sum();
    let mut k = n.min(MAX_TILES);
    loop {
        let rest = (n > k).then(|| {
            ((total - shown_bytes) / total * area)
                .max(min)
                .min(area / 2.0)
        });
        let scale = (area - rest.unwrap_or(0.0)) / shown_bytes;
        if k == 1 || sizes[k - 1] as f64 * scale >= min {
            return (k, rest);
        }
        k -= 1;
        shown_bytes -= sizes[k] as f64;
    }
}
/// Squarified treemap (Bruls, Huizing and van Wijk): rows along the shorter side, each grown
/// while that keeps its tiles closer to square. `weights` are largest first.
fn squarify(weights: &[f64], mut rect: Rect) -> Vec<Rect> {
    let total: f64 = weights.iter().sum();
    if weights.is_empty() || total <= 0.0 {
        return vec![];
    }
    let scale = rect.area() as f64 / total;
    let mut out = Vec::with_capacity(weights.len());
    let mut i = 0;
    while i < weights.len() {
        let wide = rect.width() >= rect.height();
        let short = rect.width().min(rect.height()) as f64;
        let worst = |row: &[f64]| {
            let sum: f64 = row.iter().sum::<f64>() * scale;
            let max = row.iter().cloned().fold(0.0, f64::max) * scale;
            let min = row.iter().cloned().fold(f64::MAX, f64::min) * scale;
            let (s2, sum2) = (short * short, sum * sum);
            (s2 * max / sum2).max(sum2 / (s2 * min))
        };
        let mut j = i + 1;
        while j < weights.len() && worst(&weights[i..=j]) <= worst(&weights[i..j]) {
            j += 1;
        }
        let row_area: f64 = weights[i..j].iter().sum::<f64>() * scale;
        // The last row takes what is left, so rounding never leaves a gap.
        let long = rect.width().max(rect.height());
        let thickness = if j == weights.len() {
            long
        } else {
            ((row_area / short) as f32).min(long)
        };
        let mut along = 0.0;
        for (k, weight) in weights[i..j].iter().enumerate() {
            let length = if i + k + 1 == j {
                short as f32 - along
            } else {
                (weight * scale / thickness as f64) as f32
            };
            out.push(if wide {
                Rect::from_min_size(
                    rect.min + Vec2::new(0.0, along),
                    Vec2::new(thickness, length),
                )
            } else {
                Rect::from_min_size(
                    rect.min + Vec2::new(along, 0.0),
                    Vec2::new(length, thickness),
                )
            });
            along += length;
        }
        if wide {
            rect.min.x += thickness;
        } else {
            rect.min.y += thickness;
        }
        i = j;
    }
    out
}
#[derive(Default)]
pub struct Interaction {
    pub selected: Option<usize>,
    menu: Option<(usize, egui::Pos2)>,
    focus: Option<egui::Id>,
}
#[derive(Default)]
pub struct Action {
    pub open: Option<usize>,
    pub reveal: Option<usize>,
    pub delete: Option<usize>,
    pub copy: Option<usize>,
}
fn menu_key(input: &mut egui::InputState) -> bool {
    input.consume_key(egui::Modifiers::SHIFT, egui::Key::F10)
        || input.consume_key(egui::Modifiers::NONE, egui::Key::Menu)
}
pub fn show(ui: &mut egui::Ui, layout: &MapLayout, state: &mut Interaction, height: f32) -> Action {
    if layout.entries.is_empty() {
        crate::theme::weak(
            ui,
            if layout.empty > 0 {
                "Everything in this folder is empty (0 bytes)."
            } else {
                "This folder is empty."
            },
        );
        return Action::default();
    }
    let mut action = Action::default();
    let (area, _) = ui.allocate_exact_size(
        Vec2::new(ui.available_width().max(MIN_SIDE), height),
        egui::Sense::hover(),
    );
    let placed = layout.place(area.size());
    let mut grouped = 0;
    for tile in &placed {
        let rect = tile.rect.translate(area.min.to_vec2());
        let (id, name, bytes, color, label) = match tile.target {
            Target::Entry(index) => {
                let e = &layout.entries[index];
                (
                    Some(e.id),
                    e.name.clone(),
                    e.bytes,
                    crate::category::color_for(e.category, e.bytes, index),
                    format!(
                        "{} - {} - {}",
                        e.name,
                        crate::size(e.bytes),
                        e.category.label()
                    ),
                )
            }
            Target::Group { count, bytes } => {
                grouped = count;
                (
                    None,
                    format!("{count} smaller items"),
                    bytes,
                    Category::Other.color(1),
                    format!(
                        "{count} smaller items - {} in total. Each is too small to draw; the list shows them.",
                        crate::size(bytes)
                    ),
                )
            }
        };
        let response = match id {
            Some(id) => ui.interact(
                rect,
                ui.scope_id().with(("disk-map", id)),
                egui::Sense::click(),
            ),
            None => ui.interact(
                rect,
                ui.scope_id().with("disk-map-rest"),
                egui::Sense::hover(),
            ),
        };
        if let Some(id) = id {
            if response.clicked() || response.secondary_clicked() || response.has_focus() {
                state.selected = Some(id);
                state.focus = Some(response.id);
            }
            if response.clicked() || response.secondary_clicked() {
                response.request_focus();
            }
            if response.secondary_clicked() {
                state.menu = Some((id, response.interact_pointer_pos().unwrap_or(rect.center())));
            }
            if response.has_focus() && ui.input_mut(menu_key) {
                state.menu = Some((id, rect.left_top() + Vec2::splat(12.0)));
            }
            if response.clicked() {
                action.open = Some(id);
            }
        }
        ui.painter().rect_filled(rect, 6, color);
        let selected = id.is_some() && state.selected == id;
        if selected {
            let badge = egui::Rect::from_min_max(
                egui::pos2(rect.left(), (rect.bottom() - 23.0).max(rect.top())),
                rect.right_bottom(),
            );
            ui.painter().rect_filled(badge, 6, crate::theme::ACCENT);
        }
        if response.hovered() || response.has_focus() {
            ui.painter().rect_stroke(
                rect,
                6,
                egui::Stroke::new(
                    1.5,
                    if response.has_focus() {
                        crate::theme::ACCENT
                    } else {
                        crate::theme::TEXT
                    },
                ),
                egui::StrokeKind::Inside,
            );
        }
        let ink = crate::category::text(color);
        let painter = ui.painter().with_clip_rect(rect.shrink(4.0));
        if rect.height() >= 44.0 && rect.width() >= 30.0 {
            let mut job = egui::text::LayoutJob::simple(
                name,
                egui::FontId::proportional(12.0),
                ink,
                (rect.width() - 14.0).max(1.0),
            );
            job.wrap.max_rows = 1;
            job.wrap.break_anywhere = true;
            painter.galley(rect.min + Vec2::new(7.0, 7.0), painter.layout_job(job), ink);
        }
        if rect.height() >= 20.0 && rect.width() >= 24.0 {
            painter.text(
                rect.left_bottom() + Vec2::new(7.0, -6.0),
                egui::Align2::LEFT_BOTTOM,
                crate::size(bytes),
                egui::FontId::proportional(11.0),
                if selected {
                    crate::category::text(crate::theme::ACCENT)
                } else {
                    ink
                },
            );
        }
        if selected && rect.width() > 135.0 && rect.height() >= 20.0 {
            painter.text(
                rect.right_bottom() + Vec2::new(-7.0, -6.0),
                egui::Align2::RIGHT_BOTTOM,
                "Selected",
                egui::FontId::proportional(11.0),
                crate::category::text(crate::theme::ACCENT),
            );
        }
        let role = if id.is_some() {
            egui::Role::Button
        } else {
            egui::Role::Label
        };
        response.widget_info(|| egui::WidgetInfo::labeled(role, ui.is_enabled(), &label));
        response.on_hover_text(label);
    }
    if let Some((id, position)) = state.menu {
        let mut open = true;
        egui::Popup::new(
            ui.scope_id().with("map-menu"),
            ui.ctx().clone(),
            position,
            ui.layer_id(),
        )
        .kind(egui::PopupKind::Menu)
        .open_bool(&mut open)
        .show(|ui| {
            if ui.button("Show in Explorer").clicked() {
                action.reveal = Some(id);
                ui.close();
            }
            if ui.button("Copy path").clicked() {
                action.copy = Some(id);
                ui.close();
            }
            ui.separator();
            if ui.button(crate::delete::menu_label()).clicked() {
                action.delete = Some(id);
                ui.close();
            }
        });
        if !open {
            state.menu = None;
            if let Some(id) = state.focus {
                ui.memory_mut(|m| m.request_focus(id));
            }
        }
    }
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        let mut shown = Vec::new();
        for tile in &placed {
            let Target::Entry(index) = tile.target else {
                continue;
            };
            let category = layout.entries[index].category;
            if shown.contains(&category) {
                continue;
            }
            shown.push(category);
            let (r, _) = ui.allocate_exact_size(Vec2::splat(8.0), egui::Sense::hover());
            ui.painter().rect_filled(r, 2, category.color(0));
            crate::theme::caption(ui, category.label());
        }
    });
    crate::theme::caption(ui, note(grouped, layout.empty));
    action
}
fn note(grouped: usize, empty: usize) -> String {
    let mut note = String::from("Area shows size. Click a folder to list its files.");
    if grouped > 0 {
        note.push_str(&format!(" {grouped} smaller items share one tile."));
    }
    if empty > 0 {
        note.push_str(&format!(
            " {empty} empty {} not drawn.",
            if empty == 1 { "item is" } else { "items are" }
        ));
    }
    note
}
#[cfg(test)]
mod tests {
    use super::*;
    fn frame(
        ctx: &egui::Context,
        layout: &MapLayout,
        state: &mut Interaction,
        events: Vec<egui::Event>,
    ) {
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(700.0, 600.0),
                )),
                events,
                ..Default::default()
            },
            |ui| {
                show(ui, layout, state, 300.0);
            },
        );
        output.textures_delta.clear();
    }
    fn key(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }
    #[test]
    fn context_menu_opens_at_pointer_and_closes_on_escape_or_outside_click() {
        let ctx = egui::Context::default();
        let n = [node(0, 10)];
        let layout = MapLayout::new(&n.iter().collect::<Vec<_>>());
        let mut state = Interaction::default();
        frame(&ctx, &layout, &mut state, vec![]);
        let pos = egui::pos2(50.0, 50.0);
        for pressed in [true, false] {
            frame(
                &ctx,
                &layout,
                &mut state,
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Secondary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
        }
        assert_eq!(state.menu, Some((0, pos)));
        assert_eq!(state.selected, Some(0));
        frame(
            &ctx,
            &layout,
            &mut state,
            vec![key(egui::Key::Escape, egui::Modifiers::NONE)],
        );
        assert!(state.menu.is_none());
        for k in [
            key(egui::Key::F10, egui::Modifiers::SHIFT),
            key(egui::Key::Menu, egui::Modifiers::NONE),
        ] {
            frame(&ctx, &layout, &mut state, vec![k]);
            assert_eq!(state.menu.map(|m| m.0), Some(0));
            frame(
                &ctx,
                &layout,
                &mut state,
                vec![key(egui::Key::Escape, egui::Modifiers::NONE)],
            );
            assert!(state.menu.is_none());
        }
        state.menu = Some((0, pos));
        frame(&ctx, &layout, &mut state, vec![]);
        let outside = egui::pos2(650.0, 500.0);
        for pressed in [true, false] {
            frame(
                &ctx,
                &layout,
                &mut state,
                vec![
                    egui::Event::PointerMoved(outside),
                    egui::Event::PointerButton {
                        pos: outside,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
        }
        assert!(state.menu.is_none());
    }
    fn node(id: usize, bytes: u64) -> Node {
        Node {
            id,
            parent: Some(0),
            name: format!("entry-{id}"),
            path: String::new(),
            dir: false,
            bytes,
            files: 1,
            modified: 0,
            dir_stamp: 0,
        }
    }
    fn layout(sizes: &[u64]) -> MapLayout {
        let nodes = sizes
            .iter()
            .enumerate()
            .map(|(i, &b)| node(i, b))
            .collect::<Vec<_>>();
        MapLayout::new(&nodes.iter().collect::<Vec<_>>())
    }
    fn check_fits(placed: &[Placed], size: Vec2) {
        let bounds = Rect::from_min_size(egui::Pos2::ZERO, size).expand(0.01);
        for (i, t) in placed.iter().enumerate() {
            assert!(
                bounds.contains_rect(t.rect),
                "{:?} outside {size:?}",
                t.rect
            );
            for other in &placed[i + 1..] {
                assert!(!t.rect.shrink(0.01).intersects(other.rect.shrink(0.01)));
            }
        }
        // The tiles and their gaps cover the whole map: nothing is left blank.
        let covered: f32 = placed.iter().map(|t| t.rect.expand(GAP / 2.0).area()).sum();
        let full = (size.x + GAP) * (size.y + GAP);
        assert!((covered - full).abs() / full < 0.001, "{covered} of {full}");
    }
    #[test]
    fn the_inspector_map_fits_its_width_with_readable_tiles() {
        // The owner's D: drive: one huge folder, a few large ones, many small and empty ones.
        let mut sizes = vec![
            882_000_000_000,
            90_000_000_000,
            85_000_000_000,
            36_000_000_000,
        ];
        sizes.extend([19_000_000_000, 13_000_000_000, 6_000_000_000, 1_600_000_000]);
        sizes.extend([
            750_000_000,
            160_000_000,
            46_000_000,
            118_000,
            94_000,
            14_000,
            74,
        ]);
        sizes.extend([0; 12]);
        let l = layout(&sizes);
        for size in [Vec2::new(282.0, 200.0), Vec2::new(900.0, 300.0)] {
            let placed = l.place(size);
            check_fits(&placed, size);
            for t in &placed {
                assert!(
                    t.rect.width() >= 20.0 && t.rect.height() >= 20.0,
                    "{:?} is a sliver at {size:?}",
                    t.rect
                );
            }
            assert!(placed
                .iter()
                .any(|t| matches!(t.target, Target::Group { .. })));
        }
    }
    #[test]
    fn empty_entries_get_no_tile() {
        let l = layout(&[0, 5, 0, 3, 0]);
        assert_eq!(l.empty, 3);
        let placed = l.place(Vec2::new(600.0, 300.0));
        assert_eq!(placed.len(), 2);
        let ids: Vec<_> = placed
            .iter()
            .map(|t| match t.target {
                Target::Entry(i) => l.entries[i].id,
                Target::Group { .. } => usize::MAX,
            })
            .collect();
        assert_eq!(ids, [1, 3]);
        assert_eq!(layout(&[0, 0]).place(Vec2::new(300.0, 200.0)).len(), 0);
        assert_eq!(
            note(0, 3),
            "Area shows size. Click a folder to list its files. 3 empty items are not drawn."
        );
    }
    #[test]
    fn shown_tiles_keep_their_size_ratios() {
        let l = layout(&[75, 25]);
        let placed = l.place(Vec2::new(400.0, 200.0));
        let area = |i: usize| placed[i].rect.expand(GAP / 2.0).area();
        assert!((area(0) / area(1) - 3.0).abs() < 0.01);
        let l = layout(&[1_000_000, 500_000, 250_000, 3, 2, 1]);
        let placed = l.place(Vec2::new(300.0, 200.0));
        assert_eq!(placed.len(), 4);
        assert_eq!(placed[3].target, Target::Group { count: 3, bytes: 6 });
        let area = |i: usize| placed[i].rect.expand(GAP / 2.0).area();
        assert!((area(0) / area(1) - 2.0).abs() < 0.01);
        assert!((area(1) / area(2) - 2.0).abs() < 0.01);
        assert!(area(3) >= (MIN_SIDE + GAP).powi(2) * 0.99);
    }
    #[test]
    fn many_equal_entries_are_capped_and_grouped() {
        let sizes = vec![10; 5000];
        let l = layout(&sizes);
        let size = Vec2::new(900.0, 300.0);
        let placed = l.place(size);
        check_fits(&placed, size);
        assert!(placed.len() <= MAX_TILES + 1);
        let Target::Group { count, bytes } = placed.last().unwrap().target else {
            panic!("the rest is grouped");
        };
        assert_eq!(count + placed.len() - 1, 5000);
        assert_eq!(bytes, count as u64 * 10);
    }
    #[test]
    fn one_leftover_entry_is_drawn_as_itself() {
        let l = layout(&[1_000_000_000, 1]);
        let placed = l.place(Vec2::new(300.0, 200.0));
        assert_eq!(placed.len(), 2);
        assert_eq!(placed[1].target, Target::Entry(1));
        assert!(
            placed[1].rect.width() >= MIN_SIDE - 0.5 || placed[1].rect.height() >= MIN_SIDE - 0.5
        );
    }
    #[test]
    fn the_map_never_scrolls_sideways() {
        let ctx = egui::Context::default();
        let sizes: Vec<u64> = (1..400).map(|i| i * 1_000_000).collect();
        let l = layout(&sizes);
        let mut state = Interaction::default();
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(282.0, 600.0),
                )),
                ..Default::default()
            },
            |ui| {
                show(ui, &l, &mut state, 200.0);
                assert!(ui.min_rect().width() <= 282.0 + 0.01);
            },
        );
        output.textures_delta.clear();
    }
}
