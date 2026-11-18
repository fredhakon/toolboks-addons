//! Layout maths for the Space explorer: a nested squarified treemap of one folder ("the
//! focus"), drawn through a camera. Zooming does not scale pixels: the focus is laid out again
//! at the zoomed size, so deeper folders get their own tiles instead of turning into mush. Only
//! what is on screen is laid out, files under about 2 px are grouped into one tile per folder,
//! and the tile count is capped so a scan of a million files stays fast.
//!
//! Everything is in f64 until the final on-screen rectangles: at deep zoom the virtual map is
//! billions of pixels wide.
use super::model::Model;
use egui::{Pos2, Rect, Vec2};

/// The border around a folder's contents, by depth below the focus.
pub const GAPS: [f64; 5] = [3.0, 2.0, 1.4, 1.0, 0.8];
/// Files that would be smaller than about 2 x 2 px are grouped.
pub const MIN_AREA: f64 = 4.0;
/// More tiles than this are grouped; the status bar says so.
pub const MAX_TILES: usize = 30_000;
/// The hidden strip is never thinner than this, so its label can be read.
const HIDDEN_MIN: f64 = 18.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct R64 {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}
impl R64 {
    pub fn new(x0: f64, y0: f64, w: f64, h: f64) -> Self {
        Self {
            x0,
            y0,
            x1: x0 + w,
            y1: y0 + h,
        }
    }
    pub fn w(&self) -> f64 {
        self.x1 - self.x0
    }
    pub fn h(&self) -> f64 {
        self.y1 - self.y0
    }
    fn area(&self) -> f64 {
        self.w().max(0.0) * self.h().max(0.0)
    }
    fn shrink(&self, by: f64) -> Self {
        Self {
            x0: self.x0 + by,
            y0: self.y0 + by,
            x1: self.x1 - by,
            y1: self.y1 - by,
        }
    }
    fn intersects(&self, o: &R64) -> bool {
        self.x0 < o.x1 && o.x0 < self.x1 && self.y0 < o.y1 && o.y0 < self.y1
    }
    /// On-screen rectangle; far-off edges are clamped so f32 stays exact near the view.
    fn screen(&self, view: Vec2) -> Rect {
        let c = |v: f64, max: f32| v.clamp(-64.0, max as f64 + 64.0) as f32;
        Rect::from_min_max(
            egui::pos2(c(self.x0, view.x), c(self.y0, view.y)),
            egui::pos2(c(self.x1, view.x), c(self.y1, view.y)),
        )
    }
}

