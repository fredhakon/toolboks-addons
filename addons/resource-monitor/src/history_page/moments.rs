//! Notable moments on the History page (#44): found by `recording::markers` on a worker
//! thread, listed under the timeline and marked on it; choosing one selects its period.

use std::sync::mpsc::{self, Receiver, Sender};

use egui::{Color32, RichText};
use toolboks_shared::ui::{
    ACCENT, AMBER, DATA_DISK, DATA_GPU, DATA_MEMORY, MUTED, RED, TEXT, TEXT_CAPTION, TEXT_SECTION,
};

use super::time;
use super::view::{Marks, View};
use crate::recording::markers::{self, Found, Moment, MomentKind, Options};
use crate::recording::query::{Resource, Sources};

/// What the worker is asked.
#[derive(Clone, Debug, PartialEq)]
pub struct Ask {
    pub from_ms: i64,
    pub to_ms: i64,
    pub app: Option<String>,
    pub mem_total: Option<u64>,
}

enum Job {
    Find(Ask, Sources, i64),
    /// Save a manual mark at this time, then find again.
    Mark(i64, Ask, Sources, i64),
}

pub struct Reply {
    pub ask: Ask,
    pub found: Found,
    /// A mark could not be saved: why.
    pub mark_error: Option<String>,
}

/// Finds moments (and saves manual marks) on its own thread; the newest job wins, except that
/// a mark is always saved.
pub struct Worker {
    jobs: Sender<Job>,
    replies: Receiver<Reply>,
}

