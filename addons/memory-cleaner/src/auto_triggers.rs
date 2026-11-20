//! Two extra automatic triggers beside the pressure scheduler (issue #230). Both are off by
//! default, and a run they start is the same safe per-app trim the scheduler starts: no
//! administrator window ever appears from here.
//!
//! - Memory limit: when physical memory in use reaches the limit (1 to 99 percent), at most
//!   once per 30 seconds.
//! - Interval: every N minutes (1 to 1440), with no memory check.
//!
//! The limit is checked first. Both wait while commit is near its limit (the same guard the
//! scheduler uses: trimming does not help there). The decision is pure: the clock is injected
//! as `now_ms` and nothing sleeps. `Memory::evaluate` starts the run and then calls
//! `record_run`; until then the decision repeats on every tick, like the scheduler's.
//!
//! The "allow standby and modified lists in automatic cleaning" switch is stored, and
//! `auto_regions` decides which "Clean selected regions" an automatic run may include. Every
//! region is a system-wide call through the administrator helper, which shows a Confirm
//! window, so none of them can run unattended and the filter is always empty for now.

use std::collections::BTreeMap;

use egui::{self, RichText};
use serde_json::{json, Value};

use super::regions::{Region, Selection};
use super::report::Trigger;
use super::scheduler::limits::COMMIT_NEAR_LIMIT;
use super::style::{section, unproven_note, TRIM_EVIDENCE};
use super::Memory;
use toolboks_shared::ui::toggle_switch;

const PREFERENCE_KEY: &str = "auto_triggers";

pub const LIMIT_MIN: u8 = 1;
pub const LIMIT_MAX: u8 = 99;
pub const LIMIT_PRESETS: [u8; 4] = [70, 80, 90, 95];
pub const INTERVAL_MIN: u16 = 1;
pub const INTERVAL_MAX: u16 = 1440;
pub const INTERVAL_PRESETS: [u16; 5] = [5, 15, 30, 60, 120];
/// The limit trigger runs at most once per this long.
pub const LIMIT_COOLDOWN_MS: u64 = 30_000;

fn clamp_limit(value: u64) -> u8 {
    value.clamp(u64::from(LIMIT_MIN), u64::from(LIMIT_MAX)) as u8
}

fn clamp_interval(value: u64) -> u16 {
    value.clamp(u64::from(INTERVAL_MIN), u64::from(INTERVAL_MAX)) as u16
}

/// Physical memory in use, in percent (0 when the total is unknown).
pub fn used_percent(total: u64, available: u64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    total.saturating_sub(available) as f64 / total as f64 * 100.0
}

/// What the user chose. `None` is Disable. Everything is off by default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    pub limit: Option<u8>,
    pub interval: Option<u16>,
    pub allow_lists: bool,
}

impl Settings {
    /// The saved settings; a missing or unreadable value is off, a number out of range is
    /// pulled into range.
    pub fn load(preferences: &BTreeMap<String, Value>) -> Self {
        let saved = preferences.get(PREFERENCE_KEY);
        let number = |key: &str| saved.and_then(|v| v.get(key)).and_then(Value::as_u64);
        Self {
            limit: number("limit").map(clamp_limit),
            interval: number("interval").map(clamp_interval),
            allow_lists: saved
                .and_then(|v| v.get("allow_lists"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }
    }

    /// Saves the settings; true if the preferences changed. The untouched default is not
    /// written, so an unused add-on stores nothing.
    pub fn store(self, preferences: &mut BTreeMap<String, Value>) -> bool {
        let mut value = json!({});
        if let Some(limit) = self.limit {
            value["limit"] = limit.into();
        }
        if let Some(interval) = self.interval {
            value["interval"] = interval.into();
        }
        if self.allow_lists {
            value["allow_lists"] = true.into();
        }
        let changed = match preferences.get(PREFERENCE_KEY) {
            Some(saved) => *saved != value,
            None => self != Self::default(),
        };
        if changed {
            preferences.insert(PREFERENCE_KEY.into(), value);
        }
        changed
    }
}

/// Which trigger fired.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fire {
    Limit(u8),
    Interval(u16),
}

impl Fire {
    pub fn trigger(self) -> Trigger {
        match self {
            Fire::Limit(_) => Trigger::MemoryLimit,
            Fire::Interval(_) => Trigger::Interval,
        }
    }

