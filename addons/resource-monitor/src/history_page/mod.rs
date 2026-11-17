//! The History page (#42): choose a time, see what the PC did, see which apps were involved.
//!
//! Reading path, top to bottom: the period (presets, From and To, Now), what is shown (period,
//! how much of it was recorded, the level of detail, live or history), the timeline (one lane
//! per resource), the readings at the cursor or over the selection, and the apps.
//!
//! All reading happens on query workers ([`crate::recording::query::Worker`]); the page only
//! sends requests and draws answers.

mod breakdown;
mod moments;
mod time;
mod timeline;
mod view;

use std::time::{Duration, Instant};

use egui::RichText;
use toolboks_shared::format::friendly_name;
use toolboks_shared::ui::{
    pill, ACCENT, AMBER, CARD_PAD, DATA_GPU, DATA_MEMORY, GAP, MUTED, SPACE_L, TEXT_CAPTION,
    TEXT_SECTION,
};

use crate::recording::query::{self, Answer, Request, Resource, Sources};
use time::SEC;
use timeline::{Drag, Empty, Timeline, LANES};
use view::{Marks, Preset, View};

/// A live view is read again this often.
const LIVE_EVERY: Duration = Duration::from_secs(5);

/// What the page needs from the add-on each frame.
pub(crate) struct Inputs {
    pub sources: Option<Sources>,
    /// Recording is on now.
    pub recording: bool,
    /// Steps the recorder holds in memory (0 right after a fresh start).
    pub live_steps: usize,
}

pub(crate) struct Page {
    view: View,
    marks: Marks,
    drag: Drag,
    apps_by: Resource,
    /// The app drawn on the timeline (exe name).
    selected_app: Option<String>,
    /// Notable moments (#44): the worker, the newest reply and what was asked.
    moments_worker: Option<moments::Worker>,
    moments: Option<moments::Reply>,
    asked_moments: Option<moments::Ask>,
    moments_error: Option<String>,
    curve_worker: Option<query::Worker>,
    detail_worker: Option<query::Worker>,
    curve: Option<Answer>,
    detail: Option<Answer>,
    asked_curve: Option<Request>,
    asked_detail: Option<Request>,
    asked_at: Option<Instant>,
    from_text: String,
    to_text: String,
    /// The view the From and To fields were filled from.
    texts_for: Option<(i64, i64)>,
    text_error: bool,
    mem_total: Option<u64>,
    /// Where the lanes were drawn (tests).
    lanes: egui::Rect,
}

impl Page {
    pub(crate) fn new() -> Self {
        Self {
            view: View::preset(Preset::LastHour, now_ms()),
            marks: Marks::default(),
            drag: Drag::default(),
            apps_by: Resource::Cpu,
            selected_app: None,
            moments_worker: None,
            moments: None,
            asked_moments: None,
            moments_error: None,
            curve_worker: None,
            detail_worker: None,
            curve: None,
            detail: None,
            asked_curve: None,
            asked_detail: None,
            asked_at: None,
            from_text: String::new(),
            to_text: String::new(),
            texts_for: None,
            text_error: false,
            mem_total: None,
            lanes: egui::Rect::NOTHING,
        }
    }

    /// Selects an app as if it was picked in the apps table, so the timeline draws it. Used by
    /// the Processes page's "Open in history"; Hide clears it as usual.
    pub(crate) fn show_app(&mut self, name: &str) {
        self.selected_app = Some(name.to_string());
    }
}

/// Wall clock, UTC ms, rounded down to whole seconds so a live view asks once per period.
fn now_ms() -> i64 {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    ms - ms.rem_euclid(SEC)
}

fn total_memory() -> Option<u64> {
    let mut s = sysinfo::System::new().ok()?;
    s.refresh_memory();
    Some(s.total_memory()).filter(|t| *t > 0)
}

