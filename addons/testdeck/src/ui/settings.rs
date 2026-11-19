//! Settings: edits a draft of every `Config` field; Apply saves and applies it live.
use crate::config::Config;
use crate::state::Shared;
use egui::{DragValue, Ui};

#[derive(Default)]
pub(crate) struct State {
    draft: Option<Config>,
    /// The config as of the last frame: a draft without edits follows changes made elsewhere (the test load slider).
    base: Option<Config>,
    /// Ok = saved, Err = what went wrong.
    result: Option<Result<(), String>>,
    lists: super::lists::State,
}

pub(crate) fn show(ui: &mut Ui, st: &mut State, current: &Config, shared: &Shared) {
    let State {
        draft,
        base,
        result,
        lists,
    } = st;
    // Taken out while drawing so the closures below can borrow it; put back unless reverted.
    let mut d = match draft.take() {
        Some(d) if base.as_ref() == Some(current) || base.as_ref() != Some(&d) => d,
        _ => current.clone(),
    };
    *base = Some(current.clone());
    let dirty = d != *current;
    let times_ok = valid_time(&d.night_start) && valid_time(&d.night_end);
    let mut revert = false;

    // Leave room for the separator and the Apply row below, or the full-height scroll area pushes them out of view.
    let room = (ui.available_height() - 40.0).max(120.0);
    egui::ScrollArea::vertical().auto_shrink([false, false]).max_height(room).show(ui, |ui| {
        section(ui, "scheduling", "Scheduling", |ui| {
            num(ui, "Slots", "Jobs that may run at the same time", &mut d.slots, 1..=8, "");
            let auto = crate::scheduler::worker_budget(&Config { worker_budget: 0, ..Config::default() }, crate::config::threads());
            let hint = format!("Shared by the running jobs: a job alone gets all of them, the gate always does. 0 uses every core but two ({auto} here)");
            num(ui, "Worker budget", &hint, &mut d.worker_budget, 0..=64, "");
            row(ui, "RAM per worker", "A job gets no more workers than free RAM above \"Free RAM needed\" allows", |ui| {
                ui.add(DragValue::new(&mut d.ram_per_worker_gb).range(0.1..=8.0).speed(0.05).fixed_decimals(2).suffix(" GB"));
            });
            num(ui, "Workers in user mode", "Used while you are using the PC", &mut d.workers_user_mode, 1..=8, "");
            flag(ui, "Gate waits for the whole PC", "A gate job starts only when nothing else runs", &mut d.gate_exclusive);
            flag(ui, "Gate goes first", "Gate jobs jump the queue", &mut d.gate_first);
            flag(ui, "Split cores between jobs", "Locks each job (not the gate) to its own cores: a core per worker, or a share of the budget when more jobs run", &mut d.cpu_cap);
        });
        section(ui, "limits", "Load limits", |ui| {
            pct(ui, "CPU limit", "New jobs wait while the 10 s CPU average is above this", &mut d.cpu_threshold);
            gb(ui, "Free RAM needed", "New jobs wait while free RAM is below this", &mut d.ram_free_min_gb);
        });
        section(ui, "usermode", "User mode", |ui| {
            flag(ui, "Detect PC use", "Keyboard or mouse input switches user mode on", &mut d.auto_user_mode);
            // Disable the grid's own ui, not a nested scope: a scope is one grid cell and collapses the row into column 1.
            if !d.auto_user_mode {
                ui.disable();
            }
            num(ui, "Input counts for", "Minutes after the last input that still count as using the PC", &mut d.idle_minutes, 1..=240, " min");
        });
        section(ui, "savers", "Token savers", |ui| {
            flag(ui, "Cache identical runs", "A jest run with the same command, code and changes gets the stored summary instead of running again", &mut d.cache_enabled);
            if !d.cache_enabled {
                ui.disable();
            }
            num(ui, "Cache lifetime", "A stored result older than this is dropped", &mut d.cache_ttl_hours, 1..=720, " h");
        });
        section(ui, "safety", "Safety", |ui| {
            num(ui, "Stalled after", "A job with no new PASS or FAIL line for this long is marked stalled and offered a kill", &mut d.stall_minutes, 1..=240, " min");
            gb(ui, "Kill below free RAM", "Below this the newest job that is not a gate is stopped and put back at the head of the queue", &mut d.ram_kill_free_gb);
            num(ui, "Toast after", "A toast shows when a gate, or any run at least this long, ends", &mut d.toast_min_minutes, 0..=240, " min");
            flag(ui, "Ask before a gate in user mode", "A toast with Allow or Later; Later waits 30 minutes", &mut d.gate_ask_in_user_mode);
        });
        section(ui, "night", "Night window", |ui| {
            flag(ui, "Gates only at night", "Outside the window a queued gate is skipped and does not block other jobs", &mut d.night_window_enabled);
            if !d.night_window_enabled {
                ui.disable();
            }
            time(ui, "From", "Local time, 24 hour clock", &mut d.night_start);
            time(ui, "Until", "May be earlier than From: the window then crosses midnight", &mut d.night_end);
        });
        section(ui, "broker", "Broker", |ui| {
            num(ui, "Port", "On 127.0.0.1 only. A change rebinds at once; clients must use the new port", &mut d.port, 1024..=65535, "");
            num(ui, "Heartbeat timeout", "A running job that stays silent this long is released", &mut d.heartbeat_timeout_secs, 5..=600, " s");
            row(ui, "Slot folder", "Empty uses %TEMP%\\jt-slots, where the old scripts look", |ui| {
                ui.add(egui::TextEdit::singleline(&mut d.mirror_dir).hint_text("%TEMP%\\jt-slots").desired_width(220.0));
            });
        });
        ui.add_space(8.0);
        ui.label(egui::RichText::new("Suite lists").heading());
        ui.add_space(2.0);
        super::lists::flaky(ui, lists, &mut d.flaky_suites);
        ui.add_space(10.0);
        super::lists::known_red(ui, lists, shared);
        ui.add_space(8.0);
    });

    ui.separator();
    ui.horizontal(|ui| {
        if ui
            .add_enabled(dirty && times_ok, egui::Button::new("Apply"))
            .clicked()
        {
            *result = Some(apply(&d, shared));
        }
        if ui.add_enabled(dirty, egui::Button::new("Revert")).clicked() {
            revert = true;
            *result = None;
        }
        match result {
            Some(Err(e)) => {
                ui.colored_label(ui.visuals().error_fg_color, e.as_str());
            }
            Some(Ok(())) if !dirty => {
                ui.colored_label(super::ok_color(ui), "Applied and saved.");
            }
            _ if dirty && !times_ok => {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    "Night window times must be HH:MM, for example 22:00.",
                );
            }
            _ if dirty => {
                ui.weak("Unsaved changes");
            }
            _ => {}
        }
    });
    if !revert {
        *draft = Some(d);
    }
}