    /// The rule that started the run, for "Why it ran".
    pub fn text(self) -> String {
        match self {
            Fire::Limit(limit) => format!("Memory in use reached your limit of {limit}%."),
            Fire::Interval(1) => "Your 1 minute interval passed.".to_string(),
            Fire::Interval(minutes) => format!("Your {minutes} minute interval passed."),
        }
    }
}

#[derive(Debug, Default)]
pub struct AutoTriggers {
    settings: Settings,
    /// The last automatic run of any kind: the limit's 30 s cooldown counts from it.
    last_run_ms: Option<u64>,
    /// Where the interval counts from: the first tick after it was switched on, or the last
    /// automatic run.
    anchor_ms: Option<u64>,
}

impl AutoTriggers {
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            ..Self::default()
        }
    }

    pub fn settings(&self) -> Settings {
        self.settings
    }

    pub fn any_on(&self) -> bool {
        self.settings.limit.is_some() || self.settings.interval.is_some()
    }

    /// `None` is Disable. The value is pulled into 1 to 99.
    pub fn set_limit(&mut self, limit: Option<u8>) {
        self.settings.limit = limit.map(|v| clamp_limit(u64::from(v)));
    }

    /// `None` is Disable. The value is pulled into 1 to 1440; a change restarts the count.
    pub fn set_interval(&mut self, interval: Option<u16>) {
        let interval = interval.map(|v| clamp_interval(u64::from(v)));
        if interval != self.settings.interval {
            self.anchor_ms = None;
        }
        self.settings.interval = interval;
    }

    pub fn set_allow_lists(&mut self, on: bool) {
        self.settings.allow_lists = on;
    }

    /// Both triggers to Disable. The cooldown is kept, so toggling cannot beat it.
    pub fn disable_all(&mut self) {
        self.set_limit(None);
        self.set_interval(None);
    }

    /// Call after any automatic run started (also the scheduler's), never after a manual one.
    pub fn record_run(&mut self, now_ms: u64) {
        self.last_run_ms = Some(now_ms);
        self.anchor_ms = Some(now_ms);
    }

    /// One check at `now_ms`. `used_percent` is physical memory in use, `commit_fraction` is
    /// 0.0 to 1.0. The limit is checked before the interval.
    pub fn tick(&mut self, now_ms: u64, used_percent: f64, commit_fraction: f64) -> Option<Fire> {
        // Count from the first tick even while commit blocks, so the wait is not extended.
        let anchor = self
            .settings
            .interval
            .map(|_| *self.anchor_ms.get_or_insert(now_ms));
        if commit_fraction >= COMMIT_NEAR_LIMIT {
            return None;
        }
        if let Some(limit) = self.settings.limit {
            let cooled = self
                .last_run_ms
                .is_none_or(|t| now_ms.saturating_sub(t) >= LIMIT_COOLDOWN_MS);
            if used_percent >= f64::from(limit) && cooled {
                return Some(Fire::Limit(limit));
            }
        }
        let (minutes, anchor) = self.settings.interval.zip(anchor)?;
        (now_ms.saturating_sub(anchor) >= u64::from(minutes) * 60_000)
            .then_some(Fire::Interval(minutes))
    }
}

/// Every region is a system-wide call through the administrator helper, whose Confirm window
/// cannot appear unattended.
fn needs_helper(_region: Region) -> bool {
    true
}

/// The ticked regions an automatic run may include. The standby (5) and modified (4) lists are
/// left out unless `allow_lists` is on, and so is anything that needs the helper.
pub fn auto_regions(selection: Selection, allow_lists: bool) -> Selection {
    filter_regions(selection, allow_lists, needs_helper)
}

fn filter_regions(
    selection: Selection,
    allow_lists: bool,
    needs_helper: impl Fn(Region) -> bool,
) -> Selection {
    let mut kept = selection;
    for region in selection.regions() {
        let is_list = matches!(region, Region::StandbyList | Region::ModifiedPageList);
        if (is_list && !allow_lists) || needs_helper(region) {
            kept.set(region, false);
        }
    }
    kept
}

const LIMIT_NOTE: &str = "Starts a clean when the share of memory in use reaches the limit, at \
    most once every 30 seconds. It is checked before the interval.";
const INTERVAL_NOTE: &str = "Starts a clean every so many minutes, whatever memory is doing. \
    Any automatic clean restarts the count.";
const LISTS_NOTE: &str = "Off by default. Clearing the standby or modified list can freeze the \
    PC for a moment. This only decides what automatic runs may include; every region needs the \
    administrator window, so none runs automatically yet.";

