//! The Testdeck pages: a header strip with the counts and the global switches, the banners, then the page the host
//! sidebar picked. Each frame copies what it draws out of the core in one short lock (`Snapshot`) and draws from the
//! copy.
mod agents;
mod ai;
mod disk;
mod graphs;
mod history_view;
mod lanes;
mod lists;
mod orphans;
mod queue;
mod running;
mod settings;
mod theme;
mod timeline;

use crate::config::Config;
use crate::history::{History, JobRow};
use crate::progress::{JobProgress, Tracker};
use crate::scheduler::{Kind, Ticket};
use crate::state::{JobLoad, LoadPoint, Shared};
use egui::{Align, RichText};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use theme::{AMBER, TEXT2, TEXT3};

/// The page indexes, in `crate::PAGES` order.
const HISTORY_PAGE: usize = 1;

pub(crate) struct RunRow {
    pub ticket: Ticket,
    pub kind: Kind,
    pub requester: String,
    pub command: String,
    pub log_glob: Option<String>,
    pub workers: u32,
    pub elapsed: f64,
    pub load: Option<JobLoad>,
    /// Average duration of this command in history.
    pub expected_secs: Option<f64>,
    pub below_normal: bool,
    pub stalled: bool,
    /// CPU cores the job is locked to; 0 = not locked.
    pub cpu_mask: u64,
}

pub(crate) struct QueueRow {
    pub ticket: Ticket,
    pub kind: Kind,
    pub requester: String,
    pub command: String,
    pub priority: i32,
    pub waited: f64,
    pub eta: Option<f64>,
    /// Why a gate sits in the queue without a start time.
    pub held: Option<String>,
    pub expected_secs: Option<f64>,
}

pub(crate) struct Snapshot {
    /// The broker clock as unix seconds, to place history rows on the same axis.
    pub now_unix: f64,
    pub running: Vec<RunRow>,
    pub queue: Vec<QueueRow>,
    pub slots: u32,
    pub paused: bool,
    /// Manual toggle or auto-detected.
    pub user_mode: bool,
    pub user_manual: bool,
    pub idle_secs: Option<u64>,
    pub foreign_busy: u32,
    pub cpu_avg: f32,
    pub ram_free_gb: f32,
    pub ram_total_gb: f32,
    pub points: Vec<LoadPoint>,
    pub config: Config,
    pub bound_port: Option<u16>,
    /// The gate waiting for Allow / Later, with its requester.
    pub gate_asking: Option<(Ticket, String)>,
}

/// What the pages need: the core, the history and the jest log tracker.
pub(crate) struct Hub {
    pub shared: Shared,
    pub history: Arc<History>,
    progress: Mutex<Polled>,
}

impl Hub {
    pub fn snapshot(&self) -> Snapshot {
        let shared = &self.shared;
        let now = shared.now();
        let gate_asking = shared.gate_asking();
        let c = shared.lock();
        let running = c
            .sched
            .running
            .iter()
            .map(|r| RunRow {
                ticket: r.ticket,
                kind: r.req.kind,
                requester: r.req.requester.clone(),
                command: r.req.command.clone(),
                log_glob: r.req.log_glob.clone(),
                workers: r.workers,
                elapsed: (now - r.granted_at).max(0.0),
                load: c.load.per_ticket.get(&r.ticket).copied(),
                expected_secs: c.job_stats.get(&r.req.command).copied(),
                below_normal: r.below_normal,
                stalled: r.stalled,
                cpu_mask: r.cpu_mask,
            })
            .collect();
        let gates_held = c.env(now).gates_held;
        let queue = c
            .sched
            .queue
            .iter()
            .map(|q| {
                let held = if q.req.kind != Kind::Gate {
                    None
                } else if gates_held {
                    Some(format!(
                        "waits for the night window ({})",
                        c.config.night_start
                    ))
                } else if c
                    .sched
                    .gate_snooze
                    .get(&q.ticket)
                    .is_some_and(|&until| now < until)
                {
                    Some("held by Later".to_string())
                } else if c.sched.gate_asking == Some(q.ticket) {
                    Some("waits for Allow".to_string())
                } else {
                    None
                };
                QueueRow {
                    ticket: q.ticket,
                    kind: q.req.kind,
                    requester: q.req.requester.clone(),
                    command: q.req.command.clone(),
                    priority: q.req.priority,
                    waited: (now - q.enqueued_at).max(0.0),
                    eta: if held.is_some() {
                        None
                    } else {
                        c.eta_secs(q.ticket, now)
                    },
                    held,
                    expected_secs: c.job_stats.get(&q.req.command).copied(),
                }
            })
            .collect();
        Snapshot {
            now_unix: shared.to_unix(now),
            running,
            queue,
            slots: c.slot_limit(),
            paused: c.sched.paused,
            user_mode: c.user_mode(),
            user_manual: c.user_mode_manual,
            idle_secs: c.idle_secs,
            foreign_busy: c.foreign_busy,
            cpu_avg: c.load.cpu_avg,
            ram_free_gb: c.load.ram_free_gb,
            ram_total_gb: c.load.ram_total_gb,
            points: c.load.history.iter().copied().collect(),
            config: c.config.clone(),
            bound_port: c.bound_port,
            gate_asking,
        }
    }

