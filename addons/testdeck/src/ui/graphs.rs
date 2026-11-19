//! CPU and RAM for the last five minutes, as a Toolboks graph card.
use super::Snapshot;
use crate::state::LOAD_HISTORY_SECS;
use egui::Ui;
use toolboks_shared::ui::{graph_card, GraphCard, Line, DATA_DISK, DATA_NETWORK};

fn percent(v: f32) -> String {
    format!("{v:.0}%")
}

pub(crate) fn show(ui: &mut Ui, snap: &Snapshot) {
    let total = snap.ram_total_gb.max(1.0);
    let cpu: Vec<f32> = snap.points.iter().map(|p| p.cpu).collect();
    let ram: Vec<f32> = snap
        .points
        .iter()
        .map(|p| (total - p.ram_free_gb).clamp(0.0, total) / total * 100.0)
        .collect();
    let lines = [
        Line {
            values: &cpu,
            color: DATA_DISK,
            fill: true,
            label: "CPU",
            fmt: percent,
        },
        Line {
            values: &ram,
            color: DATA_NETWORK,
            fill: false,
            label: "RAM used",
            fmt: percent,
        },
    ];
    let stats = [
        (format!("{:.0}%", snap.cpu_avg), "CPU 10 s"),
        (format!("{:.1} GB", snap.ram_free_gb), "free"),
        (format!("{:.0}%", snap.config.cpu_threshold), "CPU limit"),
    ];
    let card = GraphCard {
        title: "Load",
        detail: "last 5 min",
        lines: &lines,
        capacity: LOAD_HISTORY_SECS as usize,
        max: 100.0,
        step_secs: 1.0,
        stats: &stats,
        note: cpu.is_empty().then_some("Waiting for the first reading"),
    };
    graph_card(ui, ui.available_width(), 170.0, &card);
}
