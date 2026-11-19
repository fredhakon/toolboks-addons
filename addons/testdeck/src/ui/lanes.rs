//! Lanes tab: the real-DB test lanes, which of their ports answer, who holds each lane, and a Start button for a
//! server that is down. Read-only otherwise; it never connects to the databases.
use super::{command_label, fmt_dur, ok_color};
use crate::lanes::{self, Holder, LaneDef, LaneState, Poller, Server, LANES};
use crate::state::Shared;
use egui::{RichText, Ui};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// After Start, the button stays off this long (or until the port answers) so a slow server is not started twice.
const START_GRACE: Duration = Duration::from_secs(20);

#[derive(Default)]
pub(crate) struct State {
    poller: Option<Poller>,
    started: HashMap<u16, Instant>,
    /// Ok = what was started, Err = what went wrong.
    note: Option<Result<String, String>>,
}

pub(crate) fn show(ui: &mut Ui, st: &mut State, shared: &Shared) {
    let poller = st.poller.get_or_insert_with(Poller::start);
    let states = poller.snapshot();
    st.started.retain(|_, t| t.elapsed() < START_GRACE);

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.weak("Local throwaway servers for the real-DB jest runs. Checked every 5 s by opening and closing a TCP connection.");
        if states.is_empty() {
            ui.add_space(8.0);
            ui.weak("Checking...");
        }
        for (def, s) in LANES.iter().zip(&states) {
            lane(ui, def, s, st, shared);
        }
        match &st.note {
            Some(Ok(m)) => {
                ui.add_space(8.0);
                ui.colored_label(ok_color(ui), m.as_str());
            }
            Some(Err(e)) => {
                ui.add_space(8.0);
                ui.colored_label(ui.visuals().error_fg_color, e.as_str());
            }
            None => {}
        }
    });
}

fn lane(ui: &mut Ui, def: &LaneDef, s: &LaneState, st: &mut State, shared: &Shared) {
    ui.add_space(8.0);
    super::theme::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        lane_body(ui, def, s, st, shared);
    });
}

fn lane_body(ui: &mut Ui, def: &LaneDef, s: &LaneState, st: &mut State, shared: &Shared) {
    ui.label(RichText::new(format!("Lane {}", def.name)).heading());
    ui.add_space(2.0);
    egui::Grid::new(format!("lane-{}", def.name))
        .num_columns(3)
        .min_col_width(110.0)
        .spacing([16.0, 7.0])
        .show(ui, |ui| {
            server_row(ui, &def.mysql, s.mysql_open, st, shared);
            server_row(ui, &def.redis, s.redis_open, st, shared);
            ui.label("Held by");
            match &s.holder {
                None => {
                    ui.colored_label(ok_color(ui), "free");
                }
                Some(h) => holder(ui, h),
            }
            ui.end_row();
        });
}

fn server_row(ui: &mut Ui, sv: &Server, open: bool, st: &mut State, shared: &Shared) {
    ui.label(format!("{} :{}", sv.label, sv.port));
    if open {
        ui.colored_label(ok_color(ui), "open");
    } else {
        ui.colored_label(ui.visuals().warn_fg_color, "closed");
    }
    let waiting = st.started.contains_key(&sv.port);
    let has_script = sv.script_exists();
    let btn = ui.add_enabled(!open && !waiting && has_script, egui::Button::new("Start"));
    let btn = if has_script {
        btn.on_hover_text(format!(
            "Runs {} through Git Bash; output goes to {}",
            sv.script, sv.log
        ))
    } else {
        btn.on_disabled_hover_text(format!("No start script at {}", sv.script))
    };
    if btn.clicked() {
        st.note = Some(match lanes::start_server(sv, &lanes::data_dir(shared)) {
            Ok(()) => {
                st.started.insert(sv.port, Instant::now());
                Ok(format!(
                    "Started {} :{}; log {}.",
                    sv.label, sv.port, sv.log
                ))
            }
            Err(e) => Err(format!("Could not start {} :{}: {e}", sv.label, sv.port)),
        });
    }
    ui.end_row();
}

fn holder(ui: &mut Ui, h: &Holder) {
    ui.colored_label(ui.visuals().warn_fg_color, "in use");
    ui.horizontal(|ui| {
        let pid = if h.pid.is_empty() {
            "starting".to_string()
        } else {
            format!("pid {}", h.pid)
        };
        let age = h
            .age_secs
            .map(|a| format!(", {}", fmt_dur(a as f64)))
            .unwrap_or_default();
        ui.weak(format!("{pid}{age}"));
        if !h.cmd.is_empty() {
            command_label(ui, &h.cmd, (ui.available_width() - 20.0).max(120.0));
        }
    });
}