    /// Suite progress of the running jest jobs (reads only the new part of each log). The logs are polled at most
    /// once a second: a poll lists the log folder (all of %TEMP%) per job, and a frame can come many times a second.
    pub fn progress(&self, snap: &Snapshot) -> HashMap<Ticket, JobProgress> {
        let mut p = self.progress.lock().unwrap_or_else(|e| e.into_inner());
        let jest: HashSet<Ticket> = snap
            .running
            .iter()
            .filter(|r| r.kind == Kind::Jest)
            .map(|r| r.ticket)
            .collect();
        let fresh = p.at.is_some_and(|t| t.elapsed() < Polled::EVERY)
            && p.last.len() == jest.len()
            && jest.iter().all(|t| p.last.contains_key(t));
        if !fresh {
            let Polled { tracker, last, at } = &mut *p;
            last.clear();
            for r in snap.running.iter().filter(|r| r.kind == Kind::Jest) {
                last.insert(
                    r.ticket,
                    tracker.poll(r.ticket, r.log_glob.as_deref(), &r.command, &self.history),
                );
            }
            tracker.retain(&snap.running.iter().map(|r| r.ticket).collect());
            *at = Some(Instant::now());
        }
        p.last.clone()
    }
}

/// The jest log tracker and its last result.
#[derive(Default)]
struct Polled {
    tracker: Tracker,
    last: HashMap<Ticket, JobProgress>,
    at: Option<Instant>,
}

impl Polled {
    const EVERY: Duration = Duration::from_secs(1);
}

/// The progress of one running job, the same for the card and the timeline block.
pub(crate) struct Bar {
    /// None: neither a suite count nor a history average exists, so only a spinner can be shown.
    pub frac: Option<f32>,
    /// Short text: "76/118", "about 40s left" or "running".
    pub short: String,
    /// Longer text for the card.
    pub detail: String,
}

/// Item 1: jest uses done/expected suites; without a suite count, and for every other kind, elapsed time against the
/// history average of the same command (held at 99% while the job still runs, since the average is a guess).
pub(crate) fn job_bar(r: &RunRow, p: Option<&JobProgress>) -> Bar {
    let suites = p.and_then(|p| p.expected.filter(|e| *e > 0).map(|e| (p.done, e)));
    if let Some((done, e)) = suites {
        return Bar {
            frac: Some((done as f32 / e as f32).min(1.0)),
            short: format!("{done}/{e}"),
            detail: format!("{done} / {e} suites"),
        };
    }
    let done_text = p
        .map(|p| format!("{} suites done, ", p.done))
        .unwrap_or_default();
    match r.expected_secs.filter(|e| *e > 0.0) {
        Some(avg) => {
            let frac = (r.elapsed / avg).min(0.99) as f32;
            let (short, how) = if r.elapsed < avg {
                let left = format!("about {} left", fmt_dur(avg - r.elapsed));
                (left.clone(), left)
            } else {
                (
                    "almost done".to_string(),
                    format!("longer than the usual {}", fmt_dur(avg)),
                )
            };
            Bar {
                frac: Some(frac),
                short,
                detail: format!("{done_text}{how}"),
            }
        }
        None => Bar {
            frac: None,
            short: "running".into(),
            detail: format!("{done_text}running {}", fmt_dur(r.elapsed)),
        },
    }
}