impl Page {
    /// The period the apps list and the readings describe: the selection, the point under the
    /// cursor, or the whole view.
    fn focus_period(&self) -> (i64, i64) {
        if let Some(s) = self.marks.selection {
            return s;
        }
        if let Some(c) = self.marks.cursor {
            if let Some(b) = self
                .curve
                .as_ref()
                .and_then(|a| timeline::bin_at(&a.curve, c))
            {
                return (b.start_ms, b.end_ms);
            }
            let half = (self.view.span() / 400).max(5 * SEC) / 2;
            return (c - half, c + half);
        }
        (self.view.from, self.view.to)
    }

    fn ask(&mut self, inputs: &Inputs, bins: usize) {
        let Some(src) = &inputs.sources else {
            return;
        };
        let stale = self.asked_at.is_none_or(|t| t.elapsed() >= LIVE_EVERY) && self.view.follow;
        let curve = Request {
            from_ms: self.view.from,
            to_ms: self.view.to,
            bins,
            app: self.selected_app.clone(),
        };
        if stale || self.asked_curve.as_ref() != Some(&curve) {
            if self.curve_worker.is_none() {
                self.curve_worker = query::Worker::start();
            }
            if let Some(w) = &self.curve_worker {
                w.ask(curve.clone(), src.clone());
                self.asked_curve = Some(curve);
                self.asked_at = Some(Instant::now());
            }
        }
        let (from, to) = self.focus_period();
        let detail = Request {
            from_ms: from,
            to_ms: to,
            bins: 1,
            app: None,
        };
        if stale || self.asked_detail.as_ref() != Some(&detail) {
            if self.detail_worker.is_none() {
                self.detail_worker = query::Worker::start();
            }
            if let Some(w) = &self.detail_worker {
                w.ask(detail.clone(), src.clone());
                self.asked_detail = Some(detail);
            }
        }
    }

    fn receive(&mut self) -> bool {
        let mut waiting = false;
        if let Some(a) = self.curve_worker.as_ref().and_then(|w| w.take()) {
            self.curve = Some(a);
        }
        if let Some(a) = self.detail_worker.as_ref().and_then(|w| w.take()) {
            self.detail = Some(a);
        }
        if self.curve.as_ref().and_then(|a| a.request.as_ref()) != self.asked_curve.as_ref() {
            waiting = true;
        }
        if self.detail.as_ref().and_then(|a| a.request.as_ref()) != self.asked_detail.as_ref() {
            waiting = true;
        }
        if let Some(r) = self.moments_worker.as_ref().and_then(|w| w.take()) {
            if r.mark_error.is_some() {
                self.moments_error = r.mark_error.clone();
            }
            self.moments = Some(r);
        }
        if self.moments.as_ref().map(|r| &r.ask) != self.asked_moments.as_ref() {
            waiting = true;
        }
        waiting
    }

    /// What the notable moments are asked for: the view, to the whole minute when live (so a
    /// live view looks again once a minute, not every second).
    fn moments_ask(&self) -> moments::Ask {
        let (mut from, mut to) = (self.view.from, self.view.to);
        if self.view.follow {
            let up = (to + time::MIN - 1).div_euclid(time::MIN) * time::MIN;
            from += up - to;
            to = up;
        }
        moments::Ask {
            from_ms: from,
            to_ms: to,
            app: self.selected_app.clone(),
            mem_total: self.mem_total,
        }
    }

    fn ask_moments(&mut self, inputs: &Inputs, now: i64, mark: Option<i64>) {
        let Some(src) = &inputs.sources else {
            return;
        };
        let ask = self.moments_ask();
        if mark.is_none() && self.asked_moments.as_ref() == Some(&ask) {
            return;
        }
        if self.moments_worker.is_none() {
            self.moments_worker = moments::Worker::start();
        }
        if let Some(w) = &self.moments_worker {
            match mark {
                Some(at) => w.mark(at, ask.clone(), src.clone(), now),
                None => w.find(ask.clone(), src.clone(), now),
            }
            self.asked_moments = Some(ask);
        }
    }