fn apply(d: &Config, shared: &Shared) -> Result<(), String> {
    shared
        .set_config(d.clone())
        .map_err(|e| format!("Applied, but saving config.toml failed: {e:#}"))
}

fn section(ui: &mut Ui, id: &str, title: &str, rows: impl FnOnce(&mut Ui)) {
    ui.add_space(8.0);
    ui.label(egui::RichText::new(title).heading());
    ui.add_space(2.0);
    egui::Grid::new(id)
        .num_columns(3)
        .min_col_width(170.0)
        .spacing([16.0, 7.0])
        .show(ui, rows);
}

fn row(ui: &mut Ui, label: &str, hint: &str, control: impl FnOnce(&mut Ui)) {
    ui.label(label);
    control(ui);
    // Last grid column: wraps at the window edge instead of running past it.
    ui.add(egui::Label::new(egui::RichText::new(hint).weak()).wrap());
    ui.end_row();
}

fn num<T: egui::emath::Numeric>(
    ui: &mut Ui,
    label: &str,
    hint: &str,
    v: &mut T,
    range: std::ops::RangeInclusive<T>,
    suffix: &str,
) {
    row(ui, label, hint, |ui| {
        ui.add(DragValue::new(v).range(range).suffix(suffix));
    });
}

fn pct(ui: &mut Ui, label: &str, hint: &str, v: &mut f32) {
    row(ui, label, hint, |ui| {
        ui.add(
            DragValue::new(v)
                .range(5.0..=100.0)
                .speed(0.5)
                .fixed_decimals(0)
                .suffix(" %"),
        );
    });
}

fn gb(ui: &mut Ui, label: &str, hint: &str, v: &mut f32) {
    row(ui, label, hint, |ui| {
        ui.add(
            DragValue::new(v)
                .range(0.0..=128.0)
                .speed(0.1)
                .fixed_decimals(1)
                .suffix(" GB"),
        );
    });
}

/// "HH:MM", 00:00 to 23:59.
fn valid_time(t: &str) -> bool {
    let b = t.as_bytes();
    b.len() == 5
        && b[2] == b':'
        && [0, 1, 3, 4].iter().all(|&i| b[i].is_ascii_digit())
        && t[..2].parse::<u8>().is_ok_and(|h| h < 24)
        && t[3..].parse::<u8>().is_ok_and(|m| m < 60)
}

fn time(ui: &mut Ui, label: &str, hint: &str, v: &mut String) {
    let ok = valid_time(v);
    let hint = if ok {
        hint
    } else {
        "Not a time. Use HH:MM, for example 22:00"
    };
    row(ui, label, hint, |ui| {
        let mut edit = egui::TextEdit::singleline(v)
            .char_limit(5)
            .desired_width(48.0)
            .hint_text("HH:MM");
        if !ok {
            edit = edit.text_color(ui.visuals().error_fg_color);
        }
        ui.add(edit);
    });
}

fn flag(ui: &mut Ui, label: &str, hint: &str, v: &mut bool) {
    row(ui, label, hint, |ui| {
        toolboks_shared::ui::toggle_switch(ui, v);
    });
}
