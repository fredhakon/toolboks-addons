//! What part of the history is shown, the cursor and the selection: pure logic, no drawing.

use super::time::{local_midnight, DAY, HOUR, MIN, SEC};

/// The shortest and longest view.
pub const MIN_SPAN: i64 = 5 * MIN;
pub const MAX_SPAN: i64 = 7 * DAY;
/// The view never starts earlier than this before now (7 days kept, plus a little room).
pub const OLDEST: i64 = 7 * DAY + HOUR;
/// A view whose end is this close to now follows now.
const FOLLOW_SLACK: i64 = 2 * SEC;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preset {
    LastHour,
    Last24Hours,
    Yesterday,
    Last7Days,
}

impl Preset {
    pub const ALL: [Preset; 4] = [
        Preset::LastHour,
        Preset::Last24Hours,
        Preset::Yesterday,
        Preset::Last7Days,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Preset::LastHour => "Last hour",
            Preset::Last24Hours => "Last 24 hours",
            Preset::Yesterday => "Yesterday",
            Preset::Last7Days => "Last 7 days",
        }
    }
}

/// The shown period `[from, to)` in UTC ms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct View {
    pub from: i64,
    pub to: i64,
    /// Live: the end moves with the clock.
    pub follow: bool,
}

impl View {
    pub fn preset(p: Preset, now: i64) -> View {
        match p {
            Preset::LastHour => View::last(HOUR, now),
            Preset::Last24Hours => View::last(DAY, now),
            Preset::Last7Days => View::last(7 * DAY, now),
            Preset::Yesterday => {
                let today = local_midnight(now);
                // Local midnight of the day before (23 or 25 hours on a daylight saving day).
                let yesterday = local_midnight(today - HOUR);
                View {
                    from: yesterday,
                    to: today,
                    follow: false,
                }
            }
        }
    }

    pub fn last(span: i64, now: i64) -> View {
        View {
            from: now - span,
            to: now,
            follow: true,
        }
    }

    /// Which preset this view is, if any.
    pub fn matches(&self, p: Preset, now: i64) -> bool {
        let v = View::preset(p, now);
        if p == Preset::Yesterday {
            *self == v
        } else {
            self.follow && self.span() == v.span()
        }
    }

    pub fn span(&self) -> i64 {
        self.to - self.from
    }

    /// A custom period; `None` when it is empty or reversed.
    pub fn custom(from: i64, to: i64, now: i64) -> Option<View> {
        if to <= from {
            return None;
        }
        let mut v = View {
            from,
            to,
            follow: false,
        };
        v.clamp(now);
        Some(v)
    }

    /// Keeps the span within limits, the view inside the kept history, and follows now when
    /// its end reaches now.
    pub fn clamp(&mut self, now: i64) {
        let span = self.span().clamp(MIN_SPAN, MAX_SPAN);
        if self.to > now - FOLLOW_SLACK {
            self.to = now;
        }
        self.from = self.to - span;
        if self.from < now - OLDEST {
            self.from = now - OLDEST;
            self.to = self.from + span;
        }
        self.follow = self.to >= now - FOLLOW_SLACK;
        if self.follow {
            self.to = now;
            self.from = now - span;
        }
    }

    /// Zooms by `factor` (below 1 zooms in) keeping the moment `at` in place.
    pub fn zoom(&mut self, at: i64, factor: f64, now: i64) {
        let at = at.clamp(self.from, self.to);
        let span = ((self.span() as f64 * factor) as i64).clamp(MIN_SPAN, MAX_SPAN);
        let left = (at - self.from) as f64 / self.span().max(1) as f64;
        let follow = self.follow;
        self.from = at - (span as f64 * left) as i64;
        self.to = self.from + span;
        if follow {
            // Zooming a live view keeps it live.
            self.to = now;
            self.from = now - span;
        }
        self.clamp(now);
    }

    /// Moves the view by `delta` ms (negative = earlier).
    pub fn pan(&mut self, delta: i64, now: i64) {
        self.from += delta;
        self.to += delta;
        self.clamp(now);
    }

    /// A live view moves with the clock.
    pub fn tick(&mut self, now: i64) {
        if self.follow {
            let span = self.span();
            self.to = now;
            self.from = now - span;
        }
    }
}

/// The cursor and the selection, moved by mouse or keyboard.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Marks {
    pub cursor: Option<i64>,
    pub selection: Option<(i64, i64)>,
    /// Where Shift+arrows extend from.
    anchor: Option<i64>,
}

impl Marks {
    pub fn click(&mut self, t: i64) {
        self.cursor = Some(t);
        self.anchor = Some(t);
        self.selection = None;
    }

    /// A drag on the lanes from `a` to `b`.
    pub fn drag(&mut self, a: i64, b: i64) {
        self.anchor = Some(a);
        self.cursor = Some(b);
        self.selection = Some((a.min(b), a.max(b)));
    }

    pub fn clear(&mut self) {
        *self = Marks::default();
    }