impl Memory {
    /// The "Rules" tab part of automatic cleaning: the two extra triggers and their notes.
    pub(super) fn auto_triggers_ui(&mut self, ui: &mut egui::Ui) {
        section(ui, "More automatic triggers");
        ui.label(
            RichText::new(
                "Both are off by default. They start the same safe per-app trim as above, never \
                 touch the app you are using and never ask for administrator permission. They \
                 pause when you switch away from Memory Cleaner or hide Toolboks in the tray.",
            )
            .weak()
            .small(),
        );
        unproven_note(ui, TRIM_EVIDENCE);
        let mut limit = self.auto.settings().limit;
        if picker(
            ui,
            "Memory limit",
            &mut limit,
            &LIMIT_PRESETS,
            LIMIT_MIN..=LIMIT_MAX,
            "%",
        ) {
            self.auto.set_limit(limit);
        }
        ui.label(RichText::new(LIMIT_NOTE).weak().small());
        let mut interval = self.auto.settings().interval;
        if picker(
            ui,
            "Interval",
            &mut interval,
            &INTERVAL_PRESETS,
            INTERVAL_MIN..=INTERVAL_MAX,
            " min",
        ) {
            self.auto.set_interval(interval);
        }
        ui.label(RichText::new(INTERVAL_NOTE).weak().small());

        let mut allow = self.auto.settings().allow_lists;
        ui.horizontal(|ui| {
            ui.label("Allow standby and modified lists in automatic cleaning");
            let response = toggle_switch(ui, &mut allow);
            response.widget_info(|| {
                egui::WidgetInfo::selected(
                    egui::Role::Switch,
                    true,
                    allow,
                    "Allow standby and modified lists in automatic cleaning",
                )
            });
            if response.changed() {
                self.auto.set_allow_lists(allow);
            }
        });
        ui.label(RichText::new(LISTS_NOTE).weak().small());
        let runnable = auto_regions(self.regions, self.auto.settings().allow_lists)
            .regions()
            .count();
        ui.label(
            RichText::new(format!(
                "Ticked regions an automatic run would include: {runnable}"
            ))
            .small(),
        );
    }
}