    /// The notable moments list; choosing one selects its period.
    fn moments_ui(&mut self, ui: &mut egui::Ui, inputs: &Inputs, now: i64) {
        let at = self.marks.cursor.unwrap_or(now).min(now);
        let found = self
            .moments
            .as_ref()
            .filter(|r| Some(&r.ask) == self.asked_moments.as_ref())
            .or(self.moments.as_ref())
            .map(|r| &r.found);
        let (chosen, mark) = moments::list_ui(
            ui,
            found,
            self.marks.selection,
            inputs.sources.is_some(),
            &time::moment(at, now, true),
            now,
        );
        if let Some(e) = &self.moments_error {
            ui.label(RichText::new(format!("The mark was not saved: {e}.")).color(AMBER));
        }
        if let Some(m) = chosen {
            moments::choose(&m, &mut self.view, &mut self.marks, now);
        }
        if mark {
            self.moments_error = None;
            self.ask_moments(inputs, now, Some(at));
        }
    }

    pub(crate) fn ui(&mut self, ui: &mut egui::Ui, inputs: Inputs) {
        let now = now_ms();
        self.view.tick(now);
        if self.mem_total.is_none() {
            self.mem_total = total_memory();
        }
        let waiting = self.receive();
        ui.spacing_mut().item_spacing.y = GAP;

        toolboks_addon_sdk::tutorial::region(ui, "history-range", |ui| self.period_bar(ui, now));
        self.status_line(ui, &inputs, now);

        let lane_h = ((ui.available_height() - 330.0) / LANES.len() as f32)
            .clamp(timeline::LANE_MIN_H, timeline::LANE_MAX_H);
        let empty = self.empty_state(&inputs);
        let t = Timeline {
            curve: self.curve.as_ref(),
            mem_total: self.mem_total,
            empty: empty.map(|(title, body)| Empty { title, body }),
            now,
            lane_h,
            app: self.selected_app.as_deref(),
            moments: self
                .moments
                .as_ref()
                .map(|r| r.found.moments.as_slice())
                .unwrap_or(&[]),
        };
        self.lanes = timeline::show(ui, &t, &mut self.view, &mut self.marks, &mut self.drag);
        if empty.is_some_and(|(title, _)| title == NOTHING.0)
            && ui.button("Show the last 7 days").clicked()
        {
            self.view = View::preset(Preset::Last7Days, now);
            self.marks.clear();
        }
        self.readout(ui, now);
        toolboks_addon_sdk::tutorial::region(ui, "history-moments", |ui| {
            self.moments_ui(ui, &inputs, now)
        });
        toolboks_addon_sdk::tutorial::region(ui, "history-apps", |ui| self.apps_ui(ui, now));

        let bins = ((self.lanes.width() / 2.0).round() as usize).clamp(1, query::MAX_BINS);
        self.ask(&inputs, bins);
        self.ask_moments(&inputs, now, None);
        if waiting || self.receive() {
            ui.ctx().request_repaint_after(Duration::from_millis(50));
        } else if self.view.follow {
            ui.ctx().request_repaint_after(LIVE_EVERY);
        }
    }