/// A short-lived copy of the newest history rows, for the timeline and the Agents page.
#[derive(Default)]
struct Recent {
    rows: Vec<JobRow>,
    at: Option<Instant>,
}

impl Recent {
    const EVERY: Duration = Duration::from_secs(4);

    fn refresh(&mut self, history: &History) {
        if self.at.is_none_or(|t| t.elapsed() >= Self::EVERY) {
            self.rows = history.recent("", 1000);
            self.at = Some(Instant::now());
        }
    }
}

pub struct TestdeckApp {
    hub: Arc<Hub>,
    /// A page change the page itself asked for (Agents opens History), for the host to follow.
    go_to: Option<usize>,
    kill_pending: Option<Ticket>,
    recent: Recent,
    settings: settings::State,
    hist_view: history_view::State,
    orphans: orphans::State,
    disk: disk::State,
    lanes: lanes::State,
    agents: agents::State,
    ai: ai::State,
}

impl TestdeckApp {
    pub fn new(shared: Shared, history: Arc<History>) -> Self {
        Self {
            hub: Arc::new(Hub {
                shared,
                history,
                progress: Mutex::default(),
            }),
            go_to: None,
            kill_pending: None,
            recent: Recent::default(),
            settings: settings::State::default(),
            hist_view: history_view::State::default(),
            orphans: orphans::State::default(),
            disk: disk::State::default(),
            lanes: lanes::State::default(),
            agents: agents::State::default(),
            ai: ai::State::default(),
        }
    }

    /// The page index the page asked to move to since the last call.
    pub fn take_page(&mut self) -> Option<usize> {
        self.go_to.take()
    }