    /// Arrow keys: moves the cursor one `step`; with `extend` (Shift) the selection grows from
    /// the anchor. The view pans when the cursor leaves it.
    pub fn arrow(&mut self, dir: i64, extend: bool, step: i64, view: &mut View, now: i64) {
        let step = step.max(SEC);
        let start = self.cursor.unwrap_or(match dir {
            d if d < 0 => view.to,
            _ => view.from,
        });
        let t = (start + dir * step).clamp(now - OLDEST, now);
        if extend {
            let anchor = self.anchor.or(self.cursor).unwrap_or(start);
            self.anchor = Some(anchor);
            self.selection = (anchor != t).then(|| (anchor.min(t), anchor.max(t)));
        } else {
            self.anchor = Some(t);
            self.selection = None;
        }
        self.cursor = Some(t);
        if t < view.from {
            view.pan(t - view.from, now);
        } else if t >= view.to {
            view.pan(t - view.to + step, now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_794_916_800_000;

    #[test]
    fn presets_and_custom_periods() {
        let v = View::preset(Preset::LastHour, NOW);
        assert_eq!((v.from, v.to, v.follow), (NOW - HOUR, NOW, true));
        assert!(v.matches(Preset::LastHour, NOW));
        assert!(!v.matches(Preset::Last24Hours, NOW));
        let y = View::preset(Preset::Yesterday, NOW);
        assert!(!y.follow && y.to <= NOW && (23 * HOUR..=25 * HOUR).contains(&y.span()));
        assert!(y.matches(Preset::Yesterday, NOW));
        assert_eq!(View::custom(NOW, NOW - 1, NOW), None);
        let c = View::custom(NOW - 3 * HOUR, NOW - 2 * HOUR, NOW).unwrap();
        assert_eq!(
            (c.from, c.to, c.follow),
            (NOW - 3 * HOUR, NOW - 2 * HOUR, false)
        );
        // Too short, too old, or into the future: kept within the limits.
        let c = View::custom(NOW - 3 * HOUR, NOW - 3 * HOUR + SEC, NOW).unwrap();
        assert_eq!(c.span(), MIN_SPAN);
        let c = View::custom(NOW - 30 * DAY, NOW - 29 * DAY, NOW).unwrap();
        assert_eq!(c.from, NOW - OLDEST);
        let c = View::custom(NOW - HOUR, NOW + HOUR, NOW).unwrap();
        assert!(c.follow && c.to == NOW);
    }

    #[test]
    fn zoom_keeps_the_moment_under_the_pointer_and_stays_in_limits() {
        let mut v = View::custom(NOW - 4 * HOUR, NOW - 2 * HOUR, NOW).unwrap();
        let at = NOW - 3 * HOUR;
        v.zoom(at, 0.5, NOW);
        assert_eq!((v.from, v.to), (at - 30 * MIN, at + 30 * MIN));
        for _ in 0..50 {
            v.zoom(at, 0.5, NOW);
        }
        assert_eq!(v.span(), MIN_SPAN);
        for _ in 0..50 {
            v.zoom(at, 2.0, NOW);
        }
        assert_eq!(v.span(), MAX_SPAN);
        assert!(v.from >= NOW - OLDEST && v.to <= NOW);
        // A live view zooms around now and stays live.
        let mut v = View::last(HOUR, NOW);
        v.zoom(NOW - 10 * MIN, 2.0, NOW);
        assert!(v.follow && v.to == NOW && v.span() == 2 * HOUR);
    }

    #[test]
    fn pan_leaves_and_rejoins_live() {
        let mut v = View::last(HOUR, NOW);
        v.pan(-10 * MIN, NOW);
        assert!(!v.follow);
        assert_eq!(v.to, NOW - 10 * MIN);
        v.tick(NOW + MIN);
        assert_eq!(v.to, NOW - 10 * MIN, "a past view does not move");
        v.pan(20 * MIN, NOW);
        assert!(v.follow && v.to == NOW);
        v.tick(NOW + MIN);
        assert_eq!(v.to, NOW + MIN);
    }

    #[test]
    fn arrows_move_the_cursor_and_shift_extends_the_selection() {
        let mut v = View::custom(NOW - 2 * HOUR, NOW - HOUR, NOW).unwrap();
        let mut m = Marks::default();
        m.click(NOW - 90 * MIN);
        m.arrow(1, false, MIN, &mut v, NOW);
        assert_eq!(m.cursor, Some(NOW - 89 * MIN));
        assert_eq!(m.selection, None);
        m.arrow(1, true, MIN, &mut v, NOW);
        m.arrow(1, true, MIN, &mut v, NOW);
        assert_eq!(m.selection, Some((NOW - 89 * MIN, NOW - 87 * MIN)));
        m.arrow(-1, true, MIN, &mut v, NOW);
        m.arrow(-1, true, MIN, &mut v, NOW);
        m.arrow(-1, true, MIN, &mut v, NOW);
        assert_eq!(
            m.selection,
            Some((NOW - 90 * MIN, NOW - 89 * MIN)),
            "back past the anchor"
        );
        // Without Shift the selection goes.
        m.arrow(-1, false, MIN, &mut v, NOW);
        assert_eq!((m.cursor, m.selection), (Some(NOW - 91 * MIN), None));
        // Past the edge of the view: the view follows the cursor.
        for _ in 0..40 {
            m.arrow(-1, false, MIN, &mut v, NOW);
        }
        assert!(v.from <= m.cursor.unwrap() && m.cursor.unwrap() < v.to);
        // No cursor yet: Left starts at the right edge.
        let mut m = Marks::default();
        let mut v = View::last(HOUR, NOW);
        m.arrow(-1, false, MIN, &mut v, NOW);
        assert_eq!(m.cursor, Some(NOW - MIN));
    }

    #[test]
    fn a_drag_selects_in_either_direction() {
        let mut m = Marks::default();
        m.drag(NOW - MIN, NOW - 5 * MIN);
        assert_eq!(m.selection, Some((NOW - 5 * MIN, NOW - MIN)));
        m.clear();
        assert_eq!(m, Marks::default());
    }
}
