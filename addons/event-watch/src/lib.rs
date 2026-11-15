//! Event Watch: reads the Windows event logs and shows live errors and what happened before a
//! bluescreen, a hard reset or a hardware fault. All Event Log work runs on worker threads.
//! It only reads: it never clears a log and needs no administrator rights.

pub mod analysis;
pub mod bugchecks;
pub mod catalog;
pub mod model;
mod page;
pub mod source;

use std::collections::BTreeMap;
use toolboks_addon_sdk::{NativeAddon, TutorialStep};

pub const ID: &str = "toolboks.event-watch";
/// Page names, in order. They match `host-services.json`.
pub const PAGES: [&str; 3] = ["Live", "Crashes", "Summary"];

pub struct EventWatch {
    page: page::Page,
    index: usize,
}

impl Default for EventWatch {
    fn default() -> Self {
        Self {
            page: page::Page::new(),
            index: 0,
        }
    }
}

impl NativeAddon for EventWatch {
    fn ui(
        &mut self,
        ui: &mut egui::Ui,
        _preferences: &mut BTreeMap<String, serde_json::Value>,
    ) -> bool {
        self.page.ui(ui, self.index);
        false
    }
    fn navigate(&mut self, page: &str) {
        self.index = page.parse::<usize>().unwrap_or(0).min(PAGES.len() - 1);
    }
    fn page_index(&self) -> usize {
        self.index
    }
    /// Releases the loaded events. The next time the window is shown they are read again.
    fn suspend(&mut self) {
        self.page = page::Page::new();
    }
    fn tutorial_steps(&self) -> Vec<TutorialStep> {
        [
            ("0", "toolbar", "Event Watch · refresh and coverage", "Event Watch reads supported Windows logs in the background. The header reports coverage and access limitations. Refresh reloads events; it does not clear the Windows logs."),
            ("0", "table", "Live · filter and group events", "Search provider, event ID or text, filter by severity, and group repeated events. Pause stops live display updates so you can inspect a snapshot. A severity label is a clue to investigate, not a diagnosis."),
            ("0", "inspector", "Live · understand an event", "Select an event to see its explanation, source and available details. Event timing and repeated patterns matter. The same event ID can need context from its provider and neighboring events."),
            ("1", "table", "Crashes · episodes and timeline", "Crashes groups evidence around bluescreens, unexpected restarts and hardware faults. Select an episode to inspect the surrounding timeline. Events near a crash can be consequences rather than causes."),
            ("1", "inspector", "Crashes · evidence and dumps", "The inspector explains the selected evidence and can offer additional details or dump reading when available. A stop code or named component is a starting point for investigation, not proof of the root cause."),
            ("2", "table", "Summary · recurring patterns", "The summary highlights notable patterns in the loaded period and reports when nothing notable was found. Limited log access or retention can hide older events, so absence of evidence is not a complete health guarantee."),
        ]
        .into_iter()
        .map(|(p, t, h, d)| TutorialStep::new(p, t, h, d))
        .collect()
    }
    fn tutorial_location(&self) -> String {
        self.index.to_string()
    }
    fn tutorial_navigate(&mut self, page: &str) {
        self.navigate(page);
    }
}

#[cfg(test)]
mod real_log_tests {
    use super::analysis::{detect_crashes, digest, group, ms_to_text, DEFAULT_WINDOW_MS};
    use super::source::{load, WinEvents};

    /// Read-only: runs the whole pipeline on this PC's real System and Application logs.
    #[test]
    #[ignore]
    fn real_pipeline_smoke() {
        let since = {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            now - 30 * 24 * 3600 * 1000
        };
        let (events, statuses) = load(&WinEvents, since);
        let groups = group(&events);
        let crashes = detect_crashes(&events, DEFAULT_WINDOW_MS, |noticed| {
            super::source::estimate_stop(&WinEvents, since, noticed)
        });
        println!(
            "events={} groups={} crashes={}",
            events.len(),
            groups.len(),
            crashes.len()
        );
        for s in &statuses {
            println!(
                "channel {} readable={} oldest={:?}",
                s.name,
                s.readable,
                s.oldest_ms.map(ms_to_text)
            );
        }
        for line in digest(&groups, &crashes) {
            println!("digest [{:?}] {}", line.severity, line.text);
        }
        assert!(!statuses.is_empty());
    }
}