/// A "Disable" entry and the presets in a drop-down, plus a number box for any other value in
/// `range` while it is on. Returns true when `value` changed.
fn picker<T>(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut Option<T>,
    presets: &[T],
    range: std::ops::RangeInclusive<T>,
    suffix: &str,
) -> bool
where
    T: Copy + PartialEq + std::fmt::Display + egui::emath::Numeric,
{
    let before = *value;
    ui.horizontal(|ui| {
        ui.label(label);
        let shown = value.map_or("Disable".to_string(), |v| format!("{v}{suffix}"));
        egui::ComboBox::from_id_salt(label)
            .selected_text(shown)
            .show_ui(ui, |ui| {
                ui.selectable_value(value, None, "Disable");
                for preset in presets {
                    ui.selectable_value(value, Some(*preset), format!("{preset}{suffix}"));
                }
            });
        if let Some(v) = value {
            ui.add(egui::DragValue::new(v).range(range).suffix(suffix));
        }
    });
    *value != before
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: u64 = 1_000;
    const MIN: u64 = 60_000;

    fn on(limit: Option<u8>, interval: Option<u16>) -> AutoTriggers {
        AutoTriggers::new(Settings {
            limit,
            interval,
            allow_lists: false,
        })
    }

    #[test]
    fn everything_is_off_by_default_and_never_fires() {
        let mut t = AutoTriggers::default();
        assert_eq!(t.settings(), Settings::default());
        assert!(!t.any_on());
        for now in [0, 40 * SEC, 2_000 * MIN] {
            assert_eq!(t.tick(now, 100.0, 0.1), None);
        }
    }

    #[test]
    fn limit_fires_at_the_limit_and_not_below() {
        let mut t = on(Some(90), None);
        assert_eq!(t.tick(0, 89.99, 0.5), None);
        assert_eq!(t.tick(1, 90.0, 0.5), Some(Fire::Limit(90)));
        assert_eq!(t.tick(2, 99.0, 0.5), Some(Fire::Limit(90)));
    }

    #[test]
    fn limit_repeats_until_recorded_then_waits_thirty_seconds() {
        let mut t = on(Some(80), None);
        assert!(t.tick(0, 85.0, 0.5).is_some());
        assert!(t.tick(SEC, 85.0, 0.5).is_some(), "not recorded: retried");
        t.record_run(SEC);
        assert_eq!(t.tick(2 * SEC, 99.0, 0.5), None);
        assert_eq!(t.tick(SEC + LIMIT_COOLDOWN_MS - 1, 99.0, 0.5), None);
        assert_eq!(
            t.tick(SEC + LIMIT_COOLDOWN_MS, 99.0, 0.5),
            Some(Fire::Limit(80))
        );
    }

    #[test]
    fn limit_ignores_the_clock_going_backwards() {
        let mut t = on(Some(80), None);
        t.record_run(100 * SEC);
        assert_eq!(t.tick(10 * SEC, 99.0, 0.5), None);
    }

    #[test]
    fn interval_waits_from_the_first_tick_and_ignores_memory() {
        let mut t = on(None, Some(5));
        assert_eq!(t.tick(10 * SEC, 0.0, 0.0), None);
        assert_eq!(t.tick(10 * SEC + 5 * MIN - 1, 0.0, 0.0), None);
        assert_eq!(
            t.tick(10 * SEC + 5 * MIN, 0.0, 0.0),
            Some(Fire::Interval(5))
        );
    }

    #[test]
    fn interval_never_repeats_faster_than_its_minutes() {
        let mut t = on(None, Some(1));
        t.tick(0, 0.0, 0.0);
        assert!(t.tick(MIN, 0.0, 0.0).is_some());
        t.record_run(MIN);
        assert_eq!(t.tick(2 * MIN - 1, 0.0, 0.0), None);
        assert!(t.tick(2 * MIN, 0.0, 0.0).is_some());
    }

    #[test]
    fn a_limit_run_restarts_the_interval_count() {
        let mut t = on(Some(90), Some(5));
        t.tick(0, 10.0, 0.5);
        t.record_run(4 * MIN);
        assert_eq!(t.tick(5 * MIN, 10.0, 0.5), None);
        assert_eq!(t.tick(9 * MIN, 10.0, 0.5), Some(Fire::Interval(5)));
    }

    #[test]
    fn limit_is_checked_before_interval() {
        let mut t = on(Some(70), Some(1));
        t.tick(0, 10.0, 0.5);
        assert_eq!(t.tick(MIN, 75.0, 0.5), Some(Fire::Limit(70)));
        assert_eq!(t.tick(MIN, 50.0, 0.5), Some(Fire::Interval(1)));
    }

    #[test]
    fn a_recent_run_holds_back_both() {
        let mut t = on(Some(70), Some(5));
        t.tick(0, 10.0, 0.5);
        t.record_run(5 * MIN - SEC);
        // The limit is cooling down and the interval count restarted with the run.
        assert_eq!(t.tick(5 * MIN, 99.0, 0.5), None);
    }

    #[test]
    fn commit_near_limit_blocks_both() {
        let mut t = on(Some(70), Some(1));
        t.tick(0, 10.0, 0.5);
        assert_eq!(t.tick(MIN, 99.0, COMMIT_NEAR_LIMIT), None);
        assert_eq!(t.tick(MIN, 99.0, 0.99), None);
        assert!(t.tick(MIN, 99.0, 0.949).is_some());
    }

    #[test]
    fn commit_block_does_not_extend_the_interval() {
        let mut t = on(None, Some(1));
        assert_eq!(t.tick(0, 0.0, 0.99), None);
        assert_eq!(t.tick(MIN, 0.0, 0.5), Some(Fire::Interval(1)));
    }

    #[test]
    fn disabling_stops_firing_and_a_new_interval_restarts_the_count() {
        let mut t = on(Some(70), Some(5));
        t.tick(0, 10.0, 0.5);
        t.disable_all();
        assert!(!t.any_on());
        assert_eq!(t.tick(10 * MIN, 99.0, 0.5), None);
        t.set_interval(Some(5));
        assert_eq!(t.tick(11 * MIN, 0.0, 0.5), None, "counts from here");
        assert_eq!(t.tick(16 * MIN, 0.0, 0.5), Some(Fire::Interval(5)));
        t.set_interval(Some(5));
        assert!(
            t.tick(16 * MIN, 0.0, 0.5).is_some(),
            "same value keeps the count"
        );
    }

    #[test]
    fn disabling_keeps_the_cooldown() {
        let mut t = on(Some(70), None);
        t.record_run(0);
        t.set_limit(None);
        t.set_limit(Some(70));
        assert_eq!(t.tick(SEC, 99.0, 0.5), None);
    }

    #[test]
    fn values_are_pulled_into_range() {
        let mut t = AutoTriggers::default();
        t.set_limit(Some(0));
        assert_eq!(t.settings().limit, Some(1));
        t.set_limit(Some(100));
        assert_eq!(t.settings().limit, Some(99));
        t.set_interval(Some(0));
        assert_eq!(t.settings().interval, Some(1));
        t.set_interval(Some(5_000));
        assert_eq!(t.settings().interval, Some(1440));
        assert_eq!(clamp_limit(u64::MAX), 99);
        assert_eq!(clamp_interval(u64::MAX), 1440);
    }

    #[test]
    fn presets_are_the_documented_ones_and_in_range() {
        assert_eq!(LIMIT_PRESETS, [70, 80, 90, 95]);
        assert_eq!(INTERVAL_PRESETS, [5, 15, 30, 60, 120]);
        assert!(LIMIT_PRESETS
            .iter()
            .all(|p| (LIMIT_MIN..=LIMIT_MAX).contains(p)));
        assert!(INTERVAL_PRESETS
            .iter()
            .all(|p| (INTERVAL_MIN..=INTERVAL_MAX).contains(p)));
    }

    #[test]
    fn used_percent_is_total_minus_available() {
        assert_eq!(used_percent(0, 0), 0.0);
        assert_eq!(used_percent(200, 50), 75.0);
        assert_eq!(used_percent(100, 500), 0.0);
    }

    #[test]
    fn fire_names_its_trigger_and_text() {
        assert_eq!(Fire::Limit(90).trigger(), Trigger::MemoryLimit);
        assert_eq!(Fire::Interval(5).trigger(), Trigger::Interval);
        assert!(Fire::Limit(90).text().contains("90%"));
        assert!(Fire::Interval(30).text().contains("30 minute interval"));
        assert!(Fire::Interval(1).text().contains("1 minute interval"));
    }

    #[test]
    fn settings_round_trip_and_default_stores_nothing() {
        let mut preferences = BTreeMap::new();
        assert_eq!(Settings::load(&preferences), Settings::default());
        assert!(!Settings::default().store(&mut preferences));
        assert!(preferences.is_empty());

        let picked = Settings {
            limit: Some(95),
            interval: Some(120),
            allow_lists: true,
        };
        assert!(picked.store(&mut preferences));
        assert!(!picked.store(&mut preferences), "unchanged: no second save");
        assert_eq!(Settings::load(&preferences), picked);

        let only_limit = Settings {
            limit: Some(70),
            ..Settings::default()
        };
        assert!(only_limit.store(&mut preferences));
        assert_eq!(Settings::load(&preferences), only_limit);

        assert!(Settings::default().store(&mut preferences));
        assert_eq!(Settings::load(&preferences), Settings::default());
        assert!(!Settings::default().store(&mut preferences));
    }

    #[test]
    fn saved_values_out_of_range_or_wrong_type_are_fixed_or_off() {
        let mut preferences = BTreeMap::new();
        preferences.insert(
            PREFERENCE_KEY.to_string(),
            json!({"limit": 500, "interval": 0, "allow_lists": true}),
        );
        let s = Settings::load(&preferences);
        assert_eq!(
            (s.limit, s.interval, s.allow_lists),
            (Some(99), Some(1), true)
        );
        for bad in [
            json!("x"),
            json!(7),
            json!(null),
            json!({"limit": "90", "interval": -3, "allow_lists": "yes"}),
        ] {
            preferences.insert(PREFERENCE_KEY.to_string(), bad);
            assert_eq!(Settings::load(&preferences), Settings::default());
        }
    }

    fn picked(regions: &[Region]) -> Selection {
        let mut s = Selection::from_bits(0);
        for r in regions {
            s.set(*r, true);
        }
        s
    }

    #[test]
    fn no_region_runs_automatically_while_all_need_the_helper() {
        let all = Selection::from_bits(u8::MAX);
        assert!(auto_regions(all, false).is_empty());
        assert!(auto_regions(all, true).is_empty());
        assert!(auto_regions(Selection::default(), true).is_empty());
    }

    #[test]
    fn lists_stay_out_unless_allowed() {
        let sel = picked(&[
            Region::WorkingSet,
            Region::ModifiedPageList,
            Region::StandbyList,
            Region::LowPriorityStandby,
        ]);
        let without = filter_regions(sel, false, |_| false);
        assert_eq!(
            without.regions().collect::<Vec<_>>(),
            [Region::WorkingSet, Region::LowPriorityStandby]
        );
        assert_eq!(filter_regions(sel, true, |_| false), sel);
    }

    #[test]
    fn helper_regions_stay_out_even_when_lists_are_allowed() {
        let sel = picked(&[Region::WorkingSet, Region::StandbyList]);
        let kept = filter_regions(sel, true, |r| r == Region::WorkingSet);
        assert_eq!(kept.regions().collect::<Vec<_>>(), [Region::StandbyList]);
    }
}