    /// The header strip, the banners and page `page` (an index into `crate::PAGES`).
    pub fn show(&mut self, ui: &mut egui::Ui, page: usize, broker_error: Option<&str>) {
        ui.ctx().request_repaint_after(Duration::from_secs(1));
        let hub = Arc::clone(&self.hub);
        let snap = hub.snapshot();
        let progress = hub.progress(&snap);
        self.recent.refresh(&hub.history);
        self.agents.refresh(&self.recent.rows, snap.now_unix);

        self.header(ui, &snap);
        if let Some(e) = broker_error {
            ui.label(RichText::new(e).color(theme::RED));
        }
        if snap.paused || snap.user_mode || snap.foreign_busy > 0 {
            self.banner(ui, &snap);
        }
        if let Some((t, who)) = &snap.gate_asking {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!(
                        "The gate from {who} can start now, but you are using the PC."
                    ))
                    .color(AMBER),
                );
                if theme::small_button(ui, "Allow")
                    .on_hover_text("Start the gate now")
                    .clicked()
                {
                    hub.shared.allow_gate(*t);
                }
                if theme::small_button(ui, "Later")
                    .on_hover_text("Hold the gate for 30 minutes")
                    .clicked()
                {
                    hub.shared.snooze_gate(*t);
                }
            });
        }
        ui.separator();
        match page {
            0 => self.overview(ui, &snap, &progress),
            1 => history_view::show(ui, &mut self.hist_view, &hub.history),
            2 => {
                if let Some(who) = agents::show(ui, &mut self.agents, &snap, &progress) {
                    self.hist_view.set_filter(who);
                    self.go_to = Some(HISTORY_PAGE);
                }
            }
            3 => lanes::show(ui, &mut self.lanes, &hub.shared),
            4 => orphans::show(ui, &mut self.orphans, &hub.shared),
            5 => disk::show(
                ui,
                &mut self.disk,
                &hub.shared,
                &hub.history,
                snap.config.disk_limit_gb,
            ),
            6 => ai::show(ui, &mut self.ai, &hub.shared),
            _ => settings::show(ui, &mut self.settings, &snap.config, &hub.shared),
        }
    }

    fn header(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        ui.horizontal(|ui| {
            // Idle boost runs jobs beyond the slots: say so, so "5 of 4" does not look like a bug.
            let jobs = snap.running.iter().filter(|r| r.kind != Kind::Tsc).count();
            let extra = jobs.saturating_sub(snap.slots as usize);
            let extra = if extra > 0 { format!(" (+{extra} extra)") } else { String::new() };
            ui.label(theme::mono(format!("{} of {} slots{extra} \u{B7} {} queued", snap.running.len(), snap.slots, snap.queue.len())).color(TEXT2));
            let port = snap.bound_port.unwrap_or(snap.config.port);
            ui.label(RichText::new(format!("127.0.0.1:{port}")).small().color(TEXT3)).on_hover_text("The broker agents ask for a slot");
            if snap.config.night_window_enabled {
                ui.label(RichText::new(format!("Gate window {}\u{2013}{}", snap.config.night_start, snap.config.night_end)).small().color(TEXT3));
            }
            ui.add_space(12.0);
            load_slider(ui, &self.hub.shared, &snap.config);
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                let shared = &self.hub.shared;
                let label = if snap.paused { "Resume queue" } else { "Pause queue" };
                if theme::small_button(ui, label).on_hover_text("A paused queue grants nothing; running jobs continue").clicked() {
                    shared.set_paused(!snap.paused);
                }
                let hint = "Limits Testdeck to one slot and low priority so the PC stays responsive. \
                            Also switches on by itself when you use the keyboard or mouse (see Settings).";
                let mut on = snap.user_manual;
                if toolboks_shared::ui::toggle_switch(ui, &mut on).on_hover_text(hint).changed() {
                    shared.set_user_mode(on);
                }
                let label = if snap.user_mode && !snap.user_manual { "I am using the PC (auto)" } else { "I am using the PC" };
                ui.label(label).on_hover_text(hint);
            });
        });
    }

    /// One strip for the states that change what the queue does: paused, user mode, old scripts holding slots.
    fn banner(&self, ui: &mut egui::Ui, snap: &Snapshot) {
        ui.horizontal_wrapped(|ui| {
            if snap.paused {
                ui.label(
                    RichText::new("Queue paused: nothing is granted until you resume.")
                        .color(AMBER),
                );
            }
            if snap.user_mode {
                let how = if snap.user_manual {
                    "on".to_string()
                } else {
                    snap.idle_secs.map_or("auto".to_string(), |s| {
                        format!("auto, last input {} ago", fmt_dur(s as f64))
                    })
                };
                ui.label(
                    RichText::new(format!("User mode {how}: one slot, low priority.")).color(AMBER),
                );
            }
            if snap.foreign_busy > 0 {
                ui.label(
                    RichText::new(format!("{} slot(s) held by old scripts", snap.foreign_busy))
                        .color(TEXT2),
                );
            }
        });
    }

    fn overview(
        &mut self,
        ui: &mut egui::Ui,
        snap: &Snapshot,
        progress: &HashMap<Ticket, JobProgress>,
    ) {
        // Fixed to the panel (minus the scroll bar) so a wide row can never widen the whole page.
        let avail = ui.available_width() - 14.0;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_width(avail);
                ui.spacing_mut().item_spacing.y = 18.0;
                timeline::show(ui, snap, progress, &self.recent.rows);
                running::show(ui, snap, progress, &mut self.kill_pending, &self.hub.shared);
                let w = ui.available_width();
                let (left, right) = ((w - 14.0) * 0.58, (w - 14.0) * 0.42);
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 14.0;
                    ui.allocate_ui_with_layout(
                        egui::vec2(left, 10.0),
                        egui::Layout::top_down(Align::Min),
                        |ui| {
                            ui.set_width(left);
                            queue::show(ui, snap, &self.hub.shared);
                        },
                    );
                    ui.allocate_ui_with_layout(
                        egui::vec2(right, 10.0),
                        egui::Layout::top_down(Align::Min),
                        |ui| {
                            ui.set_width(right);
                            graphs::show(ui, snap);
                        },
                    );
                });
            });
    }
}

