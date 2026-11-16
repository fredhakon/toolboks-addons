//! Event Watch: reads the Windows event logs and shows live errors and what happened before a
//! bluescreen, a hard reset or a hardware fault. All Event Log work runs on worker threads.
//! It only reads: it never clears a log. The opt-in latency check (`latency`, `admin`) is the
//! one part that asks for administrator rights, each time, through the SDK's separate helper.

pub mod admin;
pub mod analysis;
pub mod bugchecks;
pub mod catalog;
mod latency;
pub mod model;
mod page;
pub mod source;

use std::collections::BTreeMap;
use toolboks_addon_sdk::{NativeAddon, TutorialStep};

pub const ID: &str = "toolboks.event-watch";
/// Page names, in order. They match `host-services.json`.
pub const PAGES: [&str; 5] = ["Live", "Crashes", "Summary", "Latency check", "Boot"];
/// Index of the Latency check page in [`PAGES`], looked up by name when compiling, so a page
/// added or moved elsewhere in the list cannot make it open the wrong page.
const LATENCY_PAGE: usize = page_index("Latency check");
/// Index of the Boot page in [`PAGES`], looked up by name for the same reason.
pub(crate) const BOOT_PAGE: usize = page_index("Boot");

/// Position of `name` in [`PAGES`]; the build fails if it is missing.
const fn page_index(name: &str) -> usize {
    let mut i = 0;
    while i < PAGES.len() {
        let (a, b) = (PAGES[i].as_bytes(), name.as_bytes());
        if a.len() == b.len() {
            let mut j = 0;
            while j < a.len() && a[j] == b[j] {
                j += 1;
            }
            if j == a.len() {
                return i;
            }
        }
        i += 1;
    }
    panic!("page missing from PAGES")
}

/// Validates the one administrator action (the latency check) inside the separate helper. It
/// runs nothing; the SDK's confirmation window decides whether `apply` runs.
pub fn prepare_admin(
    request: &toolboks_addon_sdk::AdminRequest,
) -> Result<Box<dyn toolboks_addon_sdk::admin::AdminChange>, String> {
    admin::prepare_admin(request)
}

pub struct EventWatch {
    page: page::Page,
    /// Kept across `suspend`, so an answer from the helper still lands. Memory only.
    latency: latency::Latency,
    index: usize,
}

impl Default for EventWatch {
    fn default() -> Self {
        Self {
            page: page::Page::new(),
            latency: latency::Latency::default(),
            index: 0,
        }
    }
}

/// Applies the Toolboks look (fonts, text sizes, spacing, colours) the first time the add-on
/// draws. The add-on runs in its own process, so the host's own set-up does not reach it.
fn style_once(ctx: &egui::Context) {
    let id = egui::Id::NULL.with("toolboks-style-applied");
    if !ctx.data(|data| data.get_temp::<bool>(id)).unwrap_or(false) {
        toolboks_shared::ui::apply_style(ctx);
        ctx.data_mut(|data| data.insert_temp(id, true));
    }
}

impl NativeAddon for EventWatch {
    fn ui(
        &mut self,
        ui: &mut egui::Ui,
        _preferences: &mut BTreeMap<String, serde_json::Value>,
    ) -> bool {
        style_once(ui.ctx());
        if self.index == LATENCY_PAGE {
            self.latency.ui(ui);
        } else {
            self.page.ui(ui, self.index);
        }
        false
    }
    fn set_capabilities(&mut self, capabilities: &[toolboks_addon_sdk::Capability]) {
        self.latency.set_capabilities(capabilities);
    }
    fn prepare_admin(
        &mut self,
        request: &toolboks_addon_sdk::AdminRequest,
    ) -> Result<Box<dyn toolboks_addon_sdk::admin::AdminChange>, String> {
        admin::prepare_admin(request)
    }
    fn admin_result(&mut self, id: u64, outcome: toolboks_addon_sdk::AdminOutcome) {
        self.latency.admin_result(id, outcome);
    }
    fn take_host_request(&mut self) -> Option<toolboks_addon_sdk::HostRequest> {
        self.latency.take_request()
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
        .chain(std::iter::once(TutorialStep::new(
            &LATENCY_PAGE.to_string(),
            "table",
            "Latency check · slow drivers",
            "Run a short Windows recording while stutter or audio crackle happens, and see which drivers held the processor the longest. It asks for administrator rights each time and never runs on its own. The recording is deleted after it is read. A long run is a clue, not proof of the cause.",
        )))
        .chain(std::iter::once(TutorialStep::new(
            &BOOT_PAGE.to_string(),
            "table",
            "Boot · startup time and slow items",
            "Boot shows how long each startup took and which apps, services and drivers Windows itself named as slow. Windows keeps this in a log that, on most PCs, only administrators can read. When Event Watch cannot read it, this page says so instead of showing a time, and a missing entry never means a fast boot.",
        )))
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
mod page_tests {
    use super::*;

    #[test]
    fn the_latency_page_index_follows_the_page_list() {
        assert_eq!(PAGES[LATENCY_PAGE], "Latency check");
        assert_eq!(page_index("Live"), 0);
        let mut watch = EventWatch::default();
        watch.navigate(&LATENCY_PAGE.to_string());
        assert_eq!(watch.page_index(), LATENCY_PAGE);
        let steps = watch.tutorial_steps();
        let step = steps
            .iter()
            .find(|s| s.title.starts_with("Latency check"))
            .unwrap();
        assert_eq!(step.page, LATENCY_PAGE.to_string());
    }

    #[test]
    fn the_boot_page_index_follows_the_page_list_and_never_collides_with_latency() {
        assert_eq!(PAGES[BOOT_PAGE], "Boot");
        assert_ne!(BOOT_PAGE, LATENCY_PAGE);
        assert_eq!(
            PAGES,
            ["Live", "Crashes", "Summary", "Latency check", "Boot"]
        );
        let mut watch = EventWatch::default();
        watch.navigate(&BOOT_PAGE.to_string());
        assert_eq!(watch.page_index(), BOOT_PAGE);
        let steps = watch.tutorial_steps();
        let step = steps.iter().find(|s| s.title.starts_with("Boot")).unwrap();
        assert_eq!(step.page, BOOT_PAGE.to_string());
        // The tour steps for the two new pages point at their own pages.
        let latency = steps
            .iter()
            .find(|s| s.title.starts_with("Latency check"))
            .unwrap();
        assert_ne!(latency.page, step.page);
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