impl Worker {
    pub fn start() -> Option<Worker> {
        let (jobs, waiting) = mpsc::channel::<Job>();
        let (send, replies) = mpsc::channel();
        std::thread::Builder::new()
            .name("resource-history-moments".into())
            .spawn(move || {
                while let Ok(mut job) = waiting.recv() {
                    let mut mark_error = None;
                    loop {
                        if let Job::Mark(at, ask, src, now) = job {
                            if let Err(e) = markers::manual::add(&src.dir, at, "", now) {
                                mark_error = Some(e.to_string());
                            }
                            job = Job::Find(ask.clone(), src.clone(), now);
                        }
                        match waiting.try_recv() {
                            Ok(newer) => job = newer,
                            Err(_) => break,
                        }
                    }
                    let Job::Find(ask, src, now) = job else {
                        continue;
                    };
                    let opts = Options {
                        mem_total: ask.mem_total,
                        app: ask.app.clone(),
                    };
                    let found = markers::find(&src, ask.from_ms, ask.to_ms, now, &opts);
                    if send
                        .send(Reply {
                            ask,
                            found,
                            mark_error,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            })
            .ok()?;
        Some(Worker { jobs, replies })
    }

    pub fn find(&self, ask: Ask, src: Sources, now: i64) {
        let _ = self.jobs.send(Job::Find(ask, src, now));
    }

    pub fn mark(&self, at: i64, ask: Ask, src: Sources, now: i64) {
        let _ = self.jobs.send(Job::Mark(at, ask, src, now));
    }

    pub fn take(&self) -> Option<Reply> {
        self.replies.try_iter().last()
    }
}

/// The colour a moment is marked in.
pub fn color(kind: &MomentKind) -> Color32 {
    match kind {
        MomentKind::Held(Resource::Cpu) => ACCENT,
        MomentKind::Held(Resource::Gpu) => DATA_GPU,
        MomentKind::Held(_) => DATA_DISK,
        MomentKind::MemoryHigh => DATA_MEMORY,
        MomentKind::Leak(_) => RED,
        MomentKind::AppStarted(_) => DATA_GPU,
        MomentKind::AppExited(_) => MUTED,
        MomentKind::Gap => MUTED,
        MomentKind::Manual => TEXT,
    }
}

/// Choosing a moment: selects its period and moves the view so the whole period shows, with a
/// little room on both sides when the view has to move.
pub fn choose(m: &Moment, view: &mut View, marks: &mut Marks, now: i64) {
    let (a, b) = m.period();
    let b = b.min(now);
    let a = a.min(b - time::SEC);
    if a < view.from || b > view.to {
        let pad = ((b - a) / 4).max(5 * time::MIN);
        let span = (b - a + 2 * pad).max(view.span().min(b - a + 2 * pad));
        if let Some(v) = View::custom(a - pad, a - pad + span, now) {
            *view = v;
        }
    }
    marks.drag(a, b);
}

/// The "Notable moments" list. Returns the moment chosen, and whether "Mark this moment" was
/// pressed.
pub fn list_ui(
    ui: &mut egui::Ui,
    found: Option<&Found>,
    selected: Option<(i64, i64)>,
    can_mark: bool,
    mark_at: &str,
    now: i64,
) -> (Option<Moment>, bool) {
    let mut chosen = None;
    let mut mark = false;
    ui.horizontal_wrapped(|ui| {
        let n = found.map(|f| f.moments.len());
        let title = match n {
            Some(n) => format!("Notable moments ({n})"),
            None => "Notable moments".to_string(),
        };
        ui.label(RichText::new(title).size(TEXT_SECTION).strong());
        let resp = ui
            .add_enabled(can_mark, egui::Button::new("Mark this moment"))
            .on_hover_text(format!(
                "Saves a mark at {mark_at}, kept with the history for 7 days"
            ));
        mark = resp.clicked();
    });
    let Some(found) = found else {
        ui.label(RichText::new("Looking for spikes, high memory, growing apps and gaps...").weak());
        return (None, mark);
    };
    if !found.problems.is_empty() {
        ui.label(
            RichText::new(format!(
                "{} history file(s) could not be read in full",
                found.problems.len()
            ))
            .size(TEXT_CAPTION)
            .color(AMBER),
        )
        .on_hover_text(found.problems.join("\n"));
    }
    if found.moments.is_empty() {
        ui.label(
            RichText::new("Nothing notable in this period: no resource held near full, no memory above 90 %, no app growing steadily, no gap.")
                .size(TEXT_CAPTION)
                .weak(),
        );
        return (None, mark);
    }
    egui::ScrollArea::vertical()
        .id_salt("history-moments")
        .max_height(120.0)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            for m in &found.moments {
                let on = selected == Some(m.period());
                let when = if m.is_point() {
                    time::moment(m.start_ms, now, false)
                } else {
                    time::period(m.start_ms, m.end_ms, now, false)
                };
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let (dot, _) =
                        ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                    ui.painter()
                        .circle_filled(dot.center(), 3.5, color(&m.kind));
                    let resp = ui
                        .selectable_label(on, format!("{} · {when}", m.kind.title()))
                        .on_hover_text(&m.text);
                    if resp.clicked() {
                        chosen = Some(m.clone());
                    }
                    ui.add(
                        egui::Label::new(RichText::new(&m.text).size(TEXT_CAPTION).weak())
                            .truncate(),
                    );
                });
            }
        });
    (chosen, mark)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history_page::time::{DAY, HOUR, MIN};

    const NOW: i64 = 1_794_916_800_000;

    fn moment(kind: MomentKind, start: i64, end: i64) -> Moment {
        Moment {
            kind,
            start_ms: start,
            end_ms: end,
            text: "x.".into(),
        }
    }

    #[test]
    fn choosing_a_moment_selects_its_period_and_shows_it() {
        // Inside the view: the view stays, the period is selected.
        let mut view = View::last(HOUR, NOW);
        let mut marks = Marks::default();
        let spike = moment(
            MomentKind::Held(Resource::Cpu),
            NOW - 40 * MIN,
            NOW - 30 * MIN,
        );
        choose(&spike, &mut view, &mut marks, NOW);
        assert_eq!(marks.selection, Some((NOW - 40 * MIN, NOW - 30 * MIN)));
        assert_eq!(view, View::last(HOUR, NOW));
        // Outside the view: the view moves so the whole period shows.
        let leak = moment(
            MomentKind::Leak("leaky.exe".into()),
            NOW - 2 * DAY,
            NOW - 2 * DAY + 4 * HOUR,
        );
        choose(&leak, &mut view, &mut marks, NOW);
        assert_eq!(marks.selection, Some(leak.period()));
        assert!(
            view.from < leak.start_ms && view.to > leak.end_ms,
            "{view:?}"
        );
        assert!(!view.follow);
        // A point selects 5 minutes on each side.
        let started = moment(
            MomentKind::AppStarted("a.exe".into()),
            NOW - 3 * HOUR,
            NOW - 3 * HOUR,
        );
        choose(&started, &mut view, &mut marks, NOW);
        assert_eq!(
            marks.selection,
            Some((NOW - 3 * HOUR - 5 * MIN, NOW - 3 * HOUR + 5 * MIN))
        );
        assert!(view.from <= NOW - 3 * HOUR - 5 * MIN && view.to >= NOW - 3 * HOUR + 5 * MIN);
        // A point near now: never selects the future.
        let mark = moment(MomentKind::Manual, NOW - MIN, NOW - MIN);
        choose(&mark, &mut view, &mut marks, NOW);
        assert_eq!(marks.selection, Some((NOW - 6 * MIN, NOW)));
    }

    #[test]
    fn the_worker_saves_a_mark_and_finds_it() {
        let dir = crate::recording::disk::tests::test_dir("moments-worker");
        let w = Worker::start().unwrap();
        let src = Sources {
            dir: dir.clone(),
            live: None,
        };
        let ask = Ask {
            from_ms: NOW - HOUR,
            to_ms: NOW,
            app: None,
            mem_total: None,
        };
        w.mark(NOW - 10 * MIN, ask.clone(), src, NOW);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let reply = loop {
            if let Some(r) = w.take() {
                break r;
            }
            assert!(std::time::Instant::now() < until, "no reply");
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert_eq!(reply.mark_error, None);
        let marks: Vec<_> = reply
            .found
            .moments
            .iter()
            .filter(|m| m.kind == MomentKind::Manual)
            .collect();
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].start_ms, NOW - 10 * MIN);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