    fn period_bar(&mut self, ui: &mut egui::Ui, now: i64) {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            for p in Preset::ALL {
                if pill(ui, p.label(), self.view.matches(p, now)).clicked() {
                    self.view = View::preset(p, now);
                    self.marks.clear();
                }
            }
            ui.add_space(SPACE_L);
            if self.texts_for != Some((self.view.from, self.view.to)) && !self.text_error {
                self.from_text = time::input(self.view.from);
                self.to_text = time::input(self.view.to);
                self.texts_for = Some((self.view.from, self.view.to));
            }
            let field = |ui: &mut egui::Ui, label: &str, text: &mut String| {
                ui.label(RichText::new(label).weak());
                let r = ui.add(
                    egui::TextEdit::singleline(text)
                        .desired_width(118.0)
                        .hint_text("2026-11-17 14:30"),
                );
                r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
            };
            let mut go = field(ui, "From", &mut self.from_text);
            go |= field(ui, "To", &mut self.to_text);
            go |= ui.button("Show").clicked();
            if go {
                let parsed = time::parse_input(&self.from_text)
                    .zip(time::parse_input(&self.to_text))
                    .and_then(|(f, t)| View::custom(f, t, now));
                match parsed {
                    Some(v) => {
                        self.view = v;
                        self.marks.clear();
                        self.text_error = false;
                        self.texts_for = None;
                    }
                    None => self.text_error = true,
                }
            }
            ui.add_space(SPACE_L);
            let now_button = ui.add_enabled(!self.view.follow, egui::Button::new("Now"));
            if now_button
                .on_hover_text("Back to live: the period ends now and moves with the clock")
                .clicked()
            {
                self.view = View::last(self.view.span(), now);
                self.marks.clear();
            }
        });
        if self.text_error {
            ui.label(
                RichText::new("Use the form 2026-11-17 14:30 for From and To, with To after From.")
                    .color(AMBER),
            );
        }
    }

    fn status_line(&self, ui: &mut egui::Ui, inputs: &Inputs, now: i64) {
        let seconds = self.view.span() <= 2 * time::HOUR;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let tag = if self.view.follow { "LIVE" } else { "HISTORY" };
            ui.label(
                RichText::new(tag)
                    .size(TEXT_CAPTION)
                    .strong()
                    .color(if self.view.follow { ACCENT } else { MUTED }),
            );
            ui.label(
                RichText::new(time::period(self.view.from, self.view.to, now, seconds))
                    .size(TEXT_SECTION)
                    .strong(),
            );
            if let Some(a) = &self.curve {
                let b = &a.breakdown;
                ui.label(
                    RichText::new(format!(
                        "· {} of {} recorded",
                        time::duration(b.recorded_secs),
                        time::duration(b.period_secs())
                    ))
                    .weak(),
                );
                let coarsest = a
                    .curve
                    .iter()
                    .filter_map(|b| b.detail)
                    .max_by_key(|l| match l {
                        crate::recording::disk::Level::Fine => 0,
                        crate::recording::disk::Level::Minute => 1,
                        crate::recording::disk::Level::Day => 2,
                    });
                if coarsest.is_some() {
                    ui.label(
                        RichText::new(format!("· {}", timeline::detail_text(coarsest))).weak(),
                    );
                }
                if !a.problems.is_empty() {
                    ui.label(
                        RichText::new(format!(
                            "· {} history file(s) could not be read in full",
                            a.problems.len()
                        ))
                        .color(AMBER),
                    )
                    .on_hover_text(a.problems.join("\n"));
                }
            }
            if !inputs.recording {
                ui.label(RichText::new("· Recording is off").color(AMBER));
            }
        });
    }

    /// Title and text when there is nothing to draw.
    fn empty_state(&self, inputs: &Inputs) -> Option<(&'static str, &'static str)> {
        let a = self.curve.as_ref()?;
        if a.breakdown.recorded_secs > 0.0 {
            return None;
        }
        Some(if !inputs.recording {
            RECORDING_OFF
        } else if inputs.live_steps < 2 && self.view.follow {
            STARTS_NOW
        } else {
            NOTHING
        })
    }

    /// The readings at the cursor or over the selection, as text (the same as the hover).
    fn readout(&self, ui: &mut egui::Ui, now: i64) {
        let mut parts: Vec<String> = Vec::new();
        let head = if let Some((a, b)) = self.marks.selection {
            format!(
                "{} ({})",
                time::period(a, b, now, true),
                time::duration((b - a) as f64 / 1000.0)
            )
        } else if let Some(c) = self.marks.cursor {
            format!("At {}", time::moment(c, now, true))
        } else {
            ui.label(
                RichText::new(
                    "Click the timeline to set a cursor, drag to select a period, drag the time axis to move, scroll to zoom. With the timeline focused, arrow keys move the cursor and Shift+arrows extend the selection.",
                )
                .size(TEXT_CAPTION)
                .weak(),
            );
            return;
        };
        let system = if self.marks.selection.is_some() {
            self.detail
                .as_ref()
                .filter(|d| d.request.as_ref() == self.asked_detail.as_ref())
                .map(|d| (d.breakdown.system, d.breakdown.recorded_secs > 0.0))
        } else {
            self.marks
                .cursor
                .and_then(|c| {
                    self.curve
                        .as_ref()
                        .and_then(|a| timeline::bin_at(&a.curve, c))
                })
                .map(|b| (b.totals, !b.is_gap()))
        };
        match system {
            Some((totals, true)) => {
                for lane in &LANES {
                    for (idx, _, label) in std::iter::once(lane.first).chain(lane.second) {
                        if let Some(s) = totals[idx] {
                            let mut text = format!(
                                "{label} {}",
                                timeline::value_text(lane.unit, s.avg as f64, self.mem_total)
                            );
                            if self.marks.selection.is_some() {
                                text.push_str(&format!(
                                    " (peak {})",
                                    timeline::value_text(lane.unit, s.peak as f64, None)
                                ));
                            }
                            parts.push(text);
                        }
                    }
                }
            }
            Some((_, false)) => parts.push("not recorded".into()),
            None => parts.push("reading...".into()),
        }
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(head).strong());
            ui.label(RichText::new(parts.join(" · ")).weak());
        });
    }

    fn apps_ui(&mut self, ui: &mut egui::Ui, now: i64) {
        let (from, to) = self.focus_period();
        let (title, when) = if self.marks.selection.is_some() {
            let p = time::period(from, to, now, true).replacen(" to ", " and ", 1);
            (format!("Apps between {p}"), format!("Between {p}"))
        } else if let Some(c) = self.marks.cursor {
            let m = time::moment(c, now, true);
            (format!("Apps at {m}"), format!("At {m}"))
        } else {
            let p = time::period(from, to, now, false).replacen(" to ", " and ", 1);
            ("Apps in this period".to_string(), format!("Between {p}"))
        };
        let ready = self
            .detail
            .as_ref()
            .filter(|d| d.request.as_ref() == self.asked_detail.as_ref());
        let by = self.apps_by;
        let rows = ready.map(|d| breakdown::rows(&d.breakdown, by));
        let sentence = ready.map(|d| breakdown::sentence(&d.breakdown, by, &when, self.mem_total));
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            ui.label(RichText::new(&title).size(TEXT_SECTION).strong());
            ui.add_space(GAP);
            for r in [
                Resource::Cpu,
                Resource::Memory,
                Resource::Gpu,
                Resource::Disk,
            ] {
                if pill(ui, breakdown::resource_name(r), self.apps_by == r).clicked() {
                    self.apps_by = r;
                }
            }
            ui.add_space(GAP);
            let copy = ui.add_enabled(rows.is_some(), egui::Button::new("Copy table"));
            if copy
                .on_hover_text("Copies the sentence and the table as text (tab-separated)")
                .clicked()
            {
                if let (Some(rows), Some(s)) = (&rows, &sentence) {
                    ui.ctx()
                        .copy_text(breakdown::copy_text(&title, s, by, rows));
                }
            }
        });
        let (Some(rows), Some(sentence)) = (rows, sentence) else {
            ui.label(RichText::new("Reading...").weak());
            return;
        };
        ui.label(RichText::new(sentence).strong());
        ui.label(
            RichText::new(
                "Network use is recorded for the whole PC only, not per app. Select an app to draw it on the timeline.",
            )
            .size(TEXT_CAPTION)
            .weak(),
        );
        if let Some(name) = self.selected_app.clone() {
            self.app_events_line(ui, &name, now);
        }
        let color = match by {
            Resource::Cpu => ACCENT,
            Resource::Memory => DATA_MEMORY,
            Resource::Gpu => DATA_GPU,
            Resource::Disk => AMBER,
        };
        let width = ui.available_width();
        let (name_w, num_w, gap) = (190.0, 96.0, 12.0);
        let bar_w = (width - name_w - 3.0 * num_w - 4.0 * gap - 2.0 * CARD_PAD).clamp(60.0, 320.0);
        let mut clicked: Option<String> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, true])
            .show(ui, |ui| {
                egui::Grid::new("history-apps")
                    .num_columns(5)
                    .spacing([gap, 4.0])
                    .show(ui, |ui| {
                        let share_head = format!("{} share", breakdown::resource_name(by));
                        for h in ["App", share_head.as_str(), "Average", "Peak", "Active"] {
                            ui.label(RichText::new(h).size(TEXT_CAPTION).color(MUTED));
                        }
                        ui.end_row();
                        for r in &rows {
                            ui.allocate_ui_with_layout(
                                egui::vec2(name_w, 20.0),
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| {
                                    ui.set_width(name_w);
                                    match &r.kind {
                                        breakdown::Kind::App(name) => {
                                            let on = self
                                                .selected_app
                                                .as_deref()
                                                .is_some_and(|s| s.eq_ignore_ascii_case(name));
                                            let resp = ui
                                                .selectable_label(on, friendly_name(name))
                                                .on_hover_text(format!(
                                                    "{name}: click to draw it on the timeline"
                                                ));
                                            if resp.clicked() {
                                                clicked = Some(name.clone());
                                            }
                                        }
                                        _ => {
                                            ui.label(RichText::new(&r.label).weak());
                                        }
                                    }
                                },
                            );
                            match r.share {
                                Some(share) => {
                                    let fill = if r.kind == breakdown::Kind::Else {
                                        MUTED
                                    } else {
                                        color
                                    };
                                    ui.add(
                                        egui::ProgressBar::new(share as f32)
                                            .desired_width(bar_w)
                                            .desired_height(10.0)
                                            .fill(fill),
                                    )
                                    .on_hover_text(format!(
                                        "{:.1} % of what all apps used",
                                        share * 100.0
                                    ));
                                }
                                None => {
                                    ui.label("");
                                }
                            }
                            ui.label(&r.avg);
                            ui.label(RichText::new(&r.peak).weak());
                            ui.label(RichText::new(&r.active).weak());
                            ui.end_row();
                        }
                    });
            });
        if let Some(name) = clicked {
            let same = self
                .selected_app
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case(&name));
            self.selected_app = (!same).then_some(name);
        }
    }

    /// "chrome.exe on the timeline (white line): started 14:02, exited 14:40."
    fn app_events_line(&mut self, ui: &mut egui::Ui, name: &str, now: i64) {
        let mut text = format!("{name} is drawn on the timeline (white line)");
        if let Some(a) = &self.curve {
            let mut parts: Vec<String> = Vec::new();
            for e in a
                .events
                .iter()
                .filter(|e| e.name.eq_ignore_ascii_case(name))
            {
                let at = time::moment(e.utc_ms, now, false);
                let what = match e.kind {
                    crate::recording::record::EventKind::Started if e.during_gap => {
                        format!("started while not recorded (seen at {at})")
                    }
                    crate::recording::record::EventKind::Started => format!("started {at}"),
                    crate::recording::record::EventKind::Exited if e.during_gap => {
                        format!("exited while not recorded (noticed at {at})")
                    }
                    crate::recording::record::EventKind::Exited => format!("exited {at}"),
                    crate::recording::record::EventKind::AlreadyRunning => {
                        format!("running when recording began {at}")
                    }
                };
                parts.push(what);
            }
            if parts.is_empty() {
                text.push_str(": no start or exit in this view.");
            } else {
                let n = parts.len();
                if n > 6 {
                    parts.drain(..n - 6);
                    text.push_str(&format!(": {} more before; ", n - 6));
                } else {
                    text.push_str(": ");
                }
                text.push_str(&parts.join(", "));
                text.push('.');
            }
        }
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(text).size(TEXT_CAPTION));
            if ui.small_button("Hide").clicked() {
                self.selected_app = None;
            }
        });
    }
}

