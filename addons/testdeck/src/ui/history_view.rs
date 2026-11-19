//! History tab: a filterable table of ended jobs, and the averages per command and per failing suite.
use super::{command_label, fmt_dur, kind_chip_text, ok_color, short_path};
use crate::history::{CommandStat, History, JobRow, SuiteStat};
use egui::RichText;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const RELOAD_AFTER_SECS: u64 = 5;
const ROW_LIMIT: u32 = 300;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Mode {
    #[default]
    Jobs,
    Averages,
}

#[derive(Default)]
pub(crate) struct State {
    filter: String,
    mode: Mode,
    rows: Vec<JobRow>,
    commands: Vec<CommandStat>,
    suites: Vec<SuiteStat>,
    loaded: Option<(Instant, String)>,
}

impl State {
    /// Opens the Jobs list filtered by a requester (the Agents page links here).
    pub(crate) fn set_filter(&mut self, filter: String) {
        self.filter = filter;
        self.mode = Mode::Jobs;
        self.loaded = None;
    }

    fn reload_if_stale(&mut self, history: &History) {
        let fresh = self
            .loaded
            .as_ref()
            .is_some_and(|(at, f)| *f == self.filter && at.elapsed().as_secs() < RELOAD_AFTER_SECS);
        if fresh {
            return;
        }
        self.rows = history.recent(&self.filter, ROW_LIMIT);
        self.commands = history.command_stats();
        self.suites = history.suite_stats();
        self.loaded = Some((Instant::now(), self.filter.clone()));
    }
}

pub(crate) fn show(ui: &mut egui::Ui, st: &mut State, history: &History) {
    st.reload_if_stale(history);
    ui.horizontal(|ui| {
        ui.selectable_value(&mut st.mode, Mode::Jobs, "Jobs");
        ui.selectable_value(&mut st.mode, Mode::Averages, "Averages");
        ui.add_space(12.0);
        if st.mode == Mode::Jobs {
            ui.add(
                egui::TextEdit::singleline(&mut st.filter)
                    .hint_text("Filter by requester, command or kind")
                    .desired_width(280.0),
            );
            if !st.filter.is_empty() && ui.button("Clear").clicked() {
                st.filter.clear();
            }
        }
    });
    ui.add_space(6.0);
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| match st.mode {
            Mode::Jobs => jobs_table(ui, &st.rows),
            Mode::Averages => averages(ui, &st.commands, &st.suites),
        });
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// "5m ago", "3h ago", "2d ago".
fn ago(unix: f64) -> String {
    let s = (unix_now() - unix).max(0.0);
    if s < 60.0 {
        "just now".to_string()
    } else if s < 3600.0 {
        format!("{}m ago", (s / 60.0) as u64)
    } else if s < 86_400.0 {
        format!("{}h ago", (s / 3600.0) as u64)
    } else {
        format!("{}d ago", (s / 86_400.0) as u64)
    }
}

fn jobs_table(ui: &mut egui::Ui, rows: &[JobRow]) {
    if rows.is_empty() {
        ui.weak("No finished jobs match.");
        return;
    }
    let cmd_w = (ui.available_width() - 640.0).clamp(140.0, 480.0);
    egui::Grid::new("history_jobs")
        .num_columns(8)
        .striped(true)
        .spacing([14.0, 6.0])
        .show(ui, |ui| {
            for h in [
                "Ended",
                "Requester",
                "Kind",
                "Command",
                "Duration",
                "Result",
                "Tests",
                "Failing suites",
            ] {
                ui.weak(h);
            }
            ui.end_row();
            for r in rows {
                ui.label(ago(r.ended_unix));
                ui.strong(&r.requester);
                kind_chip_text(ui, &r.kind);
                command_label(ui, &r.command, cmd_w);
                ui.label(fmt_dur(r.duration_secs));
                result_cell(ui, r);
                tests_cell(ui, r);
                failing_cell(ui, r);
                ui.end_row();
            }
        });
}

