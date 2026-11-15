use crate::scanner::Node;
use egui::{Color32, Vec2};
const MIN_SIDE: f32 = 48.0;
const HEIGHT: f32 = 300.0;
const GAP: f32 = 4.0;
struct Tile {
    id: usize,
    name: String,
    bytes: u64,
    rect: egui::Rect,
    color: Color32,
    category: crate::category::Category,
    column: usize,
}
/// Justified, full-height columns eliminate unused pockets while keeping every entry selectable.
pub struct MapLayout {
    tiles: Vec<Tile>,
    columns: usize,
    width: f32,
}
impl MapLayout {
    #[cfg(test)]
    pub fn new(nodes: &[&Node]) -> Self {
        Self::with_tree(nodes, &[])
    }
    pub fn with_tree(nodes: &[&Node], all: &[Node]) -> Self {
        let categories = crate::category::folders(nodes, all);
        let largest = nodes.iter().map(|n| n.bytes).max().unwrap_or(0).max(1) as f64;
        let mut nodes = nodes.to_vec();
        nodes.sort_by_key(|n| std::cmp::Reverse(n.bytes));
        let areas: Vec<f32> = nodes
            .iter()
            .map(|n| ((n.bytes as f64 / largest * 80_000.0) as f32).max(MIN_SIDE * MIN_SIDE))
            .collect();
        let mut tiles = Vec::with_capacity(nodes.len());
        let mut width = 0.0_f32;
        let mut start = 0;
        let mut columns = 0;
        while start < nodes.len() {
            let mut end = start + 1;
            let mut column = column_geometry(&areas[start..end]);
            // Grow a full-height column while doing so improves rectangle shapes.
            while end < nodes.len() && end - start < 5 {
                let candidate = column_geometry(&areas[start..=end]);
                if candidate.2 > column.2 {
                    break;
                }
                end += 1;
                column = candidate;
            }
            let (column_width, heights, _) = column;
            let mut y = 0.0;
            for (offset, height) in heights.into_iter().enumerate() {
                let i = start + offset;
                let n = nodes[i];
                let rect = egui::Rect::from_min_size(
                    egui::pos2(width, y),
                    Vec2::new(column_width, height),
                );
                tiles.push(Tile {
                    id: n.id,
                    column: columns,
                    name: n.name.clone(),
                    bytes: n.bytes,
                    rect,
                    color: categories[&n.id].color(i),
                    category: categories[&n.id],
                });
                y += height + GAP;
            }
            width += column_width + GAP;
            start = end;
            columns += 1;
        }
        width = (width - GAP).max(0.0);
        tiles.sort_by(|a, b| a.rect.left().total_cmp(&b.rect.left()));
        Self {
            tiles,
            columns,
            width,
        }
    }
}
// Stretch tiles within each column to fill its height, respecting the click-size floor.
fn column_geometry(areas: &[f32]) -> (f32, Vec<f32>, f32) {
    let available = HEIGHT - GAP * (areas.len() - 1) as f32;
    let total: f32 = areas.iter().sum();
    let width = (total / available).max(MIN_SIDE);
    let mut heights = vec![0.0; areas.len()];
    let mut remaining = available;
    let mut weight = total;
    loop {
        let mut fixed = false;
        for (i, area) in areas.iter().enumerate() {
            if heights[i] == 0.0 && remaining * area / weight < MIN_SIDE {
                heights[i] = MIN_SIDE;
                remaining -= MIN_SIDE;
                weight -= area;
                fixed = true;
            }
        }
        if !fixed {
            break;
        }
    }
    for (i, area) in areas.iter().enumerate() {
        if heights[i] == 0.0 {
            heights[i] = remaining * area / weight;
        }
    }
    let worst = heights
        .iter()
        .map(|h| (width / h).max(h / width))
        .fold(1.0_f32, f32::max);
    (width, heights, worst)
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
}
fn menu_key(input: &mut egui::InputState) -> bool {
    input.consume_key(egui::Modifiers::SHIFT, egui::Key::F10)
        || input.consume_key(egui::Modifiers::NONE, egui::Key::Menu)
}
pub fn show(ui: &mut egui::Ui, layout: &MapLayout, state: &mut Interaction) -> Action {
    if layout.tiles.is_empty() {
        crate::theme::weak(ui, "This folder is empty.");
        return Action::default();
    }
    let mut action = Action::default();
    let gaps = GAP * layout.columns.saturating_sub(1) as f32;
    let scale = ((ui.available_width() - gaps) / (layout.width - gaps)).max(1.0);
    let gap_correction = GAP * (scale - 1.0);
    let left = |t: &Tile| t.rect.left() * scale - t.column as f32 * gap_correction;
    let right = |t: &Tile| t.rect.right() * scale - t.column as f32 * gap_correction;
    egui::ScrollArea::horizontal()
        .id_salt("disk-map-packed")
        .auto_shrink([false, true])
        .show_viewport(ui, |ui, viewport| {
            let (r, _) = ui.allocate_exact_size(
                Vec2::new((layout.width - gaps) * scale + gaps, HEIGHT),
                egui::Sense::hover(),
            );
            let first = layout.tiles.partition_point(|t| right(t) < viewport.left());
            for tile in layout.tiles[first..]
                .iter()
                .take_while(|t| left(t) <= viewport.right())
            {
                let rect = egui::Rect::from_min_max(
                    r.min + Vec2::new(left(tile), tile.rect.top()),
                    r.min + Vec2::new(right(tile), tile.rect.bottom()),
                );
                if !ui.is_rect_visible(rect) {
                    continue;
                }
                let response = ui.interact(
                    rect,
                    ui.scope_id().with(("disk-map", tile.id)),
                    egui::Sense::click(),
                );
                if response.clicked() || response.secondary_clicked() || response.has_focus() {
                    state.selected = Some(tile.id);
                    state.focus = Some(response.id);
                }
                if response.clicked() || response.secondary_clicked() {
                    response.request_focus();
                }
                if response.secondary_clicked() {
                    state.menu = Some((
                        tile.id,
                        response.interact_pointer_pos().unwrap_or(rect.center()),
                    ));
                }
                if response.has_focus() && ui.input_mut(menu_key) {
                    state.menu = Some((tile.id, rect.left_top() + Vec2::splat(12.0)));
                }
                ui.painter().rect_filled(rect, 6, tile.color);
                let selected = state.selected == Some(tile.id);
                if selected {
                    let badge = egui::Rect::from_min_max(
                        egui::pos2(rect.left(), rect.bottom() - 23.0),
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
                let painter = ui.painter().with_clip_rect(rect.shrink(5.0));
                let mut job = egui::text::LayoutJob::simple(
                    tile.name.clone(),
                    egui::FontId::proportional(12.0),
                    crate::category::text(tile.color),
                    rect.width() - 14.0,
                );
                job.wrap.max_rows = 1;
                job.wrap.break_anywhere = true;
                let galley = painter.layout_job(job);
                painter.galley(
                    rect.min + Vec2::new(7.0, 7.0),
                    galley,
                    crate::category::text(tile.color),
                );
                painter.text(
                    rect.left_bottom() + Vec2::new(7.0, -7.0),
                    egui::Align2::LEFT_BOTTOM,
                    crate::size(tile.bytes),
                    egui::FontId::proportional(11.0),
                    if selected {
                        crate::category::text(crate::theme::ACCENT)
                    } else {
                        crate::category::text(tile.color)
                    },
                );
                if selected && rect.width() > 135.0 {
                    painter.text(
                        rect.right_bottom() + Vec2::new(-7.0, -7.0),
                        egui::Align2::RIGHT_BOTTOM,
                        "Selected",
                        egui::FontId::proportional(11.0),
                        crate::category::text(crate::theme::ACCENT),
                    );
                }
                if response.clicked() {
                    action.open = Some(tile.id);
                }
                let label = format!(
                    "{} - {} - {}",
                    tile.name,
                    crate::size(tile.bytes),
                    tile.category.label()
                );
                response.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::Role::Button, ui.is_enabled(), &label)
                });
                response.on_hover_text(label);
            }
        });
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
        for tile in &layout.tiles {
            if shown.contains(&tile.category) {
                continue;
            }
            shown.push(tile.category);
            let (r, _) = ui.allocate_exact_size(Vec2::splat(8.0), egui::Sense::hover());
            ui.painter().rect_filled(r, 2, tile.category.color(0));
            crate::theme::caption(ui, tile.category.label());
        }
    });
    let note = if scale == 1.0 && layout.width > ui.available_width() {
        "Scroll sideways for more. Small entries are enlarged. Labels show actual sizes."
    } else {
        "Open a folder from the map. Small entries are enlarged. Labels show actual sizes."
    };
    crate::theme::caption(ui, note);
    action
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
                show(ui, layout, state);
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
    #[test]
    fn packed_tiles_are_clickable_bounded_and_non_overlapping() {
        let nodes = (0..120)
            .map(|i| node(i, if i == 0 { 1_000_000_000_000 } else { i as u64 }))
            .collect::<Vec<_>>();
        let layout = MapLayout::new(&nodes.iter().collect::<Vec<_>>());
        for (i, t) in layout.tiles.iter().enumerate() {
            assert!(t.rect.width() >= MIN_SIDE - 0.01 && t.rect.height() >= MIN_SIDE - 0.01);
            assert!(t.rect.top() >= 0.0 && t.rect.bottom() <= HEIGHT + 0.01);
            for other in &layout.tiles[i + 1..] {
                assert!(!t.rect.intersects(other.rect));
            }
        }
        assert!(layout.tiles.iter().any(|t| t.rect.top() > 0.0));
        assert!(layout.width < 120.0 * MIN_SIDE / 2.0);
    }
    #[test]
    fn area_ratios_are_preserved_above_the_size_floor() {
        let nodes = [node(1, 75), node(2, 25)];
        let layout = MapLayout::new(&nodes.iter().collect::<Vec<_>>());
        let area = |id| {
            layout
                .tiles
                .iter()
                .find(|t| t.id == id)
                .unwrap()
                .rect
                .area()
        };
        assert!((area(1) / area(2) - 3.0).abs() < 0.001);
    }
    #[test]
    fn columns_fill_the_entire_map_without_pockets() {
        for count in [1, 2, 6, 37, 120] {
            let nodes = (0..count)
                .map(|i| node(i, ((i * 7919) % 10007) as u64))
                .collect::<Vec<_>>();
            let layout = MapLayout::new(&nodes.iter().collect::<Vec<_>>());
            let mut x = 0.0;
            while x < layout.width - 0.01 {
                let mut column: Vec<_> = layout
                    .tiles
                    .iter()
                    .filter(|t| (t.rect.left() - x).abs() < 0.01)
                    .collect();
                assert!(!column.is_empty());
                column.sort_by(|a, b| a.rect.top().total_cmp(&b.rect.top()));
                let mut y = 0.0;
                let right = column[0].rect.right();
                for tile in column {
                    assert!((tile.rect.top() - y).abs() < 0.01);
                    assert!((tile.rect.right() - right).abs() < 0.01);
                    assert!(
                        tile.rect.width() >= MIN_SIDE - 0.01
                            && tile.rect.height() >= MIN_SIDE - 0.01
                    );
                    y = tile.rect.bottom() + GAP;
                }
                assert!((y - GAP - HEIGHT).abs() < 0.01);
                x = right + GAP;
            }
            assert!((x - GAP - layout.width).abs() < 0.01);
        }
    }
    #[test]
    fn all_entries_including_empty_ones_remain_accessible() {
        let nodes = (0..1200).map(|i| node(i, 0)).collect::<Vec<_>>();
        let layout = MapLayout::new(&nodes.iter().collect::<Vec<_>>());
        assert_eq!(layout.tiles.len(), 1200);
        let mut ids = layout.tiles.iter().map(|t| t.id).collect::<Vec<_>>();
        ids.sort_unstable();
        assert_eq!(ids, (0..1200).collect::<Vec<_>>());
    }
}