const RECORDING_OFF: (&str, &str) = (
    "Recording is off",
    "Resource history is not being recorded. History saved earlier shows here until it is 7 days old.",
);
const STARTS_NOW: (&str, &str) = (
    "History starts now",
    "Resource Monitor records every 5 seconds while Toolboks runs, also in the tray. The first readings show here within a minute.",
);
const NOTHING: (&str, &str) = (
    "Nothing recorded in this period",
    "Toolboks was not running or recording was off. Pick another period.",
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::query::{Bin, Breakdown};
    use crate::recording::store::Stat;

    fn answer(view: &View, bins: usize, gap_from: usize) -> Answer {
        let step = view.span() / bins as i64;
        let stat = |v: f32| {
            Some(Stat {
                avg: v,
                peak: v * 1.5,
            })
        };
        Answer {
            request: None,
            curve: (0..bins)
                .map(|i| {
                    let gap = i >= gap_from;
                    let mut totals = [None; crate::recording::record::TOTAL_FIGURES];
                    if !gap {
                        for (k, t) in totals.iter_mut().enumerate() {
                            *t = stat(10.0 + k as f32);
                        }
                    }
                    Bin {
                        start_ms: view.from + i as i64 * step,
                        end_ms: view.from + (i as i64 + 1) * step,
                        covered_secs: if gap { 0.0 } else { step as f64 / 1000.0 },
                        detail: (!gap).then_some(crate::recording::disk::Level::Fine),
                        totals,
                        app: None,
                    }
                })
                .collect(),
            breakdown: Breakdown {
                from_ms: view.from,
                to_ms: view.to,
                recorded_secs: 100.0,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn frame(
        ctx: &egui::Context,
        page: &mut Page,
        events: Vec<egui::Event>,
        _mods: egui::Modifiers,
    ) {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1060.0 - 220.0, 700.0),
            )),
            events,
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                page.ui(
                    ui,
                    Inputs {
                        sources: None,
                        recording: true,
                        live_steps: 100,
                    },
                )
            });
        });
        out.textures_delta.clear();
    }

    fn key(k: egui::Key, mods: egui::Modifiers) -> Vec<egui::Event> {
        vec![
            egui::Event::Key {
                key: k,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: mods,
            },
            egui::Event::Key {
                key: k,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: mods,
            },
        ]
    }

    fn click(at: egui::Pos2) -> Vec<Vec<egui::Event>> {
        let button = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        vec![
            vec![egui::Event::PointerMoved(at)],
            vec![button(true)],
            vec![button(false)],
        ]
    }

    /// The page at the smallest window: mouse click sets the cursor, the keyboard moves it and
    /// extends a selection, Escape clears it; gaps and empty states draw without panics.
    #[test]
    fn mouse_and_keyboard_set_the_cursor_and_the_selection() {
        let ctx = egui::Context::default();
        let mut page = Page::new();
        page.view = View::custom(
            page.view.from - time::HOUR,
            page.view.to - time::HOUR,
            now_ms(),
        )
        .unwrap();
        page.curve = Some(answer(&page.view, 60, 50));
        frame(&ctx, &mut page, vec![], egui::Modifiers::NONE);
        let lanes = page.lanes;
        assert!(lanes.width() > 400.0, "{lanes:?}");
        let x = lanes.left() + lanes.width() * 0.25;
        for events in click(egui::pos2(x, lanes.center().y)) {
            frame(&ctx, &mut page, events, egui::Modifiers::NONE);
        }
        let c = page.marks.cursor.expect("a click sets the cursor");
        // One quiet frame: the focused timeline now keeps the arrow keys.
        frame(&ctx, &mut page, vec![], egui::Modifiers::NONE);
        let expected = page.view.from + page.view.span() / 4;
        assert!(
            (c - expected).abs() <= page.view.span() / 100,
            "{c} vs {expected}"
        );
        frame(
            &ctx,
            &mut page,
            key(egui::Key::ArrowRight, egui::Modifiers::NONE),
            egui::Modifiers::NONE,
        );
        let step = page.view.span() / 60;
        assert_eq!(page.marks.cursor, Some(c + step));
        frame(
            &ctx,
            &mut page,
            key(egui::Key::ArrowRight, egui::Modifiers::SHIFT),
            egui::Modifiers::SHIFT,
        );
        frame(
            &ctx,
            &mut page,
            key(egui::Key::ArrowRight, egui::Modifiers::SHIFT),
            egui::Modifiers::SHIFT,
        );
        assert_eq!(page.marks.selection, Some((c + step, c + 3 * step)));
        frame(
            &ctx,
            &mut page,
            key(egui::Key::Escape, egui::Modifiers::NONE),
            egui::Modifiers::NONE,
        );
        assert_eq!(page.marks, Marks::default());
        // The apps table draws, with an app selected and drawn on the timeline.
        let row = |name: &str, cpu: f64| query::AppRow {
            name: name.into(),
            active_secs: 60.0,
            peak_procs: 1,
            disk_known: name != "MsMpEng.exe",
            figures: [query::Figure {
                avg: cpu,
                peak: cpu,
                total: cpu * 60.0,
                share: 0.5,
            }; 5],
        };
        page.detail = Some(Answer {
            breakdown: Breakdown {
                from_ms: 0,
                to_ms: 120_000,
                recorded_secs: 60.0,
                apps: vec![row("chrome.exe", 20.0), row("MsMpEng.exe", 5.0)],
                ..Default::default()
            },
            ..Default::default()
        });
        page.show_app("chrome.exe");
        assert_eq!(page.selected_app.as_deref(), Some("chrome.exe"));
        for by in [Resource::Cpu, Resource::Disk] {
            page.apps_by = by;
            frame(&ctx, &mut page, vec![], egui::Modifiers::NONE);
        }
        // Empty states draw.
        page.curve.as_mut().unwrap().breakdown.recorded_secs = 0.0;
        frame(&ctx, &mut page, vec![], egui::Modifiers::NONE);
        assert_eq!(
            page.empty_state(&Inputs {
                sources: None,
                recording: false,
                live_steps: 0
            }),
            Some(RECORDING_OFF)
        );
    }

    #[test]
    fn a_drag_on_the_lanes_selects_and_on_the_axis_pans() {
        let ctx = egui::Context::default();
        let mut page = Page::new();
        page.view = View::custom(
            page.view.from - time::HOUR,
            page.view.to - time::HOUR,
            now_ms(),
        )
        .unwrap();
        page.curve = Some(answer(&page.view, 60, 60));
        frame(&ctx, &mut page, vec![], egui::Modifiers::NONE);
        let lanes = page.lanes;
        let drag = |page: &mut Page, y: f32, x0: f32, x1: f32| {
            let b = |x: f32, pressed| egui::Event::PointerButton {
                pos: egui::pos2(x, y),
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            let mv = |x: f32| egui::Event::PointerMoved(egui::pos2(x, y));
            for events in [
                vec![mv(x0)],
                vec![b(x0, true)],
                vec![mv(x0 + 10.0)],
                vec![mv(x1)],
                vec![mv(x1)],
                vec![b(x1, false)],
            ] {
                frame(&ctx, page, events, egui::Modifiers::NONE);
            }
        };
        let (x0, x1) = (lanes.left() + 100.0, lanes.left() + 300.0);
        drag(&mut page, lanes.center().y, x0, x1);
        let (a, b) = page.marks.selection.expect("a drag selects");
        let per_px = page.view.span() as f64 / lanes.width() as f64;
        assert!(((b - a) as f64 - 200.0 * per_px).abs() < 3.0 * per_px);
        let before = page.view;
        drag(&mut page, lanes.bottom() + 8.0, x1, x0);
        assert!(
            page.view.from > before.from,
            "dragging the axis left shows later times"
        );
        assert_eq!(page.view.span(), before.span());
    }
}