/// Applies the Toolboks look (fonts, text sizes, spacing, colours) the first time the add-on draws. The add-on runs in
/// its own process, so the host's own set-up does not reach it.
pub fn style_once(ctx: &egui::Context) {
    let id = egui::Id::NULL.with("toolboks-style-applied");
    if !ctx.data(|data| data.get_temp::<bool>(id)).unwrap_or(false) {
        toolboks_shared::ui::apply_style(ctx);
        ctx.data_mut(|data| data.insert_temp(id, true));
    }
}

/// Why Testdeck is not running, in place of the pages.
pub fn error_card(ui: &mut egui::Ui, text: &str) {
    theme::card().show(ui, |ui| {
        theme::title(ui, "Testdeck is not running");
        ui.label(RichText::new(text).color(theme::RED));
    });
}

/// "45s", "3m 05s", "1h 05m".
pub(crate) fn fmt_dur(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m {:02}s", s / 60, s % 60),
        _ => format!("{}h {:02}m", s / 3600, (s % 3600) / 60),
    }
}

pub(crate) fn ok_color(_ui: &egui::Ui) -> egui::Color32 {
    theme::ok_color()
}

pub(crate) fn kind_chip_text(ui: &mut egui::Ui, kind: &str) {
    theme::kind_tag(ui, kind);
}

/// Monospace command on one line, cut at `max_w`; the full text shows on hover.
pub(crate) fn command_label(ui: &mut egui::Ui, text: &str, max_w: f32) {
    ui.scope(|ui| {
        ui.set_max_width(max_w);
        ui.add(
            egui::Label::new(RichText::new(text).monospace().size(11.0).color(TEXT3)).truncate(),
        )
        .on_hover_text(text);
    });
}

/// The last three path components, for narrow cells.
pub(crate) fn short_path(p: &str) -> String {
    let parts: Vec<&str> = p.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    if parts.len() <= 3 {
        parts.join("/")
    } else {
        format!(".../{}", parts[parts.len() - 3..].join("/"))
    }
}

/// The test load slider in the header: Light to Heavy, applied and saved at once. Custom when the limits were set by
/// hand; the knob then sits on the preset with the nearest slot count.
fn load_slider(ui: &mut egui::Ui, shared: &crate::state::Shared, cfg: &Config) {
    use crate::config::{threads, LOAD_LEVELS};
    let t = threads();
    let preset = |l: u8| {
        let mut c = cfg.clone();
        c.set_load(l, t);
        c
    };
    let mut v = cfg.load_level.unwrap_or_else(|| {
        (0..LOAD_LEVELS.len() as u8)
            .min_by_key(|&l| preset(l).slots.abs_diff(cfg.slots))
            .unwrap_or(2)
    });
    let p = preset(v);
    let tip = format!(
        "{}: {} job{} at once plus {} type check{}, {} workers shared (this PC has {t} threads), new jobs wait above {:.0} % CPU or below {:.0} GB free RAM",
        LOAD_LEVELS[v as usize],
        p.slots,
        if p.slots == 1 { "" } else { "s" },
        p.tsc_slots,
        if p.tsc_slots == 1 { "" } else { "s" },
        p.worker_budget,
        p.cpu_threshold,
        p.ram_free_min_gb,
    );
    ui.label(egui::RichText::new("Test load").color(TEXT2));
    ui.spacing_mut().slider_width = 90.0;
    let r = ui
        .add(egui::Slider::new(&mut v, 0..=LOAD_LEVELS.len() as u8 - 1).show_value(false))
        .on_hover_text(&tip);
    let name = if cfg.load_level.is_some() {
        LOAD_LEVELS[v as usize]
    } else {
        "Custom"
    };
    ui.label(theme::mono(name)).on_hover_text(&tip);
    if r.changed() {
        let _ = shared.set_config(preset(v));
    }
}