fn result_cell(ui: &mut egui::Ui, r: &JobRow) {
    if r.end_reason != "released" {
        ui.colored_label(ui.visuals().warn_fg_color, &r.end_reason);
        return;
    }
    match r.exit_code {
        Some(0) => ui.colored_label(ok_color(ui), "exit 0"),
        Some(n) => ui.colored_label(ui.visuals().error_fg_color, format!("exit {n}")),
        None => ui.weak("unknown"),
    };
}

fn tests_cell(ui: &mut egui::Ui, r: &JobRow) {
    match (r.passed, r.failed) {
        (Some(p), Some(f)) => {
            let text = format!("{p} passed, {f} failed");
            let resp = ui.label(text);
            if let Some(n) = r.suites_total {
                resp.on_hover_text(format!("{n} suites"));
            }
        }
        _ => {
            ui.weak("-");
        }
    }
}

fn failing_cell(ui: &mut egui::Ui, r: &JobRow) {
    if r.failing_suites.is_empty() {
        ui.weak("-");
        return;
    }
    let text = RichText::new(format!("{} failing", r.failing_suites.len()))
        .color(ui.visuals().error_fg_color);
    ui.label(text).on_hover_ui(|ui| {
        for s in r.failing_suites.iter().take(15) {
            ui.label(RichText::new(s).monospace().small());
        }
        if r.failing_suites.len() > 15 {
            ui.weak(format!("and {} more", r.failing_suites.len() - 15));
        }
    });
}

fn averages(ui: &mut egui::Ui, commands: &[CommandStat], suites: &[SuiteStat]) {
    ui.label(RichText::new("Per command").heading());
    ui.weak("Jobs that ran to the end. The queue ETA uses these averages.");
    ui.add_space(4.0);
    if commands.is_empty() {
        ui.weak("No history yet.");
    } else {
        let cmd_w = (ui.available_width() - 420.0).clamp(160.0, 560.0);
        egui::Grid::new("avg_commands")
            .num_columns(6)
            .striped(true)
            .spacing([14.0, 6.0])
            .show(ui, |ui| {
                for h in [
                    "Command",
                    "Kind",
                    "Runs",
                    "Average",
                    "Non-zero exits",
                    "Last run",
                ] {
                    ui.weak(h);
                }
                ui.end_row();
                for c in commands {
                    command_label(ui, &c.command, cmd_w);
                    kind_chip_text(ui, &c.kind);
                    ui.label(c.runs.to_string());
                    ui.label(fmt_dur(c.avg_secs));
                    if c.failed_runs > 0 {
                        ui.colored_label(ui.visuals().error_fg_color, c.failed_runs.to_string());
                    } else {
                        ui.weak("0");
                    }
                    ui.label(ago(c.last_unix));
                    ui.end_row();
                }
            });
    }
    ui.add_space(18.0);
    ui.label(RichText::new("Per failing suite").heading());
    ui.weak("History keeps no per-suite timing, so the duration is that of the jobs the suite failed in.");
    ui.add_space(4.0);
    if suites.is_empty() {
        ui.weak("No failing suites recorded.");
        return;
    }
    egui::Grid::new("avg_suites")
        .num_columns(4)
        .striped(true)
        .spacing([14.0, 6.0])
        .show(ui, |ui| {
            for h in ["Suite", "Failed in", "Average job", "Last failure"] {
                ui.weak(h);
            }
            ui.end_row();
            for s in suites {
                ui.scope(|ui| {
                    ui.set_max_width((ui.available_width() - 360.0).clamp(160.0, 560.0));
                    ui.add(
                        egui::Label::new(RichText::new(short_path(&s.suite)).monospace().small())
                            .truncate(),
                    )
                    .on_hover_text(&s.suite);
                });
                ui.label(format!(
                    "{} job{}",
                    s.failures,
                    if s.failures == 1 { "" } else { "s" }
                ));
                ui.label(fmt_dur(s.avg_job_secs));
                ui.label(ago(s.last_unix));
                ui.end_row();
            }
        });
}
