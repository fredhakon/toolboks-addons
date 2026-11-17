//! Small graphs of the selected row in the Processes inspector: CPU, memory, disk, GPU and
//! connections over the last two minutes. Kept for one selected row only, and dropped as soon
//! as that row is no longer the single selection, so nothing is kept for other rows.

use egui::{Color32, RichText, Sense, Vec2};
use toolboks_shared::format::{rate, size_plain};
use toolboks_shared::ui::{DATA_CPU, DATA_DISK, DATA_GPU, DATA_MEMORY, DATA_NETWORK};

use crate::history::{History, Range};

/// Points shown: the newest `Range::Minute` points, one per process refresh (2 s), so about
/// two minutes.
const RANGE: Range = Range::Minute;
const SPARK_HEIGHT: f32 = 22.0;

/// One value per graph: CPU %, memory bytes, disk bytes/s, GPU %, open connections. NaN where
/// the value is not known yet (it leaves a gap in the line).
pub(super) type Values = [f32; 5];

/// The history of one selected row.
pub(super) struct OwnGraphs {
    key: String,
    series: [History; 5],
}

impl OwnGraphs {
    fn new(key: &str) -> Self {
        Self {
            key: key.to_string(),
            series: std::array::from_fn(|_| History::new()),
        }
    }

    pub(super) fn key(&self) -> &str {
        &self.key
    }

    pub(super) fn push(&mut self, values: Values) {
        for (h, v) in self.series.iter_mut().zip(values) {
            h.push(v);
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.series[0].last().is_none()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.series[0].view(RANGE).len()
    }
}

/// Keeps the graphs in step with the selection: a new row starts empty graphs, the same row
/// keeps them, and no single selected row drops them.
pub(super) fn follow(graphs: &mut Option<OwnGraphs>, selected: Option<&str>) {
    match selected {
        Some(key) if graphs.as_ref().is_some_and(|g| g.key == key) => {}
        Some(key) => *graphs = Some(OwnGraphs::new(key)),
        None => *graphs = None,
    }
}

/// Text of the newest value of graph `i`.
fn value_text(i: usize, v: f32) -> String {
    if !v.is_finite() {
        return "...".to_string();
    }
    match i {
        0 | 3 => format!("{v:.1}%"),
        1 => size_plain(v.max(0.0) as u64),
        2 => rate(f64::from(v)),
        _ => format!("{v:.0}"),
    }
}

/// The top of a graph: the largest value shown, never below a floor, so a quiet process draws a
/// flat line at the bottom instead of noise stretched to full height.
fn scale(values: &[f32], floor: f32) -> f32 {
    values
        .iter()
        .copied()
        .filter(|v| v.is_finite())
        .fold(floor, f32::max)
}

pub(super) fn ui(ui: &mut egui::Ui, graphs: &OwnGraphs) {
    const NAMES: [&str; 5] = ["CPU", "Memory", "Disk", "GPU", "Connections"];
    const COLORS: [Color32; 5] = [DATA_CPU, DATA_MEMORY, DATA_DISK, DATA_GPU, DATA_NETWORK];
    // CPU and GPU 5 %, memory 10 MB, disk 100 KB/s, 5 connections.
    const FLOORS: [f32; 5] = [5.0, 10_000_000.0, 100_000.0, 5.0, 5.0];
    ui.label(RichText::new("Last 2 minutes").small().weak());
    for (i, h) in graphs.series.iter().enumerate() {
        let values = h.view(RANGE);
        let newest = values.last().copied().unwrap_or(f32::NAN);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let label = RichText::new(format!("{} {}", NAMES[i], value_text(i, newest))).small();
            ui.add_sized(
                Vec2::new(96.0, SPARK_HEIGHT),
                egui::Label::new(label).truncate(),
            );
            let width = ui.available_width().max(40.0);
            let (rect, _) = ui.allocate_exact_size(Vec2::new(width, SPARK_HEIGHT), Sense::hover());
            crate::list::paint_spark(
                ui.painter(),
                rect.shrink2(Vec2::new(0.0, 2.0)),
                values,
                RANGE.points(),
                scale(values, FLOORS[i]),
                COLORS[i],
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graphs_live_only_while_their_row_is_the_single_selection() {
        let mut graphs = None;
        follow(&mut graphs, Some("chrome.exe"));
        graphs.as_mut().unwrap().push([1.0, 2.0, 3.0, 4.0, 5.0]);
        // Same row: the history stays.
        follow(&mut graphs, Some("chrome.exe"));
        assert_eq!(graphs.as_ref().unwrap().len(), 1);
        // Another row: fresh, empty graphs; nothing of the old row is kept.
        follow(&mut graphs, Some("pid:4:1"));
        assert_eq!(graphs.as_ref().unwrap().key(), "pid:4:1");
        assert_eq!(graphs.as_ref().unwrap().len(), 0);
        // Deselected (or several rows selected): dropped.
        follow(&mut graphs, None);
        assert!(graphs.is_none());
    }

    #[test]
    fn values_read_plainly_and_scales_have_a_floor() {
        assert_eq!(value_text(0, 12.34), "12.3%");
        assert_eq!(value_text(4, 7.0), "7");
        assert_eq!(value_text(3, f32::NAN), "...");
        assert_eq!(scale(&[0.1, 0.2], 5.0), 5.0);
        assert_eq!(scale(&[0.1, f32::NAN, 40.0], 5.0), 40.0);
    }
}