/// What part of the focus folder is on screen. `scale` 1 shows all of it; `x` and `y` are the
/// top-left corner of the view as a fraction of the focus.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    pub scale: f64,
    pub x: f64,
    pub y: f64,
}
impl Default for Camera {
    fn default() -> Self {
        Self {
            scale: 1.0,
            x: 0.0,
            y: 0.0,
        }
    }
}
impl Camera {
    /// Deep enough for a one-byte file to fill the map.
    pub fn max_scale(focus_bytes: u64) -> f64 {
        (focus_bytes as f64).sqrt().clamp(1.0, 1e8)
    }
    /// Keeps the view inside the focus: no zooming out past it, no panning off it.
    pub fn clamp(&mut self, max: f64) {
        if !self.scale.is_finite() {
            self.scale = 1.0;
        }
        self.scale = self.scale.clamp(1.0, max.max(1.0));
        let span = 1.0 / self.scale;
        let fix = |v: f64| {
            if v.is_finite() {
                v.clamp(0.0, 1.0 - span)
            } else {
                0.0
            }
        };
        self.x = fix(self.x);
        self.y = fix(self.y);
    }
    /// Zooms by `factor` keeping the point under `at` (view coordinates) in place.
    pub fn zoom_at(&mut self, factor: f64, at: Pos2, view: Vec2, max: f64) {
        if !(factor.is_finite() && factor > 0.0) || view.x <= 0.0 || view.y <= 0.0 {
            return;
        }
        let (w, h) = (view.x as f64, view.y as f64);
        let ux = self.x + at.x as f64 / (w * self.scale);
        let uy = self.y + at.y as f64 / (h * self.scale);
        self.scale = (self.scale * factor).clamp(1.0, max.max(1.0));
        self.x = ux - at.x as f64 / (w * self.scale);
        self.y = uy - at.y as f64 / (h * self.scale);
        self.clamp(max);
    }
    /// Moves the map with the pointer (or the keyboard) by `delta` points.
    pub fn pan(&mut self, delta: Vec2, view: Vec2, max: f64) {
        if view.x <= 0.0 || view.y <= 0.0 {
            return;
        }
        self.x -= delta.x as f64 / (view.x as f64 * self.scale);
        self.y -= delta.y as f64 / (view.y as f64 * self.scale);
        self.clamp(max);
    }
    /// The whole focus at this zoom, in view coordinates.
    pub fn world(&self, view: Vec2) -> R64 {
        let (w, h) = (view.x as f64 * self.scale, view.y as f64 * self.scale);
        R64::new(-self.x * w, -self.y * h, w, h)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileKind {
    File(u32),
    /// Entries of `folder` too small to draw one by one.
    Group {
        folder: u32,
        count: u64,
    },
    /// Space the filters hide under the focus.
    Hidden {
        files: u64,
    },
}
#[derive(Clone, Copy, Debug)]
pub struct Tile {
    pub rect: Rect,
    pub kind: TileKind,
    pub bytes: u64,
}
#[derive(Clone, Copy, Debug)]
pub struct Block {
    pub rect: Rect,
    pub id: u32,
    pub depth: u8,
}
/// What a point on the map is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Tile(usize),
    /// A folder's border.
    Folder(u32),
}
#[derive(Default)]
pub struct Layout {
    pub view: Vec2,
    pub tiles: Vec<Tile>,
    /// Folders, parents before their children.
    pub blocks: Vec<Block>,
    /// The tile limit was reached and the rest was grouped.
    pub capped: bool,
}

/// Lays out `focus` as seen through `camera` in a map of `view` points.
pub fn layout(model: &Model, focus: usize, camera: &Camera, view: Vec2) -> Layout {
    let mut out = Layout {
        view,
        ..Default::default()
    };
    if view.x <= 0.0 || view.y <= 0.0 || focus >= model.nodes.len() {
        return out;
    }
    let screen = R64::new(0.0, 0.0, view.x as f64, view.y as f64);
    let mut world = camera.world(view);
    let shown = model.shown[focus];
    let (hidden, hidden_files) = model.hidden(focus);
    if hidden > 0 {
        let share = hidden as f64 / (hidden as f64 + shown as f64);
        let strip = if shown == 0 {
            world.h()
        } else {
            (world.h() * share).max(HIDDEN_MIN).min(world.h() / 2.0)
        };
        let r = R64 {
            y0: world.y1 - strip,
            ..world
        };
        world.y1 = r.y0;
        if r.intersects(&screen) {
            out.tiles.push(Tile {
                rect: r.screen(view),
                kind: TileKind::Hidden {
                    files: hidden_files,
                },
                bytes: hidden,
            });
        }
    }
    lay(model, focus, world, 0, &screen, &mut out);
    out
}

fn lay(model: &Model, id: usize, rect: R64, depth: u8, screen: &R64, out: &mut Layout) {
    let kids = model.children(id);
    let total = model.shown[id] as f64;
    if kids.is_empty() || total <= 0.0 || rect.area() <= 0.0 {
        return;
    }
    let scale = rect.area() / total;
    let mut cut = kids
        .iter()
        .position(|&k| model.shown[k as usize] as f64 * scale < MIN_AREA)
        .unwrap_or(kids.len());
    let left = MAX_TILES.saturating_sub(out.tiles.len());
    if cut > left {
        cut = left;
        out.capped = true;
    }
    // One leftover file is drawn as itself, not as "1 small file".
    if cut + 1 == kids.len() && !model.nodes[kids[cut] as usize].dir {
        cut += 1;
    }
    let rest = &kids[cut..];
    let mut weights: Vec<f64> = kids[..cut]
        .iter()
        .map(|&k| model.shown[k as usize] as f64)
        .collect();
    let rest_bytes: u64 = rest.iter().map(|&k| model.shown[k as usize]).sum();
    if !rest.is_empty() {
        weights.push(rest_bytes as f64);
    }
    for (i, r) in squarify(&weights, rect).into_iter().enumerate() {
        if !r.intersects(screen) {
            continue;
        }
        let Some(&child) = kids[..cut].get(i) else {
            out.tiles.push(Tile {
                rect: r.screen(out.view),
                kind: TileKind::Group {
                    folder: id as u32,
                    count: rest.iter().map(|&k| model.shown_files[k as usize]).sum(),
                },
                bytes: rest_bytes,
            });
            continue;
        };
        let c = child as usize;
        if !model.nodes[c].dir {
            out.tiles.push(Tile {
                rect: r.screen(out.view),
                kind: TileKind::File(child),
                bytes: model.shown[c],
            });
            continue;
        }
        out.blocks.push(Block {
            rect: r.screen(out.view),
            id: child,
            depth,
        });
        let inner = r.shrink(GAPS[(depth as usize).min(GAPS.len() - 1)]);
        if inner.w() < 1.0 || inner.h() < 1.0 || out.tiles.len() >= MAX_TILES {
            out.capped |= out.tiles.len() >= MAX_TILES;
            out.tiles.push(Tile {
                rect: r.screen(out.view),
                kind: TileKind::Group {
                    folder: child,
                    count: model.shown_files[c],
                },
                bytes: model.shown[c],
            });
        } else {
            lay(model, c, inner, depth.saturating_add(1), screen, out);
        }
    }
}

/// Squarified treemap (Bruls, Huizing and van Wijk) of `weights` (largest first) in `rect`.
/// Rows run along the shorter side and grow while that keeps their tiles closer to square.
/// The rectangles fill `rect` exactly, in the order of `weights`.
pub fn squarify(weights: &[f64], mut rect: R64) -> Vec<R64> {
    let total: f64 = weights.iter().sum();
    let mut out = Vec::with_capacity(weights.len());
    if weights.is_empty() || total.is_nan() || total <= 0.0 || rect.area() <= 0.0 {
        return vec![R64::new(rect.x0, rect.y0, 0.0, 0.0); weights.len()];
    }
    let scale = rect.area() / total;
    let mut i = 0;
    while i < weights.len() {
        let wide = rect.w() >= rect.h();
        let short = rect.w().min(rect.h()).max(0.0);
        let long = rect.w().max(rect.h()).max(0.0);
        let s2 = short * short;
        let worst = |sum: f64, max: f64, min: f64| {
            if sum <= 0.0 || min <= 0.0 || s2 <= 0.0 {
                f64::INFINITY
            } else {
                (s2 * max / (sum * sum)).max(sum * sum / (s2 * min))
            }
        };
        let first = weights[i] * scale;
        let (mut sum, mut max, mut min) = (first, first, first);
        let mut best = worst(sum, max, min);
        let mut j = i + 1;
        while j < weights.len() {
            let a = weights[j] * scale;
            let next = worst(sum + a, max.max(a), min.min(a));
            if next > best {
                break;
            }
            (sum, max, min, best) = (sum + a, max.max(a), min.min(a), next);
            j += 1;
        }
        // The last row takes what is left, so rounding never leaves a gap.
        let thickness = if j == weights.len() || short <= 0.0 {
            long
        } else {
            (sum / short).min(long)
        };
        let mut along = 0.0;
        for (k, weight) in weights.iter().enumerate().take(j).skip(i) {
            let length = if k + 1 == j {
                (short - along).max(0.0)
            } else if thickness > 0.0 {
                weight * scale / thickness
            } else {
                0.0
            };
            out.push(if wide {
                R64::new(rect.x0, rect.y0 + along, thickness, length)
            } else {
                R64::new(rect.x0 + along, rect.y0, length, thickness)
            });
            along += length;
        }
        if wide {
            rect.x0 += thickness;
        } else {
            rect.y0 += thickness;
        }
        i = j;
    }
    out
}

impl Layout {
    /// The tile under `p`, else the deepest folder whose border is under it.
    pub fn hit(&self, p: Pos2) -> Option<Hit> {
        if let Some(i) = self.tiles.iter().position(|t| t.rect.contains(p)) {
            return Some(Hit::Tile(i));
        }
        self.blocks
            .iter()
            .rev()
            .find(|b| b.rect.contains(p))
            .map(|b| Hit::Folder(b.id))
    }
    /// The deepest folder that fills the whole map (the focus when none does).
    pub fn view_folder(&self, focus: usize) -> usize {
        let screen = Rect::from_min_size(Pos2::ZERO, self.view).shrink(0.5);
        self.blocks
            .iter()
            .rev()
            .find(|b| b.rect.contains_rect(screen))
            .map_or(focus, |b| b.id as usize)
    }
    /// The nearest tile from `from` in direction `dir` (one of the four arrows).
    pub fn neighbour(&self, from: usize, dir: Vec2) -> Option<usize> {
        let a = self.tiles.get(from)?.rect;
        let screen = Rect::from_min_size(Pos2::ZERO, self.view);
        self.tiles
            .iter()
            .enumerate()
            .filter(|(i, t)| *i != from && t.rect.intersects(screen))
            .filter_map(|(i, t)| {
                let b = t.rect;
                // Beyond `a`'s edge in that direction, measured edge to edge.
                let ahead = if dir.x > 0.0 {
                    b.left() - a.right()
                } else if dir.x < 0.0 {
                    a.left() - b.right()
                } else if dir.y > 0.0 {
                    b.top() - a.bottom()
                } else {
                    a.top() - b.bottom()
                };
                if ahead < -0.5 {
                    return None;
                }
                let off = if dir.x != 0.0 {
                    (b.center().y - a.center().y).abs() - (a.height() + b.height()) / 2.0
                } else {
                    (b.center().x - a.center().x).abs() - (a.width() + b.width()) / 2.0
                };
                // Tiles side by side with this one come first, nearest first, then the
                // one sharing the longest edge.
                let beside = off < 0.0;
                let score = ahead.max(0.0) + if beside { 0.0 } else { 3.0 * off };
                Some((i, beside, score, off))
            })
            .min_by(|x, y| {
                y.1.cmp(&x.1)
                    .then(x.2.total_cmp(&y.2))
                    .then(x.3.total_cmp(&y.3))
            })
            .map(|(i, ..)| i)
    }
    pub fn find(&self, kind: TileKind) -> Option<usize> {
        self.tiles.iter().position(|t| t.kind == kind)
    }
}

/// How much text a file tile can hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Label {
    None,
    Name,
    NameAndSize,
}
pub fn tile_label(size: Vec2) -> Label {
    if size.x >= 64.0 && size.y >= 34.0 {
        Label::NameAndSize
    } else if size.x >= 48.0 && size.y >= 16.0 {
        Label::Name
    } else {
        Label::None
    }
}
/// Whether a folder block gets its name chip: top-level blocks need less room than deeper
/// ones, so the map does not fill with chips.
pub fn chip_fits(depth: u8, size: Vec2) -> bool {
    if depth == 0 {
        size.x > 70.0 && size.y > 30.0
    } else {
        size.x > 170.0 && size.y > 80.0
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::{
        tests::{node, totals},
        Filters, Model,
    };
    use super::*;
    use std::sync::Arc;
    const VIEW: Vec2 = Vec2::new(900.0, 600.0);

    fn flat(sizes: &[u64]) -> Model {
        let mut nodes = vec![node(0, None, "root", true, 0)];
        for (i, &b) in sizes.iter().enumerate() {
            nodes.push(node(i + 1, Some(0), &format!("f{i}.bin"), false, b));
        }
        Model::build(Arc::new(totals(nodes)), 1, Filters::default(), 0)
    }
    /// root / a (folder: 3 files) / b (folder: a folder with 2 files) / c.bin
    fn nested() -> Model {
        Model::build(
            Arc::new(totals(vec![
                node(0, None, "root", true, 0),
                node(1, Some(0), "a", true, 0),
                node(2, Some(1), "a1.mkv", false, 4000),
                node(3, Some(1), "a2.mkv", false, 2000),
                node(4, Some(1), "a3.mkv", false, 1000),
                node(5, Some(0), "b", true, 0),
                node(6, Some(5), "inner", true, 0),
                node(7, Some(6), "b1.zip", false, 1500),
                node(8, Some(6), "b2.zip", false, 500),
                node(9, Some(0), "c.bin", false, 1000),
            ])),
            1,
            Filters::default(),
            0,
        )
    }
    fn assert_sane(l: &Layout) {
        let bounds = Rect::from_min_size(Pos2::ZERO, l.view).expand(0.01);
        for (i, t) in l.tiles.iter().enumerate() {
            for v in [t.rect.min.x, t.rect.min.y, t.rect.max.x, t.rect.max.y] {
                assert!(v.is_finite(), "NaN in {t:?}");
            }
            assert!(t.rect.width() >= 0.0 && t.rect.height() >= 0.0);
            let on_screen = t.rect.intersect(bounds);
            for o in &l.tiles[i + 1..] {
                let both = on_screen.intersect(o.rect.intersect(bounds));
                assert!(
                    both.width() <= 0.01 || both.height() <= 0.01,
                    "{:?} overlaps {:?}",
                    t.rect,
                    o.rect
                );
            }
        }
    }
    #[test]
    fn squarify_fills_the_rect_in_proportion() {
        let w = [50.0, 25.0, 12.5, 12.5];
        let r = squarify(&w, R64::new(0.0, 0.0, 400.0, 200.0));
        let total: f64 = r.iter().map(|r| r.area()).sum();
        assert!((total - 80_000.0).abs() < 1e-6);
        for (a, b) in r.iter().zip(w) {
            assert!((a.area() / 80_000.0 - b / 100.0).abs() < 1e-9);
        }
        assert!(squarify(&[0.0, 0.0], R64::new(0.0, 0.0, 10.0, 10.0))
            .iter()
            .all(|r| r.area() == 0.0));
        assert!(squarify(&[1.0], R64::new(0.0, 0.0, 0.0, 10.0))[0].area() == 0.0);
    }
    #[test]
    fn tile_area_is_proportional_to_size_without_overlap_or_nan() {
        let m = flat(&[8000, 4000, 2000, 1000, 1000]);
        let l = layout(&m, 0, &Camera::default(), VIEW);
        assert_sane(&l);
        assert_eq!(l.tiles.len(), 5);
        let area = |i: usize| l.tiles[i].rect.area() as f64;
        for i in 1..5 {
            let want = l.tiles[0].bytes as f64 / l.tiles[i].bytes as f64;
            assert!((area(0) / area(i) - want).abs() < 1e-3, "{i}");
        }
        let total: f64 = (0..5).map(area).sum();
        assert!((total - 900.0 * 600.0).abs() < 1.0);
    }
    #[test]
    fn folders_nest_with_their_files_inside_a_border() {
        let m = nested();
        let l = layout(&m, 0, &Camera::default(), VIEW);
        assert_sane(&l);
        let block = |id: u32| l.blocks.iter().find(|b| b.id == id).unwrap();
        let (a, inner) = (block(1), block(6));
        assert_eq!((a.depth, block(5).depth, inner.depth), (0, 0, 1));
        let file = |id: u32| l.tiles[l.find(TileKind::File(id)).unwrap()].rect;
        for f in [2, 3, 4] {
            assert!(a.rect.shrink(GAPS[0] as f32 - 0.01).contains_rect(file(f)));
        }
        for f in [7, 8] {
            assert!(inner
                .rect
                .shrink(GAPS[1] as f32 - 0.01)
                .contains_rect(file(f)));
        }
        // A click on the border is the folder; on a file, the file.
        let border = a.rect.min + Vec2::splat(1.0);
        assert_eq!(l.hit(border), Some(Hit::Folder(1)));
        let inner_border = inner.rect.min + Vec2::splat(1.0);
        assert_eq!(l.hit(inner_border), Some(Hit::Folder(6)));
        let c = l.find(TileKind::File(9)).unwrap();
        assert_eq!(l.hit(l.tiles[c].rect.center()), Some(Hit::Tile(c)));
        assert_eq!(l.hit(egui::pos2(-5.0, -5.0)), None);
    }
    #[test]
    fn tiny_files_are_grouped_per_folder_and_zoom_brings_them_back() {
        // 20,000 one-byte files of about 2.5 square points each next to one big file.
        let mut sizes = vec![200_000];
        sizes.extend([1; 20_000]);
        let m = flat(&sizes);
        let l = layout(&m, 0, &Camera::default(), VIEW);
        assert_sane(&l);
        assert_eq!(l.tiles.len(), 2);
        assert_eq!(
            l.tiles[1].kind,
            TileKind::Group {
                folder: 0,
                count: 20_000
            }
        );
        assert_eq!(l.tiles[1].bytes, 20_000);
        // Zoomed into the group, the small files get their own tiles.
        let corner = l.tiles[1].rect.center();
        let mut cam = Camera::default();
        let max = Camera::max_scale(m.shown[0]);
        for _ in 0..4 {
            cam.zoom_at(1.5, corner, VIEW, max);
        }
        let zoomed = layout(&m, 0, &cam, VIEW);
        assert_sane(&zoomed);
        assert!(
            zoomed
                .tiles
                .iter()
                .filter(|t| matches!(t.kind, TileKind::File(id) if id > 1))
                .count()
                > 1000,
            "zooming lays the folder out again with the small files"
        );
        assert!(!zoomed
            .tiles
            .iter()
            .any(|t| matches!(t.kind, TileKind::Group { .. })));
    }
    #[test]
    fn one_leftover_file_is_drawn_as_itself() {
        let m = flat(&[1_000_000_000, 1]);
        let l = layout(&m, 0, &Camera::default(), VIEW);
        assert_eq!(l.tiles[1].kind, TileKind::File(2));
    }
    #[test]
    fn a_million_files_are_capped_and_grouped() {
        // Each would be about 5 px: more tiles than the limit.
        let m = flat(&vec![10; 100_000]);
        let l = layout(&m, 0, &Camera::default(), VIEW);
        assert!(l.capped);
        assert!(l.tiles.len() <= MAX_TILES + 1);
        let grouped: u64 = l
            .tiles
            .iter()
            .map(|t| match t.kind {
                TileKind::Group { count, .. } => count,
                _ => 1,
            })
            .sum();
        assert_eq!(grouped, 100_000, "nothing is lost");
    }
    #[test]
    fn only_tiles_on_screen_are_laid_out() {
        let m = flat(&[100; 400]);
        let full = layout(&m, 0, &Camera::default(), VIEW);
        let mut cam = Camera::default();
        cam.zoom_at(4.0, Pos2::ZERO, VIEW, 100.0);
        let zoomed = layout(&m, 0, &cam, VIEW);
        assert_sane(&zoomed);
        assert!(zoomed.tiles.len() < full.tiles.len() / 2);
        let screen = Rect::from_min_size(Pos2::ZERO, VIEW);
        assert!(zoomed.tiles.iter().all(|t| t.rect.intersects(screen)));
    }
    #[test]
    fn camera_zooms_toward_the_pointer_and_pans_stay_inside() {
        let mut cam = Camera::default();
        let p = egui::pos2(225.0, 150.0);
        let max = 1000.0;
        cam.zoom_at(2.0, p, VIEW, max);
        // The point under the pointer stays under it.
        assert!((cam.x + 225.0 / (900.0 * 2.0) - 0.25).abs() < 1e-9);
        assert!((cam.y + 150.0 / (600.0 * 2.0) - 0.25).abs() < 1e-9);
        cam.pan(Vec2::new(10_000.0, 10_000.0), VIEW, max);
        assert_eq!((cam.x, cam.y), (0.0, 0.0));
        cam.pan(Vec2::new(-1e9, -1e9), VIEW, max);
        assert!((cam.x - 0.5).abs() < 1e-12 && (cam.y - 0.5).abs() < 1e-12);
        cam.zoom_at(0.0001, p, VIEW, max);
        assert_eq!(
            cam,
            Camera::default(),
            "never smaller than the whole folder"
        );
        cam.zoom_at(1e12, p, VIEW, max);
        assert_eq!(cam.scale, max);
        cam.zoom_at(f64::NAN, p, VIEW, max);
        assert!(cam.scale.is_finite() && cam.x.is_finite());
        let mut bad = Camera {
            scale: f64::NAN,
            x: f64::INFINITY,
            y: -3.0,
        };
        bad.clamp(max);
        assert_eq!(bad, Camera::default());
    }
    #[test]
    fn deep_zoom_finds_the_folder_that_fills_the_map() {
        let m = nested();
        let l = layout(&m, 0, &Camera::default(), VIEW);
        assert_eq!(l.view_folder(0), 0);
        let a = l.blocks.iter().find(|b| b.id == 1).unwrap().rect;
        let mut cam = Camera::default();
        for _ in 0..8 {
            cam.zoom_at(2.0, a.center(), VIEW, 1e6);
        }
        assert_eq!(layout(&m, 0, &cam, VIEW).view_folder(0), 1);
    }
    #[test]
    fn hidden_space_is_a_strip_so_totals_add_up() {
        let nodes = Arc::new(totals(vec![
            node(0, None, "root", true, 0),
            node(1, Some(0), "big.bin", false, 9000),
            node(2, Some(0), "small.bin", false, 1000),
        ]));
        let filters = Filters {
            hide_below: 5000,
            ..Default::default()
        };
        let m = Model::build(nodes, 1, filters, 0);
        let l = layout(&m, 0, &Camera::default(), VIEW);
        assert_sane(&l);
        let strip = l
            .tiles
            .iter()
            .find(|t| matches!(t.kind, TileKind::Hidden { files: 1 }))
            .unwrap();
        assert_eq!(strip.bytes, 1000);
        assert!((strip.rect.height() - 60.0).abs() < 0.01);
        let drawn: u64 = l.tiles.iter().map(|t| t.bytes).sum();
        assert_eq!(drawn, 10_000);
    }
    #[test]
    fn arrows_move_to_the_neighbouring_tile() {
        let m = flat(&[100; 4]);
        let l = layout(&m, 0, &Camera::default(), VIEW);
        // Four equal tiles in a 3:2 view: a 2 x 2 grid.
        let at = |x: f32, y: f32| match l.hit(egui::pos2(x, y)) {
            Some(Hit::Tile(i)) => i,
            other => panic!("{other:?}"),
        };
        let (tl, tr, bl, br) = (at(10., 10.), at(890., 10.), at(10., 590.), at(890., 590.));
        assert_eq!(l.neighbour(tl, Vec2::X), Some(tr));
        assert_eq!(l.neighbour(tl, Vec2::Y), Some(bl));
        assert_eq!(l.neighbour(br, -Vec2::X), Some(bl));
        assert_eq!(l.neighbour(br, -Vec2::Y), Some(tr));
        assert_eq!(l.neighbour(tl, -Vec2::X), None);
    }
    #[test]
    fn labels_appear_as_tiles_grow() {
        assert_eq!(tile_label(Vec2::new(64.0, 34.0)), Label::NameAndSize);
        assert_eq!(tile_label(Vec2::new(63.0, 40.0)), Label::Name);
        assert_eq!(tile_label(Vec2::new(200.0, 33.0)), Label::Name);
        assert_eq!(tile_label(Vec2::new(48.0, 16.0)), Label::Name);
        assert_eq!(tile_label(Vec2::new(47.0, 100.0)), Label::None);
        assert_eq!(tile_label(Vec2::new(100.0, 15.0)), Label::None);
        assert!(chip_fits(0, Vec2::new(71.0, 31.0)));
        assert!(!chip_fits(0, Vec2::new(70.0, 31.0)));
        assert!(!chip_fits(1, Vec2::new(150.0, 100.0)));
        assert!(chip_fits(2, Vec2::new(171.0, 81.0)));
        // Zooming in reveals names: a tile with no label at fit has one at 4x.
        let mut sizes = vec![1_000_000];
        sizes.extend([300; 200]);
        let m = flat(&sizes);
        let fit = layout(&m, 0, &Camera::default(), VIEW);
        let small = fit.find(TileKind::File(2)).unwrap();
        assert_eq!(tile_label(fit.tiles[small].rect.size()), Label::None);
        let mut cam = Camera::default();
        cam.zoom_at(6.0, fit.tiles[small].rect.center(), VIEW, 1e4);
        let zoomed = layout(&m, 0, &cam, VIEW);
        let t = zoomed.find(TileKind::File(2)).unwrap();
        assert_ne!(tile_label(zoomed.tiles[t].rect.size()), Label::None);
    }
}
