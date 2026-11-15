//! A bounded, time-based peak history. UI observations never predict future audio.
use egui::{pos2, vec2, Align2, Color32, FontId, Rect, Stroke};
use std::collections::VecDeque;
use std::time::Duration;
use toolboks_shared::ui as look;

pub const BARS: usize = 40;
pub const SLICE: Duration = Duration::from_millis(250);
pub const FRAME: Duration = Duration::from_nanos(33_333_333);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Good,
    Close,
    Clipping,
}
impl Level {
    pub fn of(db: f32) -> Self {
        if db >= -3.0 {
            Self::Clipping
        } else if db >= -9.0 {
            Self::Close
        } else {
            Self::Good
        }
    }
    pub fn color(self) -> Color32 {
        match self {
            Self::Good => look::GREEN,
            Self::Close => look::AMBER,
            Self::Clipping => look::RED,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Good => "Good",
            Self::Close => "Close to clipping",
            Self::Clipping => "Clipping",
        }
    }
}

pub struct Timeline {
    completed: VecDeque<f32>,
    current: f32,
    bucket: u64,
    last_close: Option<Duration>,
    last_clip: Option<Duration>,
    pub active: bool,
}
impl Default for Timeline {
    fn default() -> Self {
        Self {
            completed: VecDeque::with_capacity(BARS),
            current: -60.0,
            bucket: 0,
            last_close: None,
            last_clip: None,
            active: false,
        }
    }
}
impl Timeline {
    pub fn reset(&mut self, active: bool) {
        *self = Self {
            active,
            ..Default::default()
        };
    }
    pub fn observe(&mut self, elapsed: Duration, peaks: [f32; 2]) {
        if !self.active {
            return;
        }
        let bucket = (elapsed.as_secs_f64() / SLICE.as_secs_f64()).floor() as u64;
        let steps = bucket.saturating_sub(self.bucket);
        if steps > 0 {
            if steps > BARS as u64 {
                self.completed.clear();
            }
            for step in 0..steps.min(BARS as u64) {
                if self.completed.len() == BARS {
                    self.completed.pop_front();
                }
                self.completed
                    .push_back(if step == 0 && steps <= BARS as u64 {
                        self.current
                    } else {
                        -60.0
                    });
            }
            self.current = -60.0;
            self.bucket = bucket;
        }
        let peak = peaks
            .into_iter()
            .filter(|v| v.is_finite())
            .fold(-60.0, f32::max);
        self.current = self.current.max(peak);
        match Level::of(peak) {
            Level::Close => self.last_close = Some(elapsed),
            Level::Clipping => self.last_clip = Some(elapsed),
            Level::Good => {}
        }
    }
    pub fn phase(&self, elapsed: Duration) -> f32 {
        ((elapsed.as_secs_f64() / SLICE.as_secs_f64()) - self.bucket as f64).clamp(0.0, 1.0) as f32
    }
    pub fn event_text(&self, elapsed: Duration, clipping: bool) -> String {
        let (event, word) = if clipping {
            (self.last_clip, "Clipped")
        } else {
            (self.last_close, "Close to clipping")
        };
        match event {
            Some(at) => format!("{word} {} s ago", elapsed.saturating_sub(at).as_secs()),
            None => {
                if clipping {
                    "No clipping yet".into()
                } else {
                    "No close-to-clipping level yet".into()
                }
            }
        }
    }
    pub fn draw(&self, ui: &mut egui::Ui, height: f32, elapsed: Duration) {
        let (area, response) = ui.allocate_exact_size(
            vec2(ui.available_width(), height.max(140.0)),
            egui::Sense::hover(),
        );
        response.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::Role::Image,
                true,
                "Input level history, newest on the right, last 10 seconds",
            )
        });
        let plot = Rect::from_min_max(area.min + vec2(28.0, 8.0), area.max - vec2(2.0, 26.0));
        let p = ui.painter();
        let font = FontId::proportional(11.0);
        for db in [0.0, -6.0, -12.0, -24.0, -42.0, -60.0] {
            let y = plot.bottom() - (db + 60.0) / 60.0 * plot.height();
            p.text(
                pos2(plot.left() - 8.0, y),
                Align2::RIGHT_CENTER,
                format!("{db:.0}"),
                font.clone(),
                look::MUTED,
            );
            if db != -6.0 {
                p.line_segment(
                    [pos2(plot.left(), y), pos2(plot.right(), y)],
                    Stroke::new(1.0, look::BORDER),
                );
            }
        }
        for seconds in [10, 8, 6, 4, 2, 0] {
            let x = plot.right() - seconds as f32 / 10.0 * plot.width();
            p.line_segment(
                [pos2(x, plot.top()), pos2(x, plot.bottom())],
                Stroke::new(1.0, look::BORDER),
            );
            let align = if seconds == 10 {
                Align2::LEFT_TOP
            } else if seconds == 0 {
                Align2::RIGHT_TOP
            } else {
                Align2::CENTER_TOP
            };
            p.text(
                pos2(x, plot.bottom() + 8.0),
                align,
                match seconds {
                    10 => "10 s ago".into(),
                    0 => "now".into(),
                    s => format!("{s} s"),
                },
                font.clone(),
                if seconds == 0 {
                    look::DATA_MEMORY
                } else {
                    look::MUTED
                },
            );
        }
        let clipped = p.with_clip_rect(plot.intersect(ui.clip_rect()));
        let pitch = plot.width() / BARS as f32;
        let gap = 5.0_f32.min(pitch * 0.3);
        let phase = self.phase(elapsed);
        // The current slice grows backwards from 'now'. Completed slices glide left.
        for (i, db) in self.completed.iter().rev().enumerate() {
            let right = plot.right() - (i as f32 + phase) * pitch - gap * 0.5;
            draw_bar(&clipped, plot, right - pitch + gap, right, *db);
        }
        if self.active && phase > 0.0 {
            draw_bar(
                &clipped,
                plot,
                plot.right() - gap * 0.5 - (phase * pitch - gap).max(1.0),
                plot.right() - gap * 0.5,
                self.current,
            );
        }
        let warning_y = plot.top() + plot.height() * 0.1;
        let mut x = plot.left();
        while x < plot.right() {
            p.line_segment(
                [
                    pos2(x, warning_y),
                    pos2((x + 4.0).min(plot.right()), warning_y),
                ],
                Stroke::new(1.0, look::AMBER),
            );
            x += 8.0;
        }
        let label = p.layout_no_wrap("Close to clipping".into(), font.clone(), look::AMBER);
        let label_pos = pos2(plot.left() + 6.0, warning_y + 4.0);
        p.rect_filled(
            Rect::from_min_size(label_pos - vec2(2.0, 1.0), label.size() + vec2(4.0, 2.0)),
            2.0,
            look::PANEL_BG,
        );
        p.galley(label_pos, label, look::AMBER);
        p.line_segment(
            [plot.right_top(), plot.right_bottom()],
            Stroke::new(1.0, look::ACCENT),
        );
        if !self.active {
            let text = p.layout(
                "Enable the microphone to see live levels.".into(),
                FontId::proportional(12.0),
                look::MUTED,
                (plot.width() - 16.0).max(40.0),
            );
            p.galley(plot.center() - text.size() * 0.5, text, look::MUTED);
        }
    }
}
fn draw_bar(p: &egui::Painter, plot: Rect, left: f32, right: f32, db: f32) {
    if right <= left {
        return;
    }
    let height = ((db + 60.0) / 60.0).clamp(0.0, 1.0) * plot.height();
    if height <= 0.0 {
        return;
    }
    p.rect_filled(
        Rect::from_min_max(
            pos2(left, plot.bottom() - height),
            pos2(right, plot.bottom()),
        ),
        egui::CornerRadius {
            nw: 4,
            ne: 4,
            sw: 0,
            se: 0,
        },
        Level::of(db).color(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn thresholds_name_every_color() {
        for (db, expected) in [
            (-60.0, Level::Good),
            (-9.001, Level::Good),
            (-9.0, Level::Close),
            (-6.0, Level::Close),
            (-3.001, Level::Close),
            (-3.0, Level::Clipping),
            (0.0, Level::Clipping),
        ] {
            assert_eq!(Level::of(db), expected);
            assert!(!expected.label().is_empty());
        }
    }
    #[test]
    fn partial_bar_keeps_loudest_sample_and_moves_continuously() {
        let mut t = Timeline::default();
        t.reset(true);
        t.observe(Duration::from_millis(50), [-20.0, -10.0]);
        t.observe(Duration::from_millis(100), [-30.0, -25.0]);
        assert_eq!(t.current, -10.0);
        assert!(t.completed.is_empty());
        assert!((t.phase(Duration::from_millis(125)) - 0.5).abs() < 0.001);
        assert!(t.phase(Duration::from_millis(126)) > t.phase(Duration::from_millis(125)));
        t.observe(Duration::from_millis(250), [-35.0; 2]);
        assert_eq!(t.completed[0], -10.0);
        assert_eq!(t.current, -35.0);
        assert_eq!(t.phase(Duration::from_millis(250)), 0.0);
    }
    #[test]
    fn window_shifts_and_old_audio_expires_after_a_stall() {
        let mut t = Timeline::default();
        t.reset(true);
        for i in 0..45 {
            t.observe(SLICE * i, [-(i as f32); 2]);
        }
        assert_eq!(t.completed.len(), BARS);
        assert_eq!(t.completed[0], -4.0);
        t.observe(Duration::from_secs(100), [-60.0; 2]);
        assert!(t.completed.iter().all(|db| *db == -60.0));
    }
    #[test]
    fn hidden_history_does_no_work_and_resume_starts_fresh() {
        let mut t = Timeline::default();
        t.observe(Duration::from_secs(30), [0.0; 2]);
        assert!(t.completed.is_empty());
        assert!(t.last_clip.is_none());
        t.reset(true);
        t.observe(Duration::from_millis(100), [0.0; 2]);
        assert_eq!(
            t.event_text(Duration::from_millis(2100), true),
            "Clipped 2 s ago"
        );
        t.reset(false);
        assert!(t.last_clip.is_none());
        assert!(!t.active);
    }
}
