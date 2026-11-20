//! "Freed memory" arithmetic for one clean (issue #231). Pure logic, no UI and no system calls.
//!
//! Like Mem Reduct, freed = memory in use before minus memory in use after, and 0 if it rose.
//! Unlike Mem Reduct it is never shown alone: the change in the standby and modified lists and
//! the hard-fault proxy (Pages Input per second) sit next to it, and pages that only moved to
//! the standby cache are called out, because Windows hands those back to the next app that asks.
//!
//! What is measured where:
//! * whole run: freed, standby, modified and disk reads, from two readings around the run.
//! * per app: only the working set drop of that app. The system-wide numbers cannot be split
//!   between apps, so they are not.
//! * per region: the helper reads the lists around each region, so freed and the list changes
//!   are measured per region (other programs keep allocating meanwhile). Disk reads are not.

use serde_json::{json, Value};

use toolboks_shared::format::size_plain;

use super::memlist::Lists;
use super::report::{self, RunMeasurement, Trigger};

/// Memory in use before minus memory in use after; 0 if it rose.
pub fn freed(used_before: u64, used_after: u64) -> u64 {
    used_before.saturating_sub(used_after)
}

fn signed(before: u64, after: u64) -> i64 {
    after as i64 - before as i64
}

fn signed_size(bytes: i64) -> String {
    format!(
        "{}{}",
        if bytes < 0 { "-" } else { "+" },
        size_plain(bytes.unsigned_abs())
    )
}

/// The lists inside one reading of a run.
fn lists_of(m: &RunMeasurement) -> Lists {
    Lists {
        free: m.zero_free,
        modified: m.modified,
        standby: m.standby,
    }
}

/// What changed between two readings. `freed` is whole-system; see the module note.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Change {
    pub freed: u64,
    /// Standby list after minus before.
    pub standby: i64,
    /// Modified list after minus before.
    pub modified: i64,
    /// Pages read from disk per second after the run, if it was measured.
    pub disk_reads: Option<f64>,
}

impl Change {
    pub fn between(before: Lists, after: Lists, disk_reads: Option<f64>) -> Self {
        // In use = total - free - standby, so the total cancels out of "before minus after".
        let available = |l: Lists| l.free + l.standby;
        Self {
            freed: available(after).saturating_sub(available(before)),
            standby: signed(before.standby, after.standby),
            modified: signed(before.modified, after.modified),
            disk_reads,
        }
    }

    pub fn from_readings(before: &RunMeasurement, after: &RunMeasurement) -> Self {
        Self::between(lists_of(before), lists_of(after), after.pages_input_per_sec)
    }

    /// The part of `freed` that only moved to the standby cache.
    pub fn cache_part(&self) -> u64 {
        self.freed.min(self.standby.max(0) as u64)
    }

    pub fn text(&self) -> String {
        let mut text = format!(
            "Freed {} (memory in use before minus after, 0 if it rose).",
            size_plain(self.freed)
        );
        if self.cache_part() > 0 {
            text.push_str(&format!(
                " {} of that moved to the standby cache. {}",
                size_plain(self.cache_part()),
                report::cache_note()
            ));
        }
        text.push_str(&format!(
            " Standby {}, modified {}.",
            signed_size(self.standby),
            signed_size(self.modified)
        ));
        match self.disk_reads {
            Some(v) => text.push_str(&format!(" Disk reads after the run: {v:.0} pages/s.")),
            None => text.push_str(" Disk reads were not measured."),
        }
        text
    }

    fn to_json(self) -> Value {
        json!({
            "freed": self.freed,
            "standby": self.standby,
            "modified": self.modified,
            "disk_reads": self.disk_reads,
        })
    }

    fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            freed: value.get("freed")?.as_u64()?,
            standby: value.get("standby")?.as_i64()?,
            modified: value.get("modified")?.as_i64()?,
            disk_reads: value.get("disk_reads").and_then(Value::as_f64),
        })
    }
}

/// The apps line: working set is the only number that exists per app.
pub fn apps_text(count: usize, ws_before: u64, ws_after: u64) -> String {
    format!(
        "{count} app{} gave up {} of working set (measured per app). Standby, modified and disk \
         reads are measured for the whole run, not per app.",
        if count == 1 { "" } else { "s" },
        size_plain(freed(ws_before, ws_after))
    )
}

/// One line per region, and the caveat that goes with measuring them one after the other.
pub fn parts_text(parts: &[(String, Option<Change>)]) -> Vec<String> {
    let mut lines: Vec<String> = parts
        .iter()
        .map(|(label, change)| match change {
            Some(c) => format!(
                "{label}: freed {}, standby {}.",
                size_plain(c.freed),
                signed_size(c.standby)
            ),
            None => format!("{label}: not measured."),
        })
        .collect();
    if !lines.is_empty() {
        lines.push(
            "Each region is measured around itself, one after the other, so other programs using \
             memory at the same time are mixed in."
                .to_string(),
        );
    }
    lines
}

/// What the administrator helper sends back besides its text: the whole run and each region.
pub fn measured_to_json(whole: Option<Change>, parts: &[(&str, Option<Change>)]) -> Value {
    json!({
        "whole": whole.map(Change::to_json),
        "parts": parts
            .iter()
            .map(|(label, change)| json!({"label": label, "change": change.map(Change::to_json)}))
            .collect::<Vec<_>>(),
    })
}

/// The other direction. Anything unreadable becomes "not measured", never an error.
pub fn measured_from_json(value: &Value) -> (Option<Change>, Vec<(String, Option<Change>)>) {
    let whole = value.get("whole").and_then(Change::from_json);
    let parts = value
        .get("parts")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| {
                    let label = p.get("label")?.as_str()?.to_string();
                    Some((label, p.get("change").and_then(Change::from_json)))
                })
                .collect()
        })
        .unwrap_or_default();
    (whole, parts)
}

/// The last clean, for the Home line and the log.
#[derive(Clone, Debug, PartialEq)]
pub struct RunResult {
    pub trigger: Trigger,
    /// Local time of day, already formatted.
    pub when: String,
    /// What was cleaned: region labels, or "Idle apps" / "Selected apps". Never process names.
    pub what: Vec<String>,
    pub change: Option<Change>,
    pub detail: Vec<String>,
}

impl RunResult {
    pub fn headline(&self) -> String {
        let numbers = self.change.map_or_else(
            || "The memory numbers could not be read, so nothing is claimed.".to_string(),
            |c| c.text(),
        );
        format!("{}, {}: {numbers}", self.trigger.label(), self.when)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1024 * 1024;

    fn lists(free: u64, standby: u64, modified: u64) -> Lists {
        Lists {
            free: free * MB,
            standby: standby * MB,
            modified: modified * MB,
        }
    }

    #[test]
    fn freed_is_used_before_minus_used_after_and_never_negative() {
        assert_eq!(freed(8000, 7000), 1000);
        assert_eq!(freed(7000, 8000), 0);
        assert_eq!(freed(0, u64::MAX), 0);
    }

    #[test]
    fn a_rise_in_use_frees_nothing() {
        let c = Change::between(lists(1000, 500, 0), lists(900, 500, 0), None);
        assert_eq!(c.freed, 0);
    }

    #[test]
    fn freed_moved_to_standby_is_called_out_and_capped() {
        // 600 MB left use: 400 went to standby, 200 became free.
        let c = Change::between(lists(1000, 500, 50), lists(1200, 900, 40), Some(12.0));
        assert_eq!(c.freed, 600 * MB);
        assert_eq!(c.standby, 400 * MB as i64);
        assert_eq!(c.modified, -(10 * MB as i64));
        assert_eq!(c.cache_part(), 400 * MB);
        // Standby grew more than use fell: the cache part cannot exceed what was freed.
        let d = Change::between(lists(1000, 500, 0), lists(900, 700, 0), None);
        assert_eq!(d.freed, 100 * MB);
        assert_eq!(d.cache_part(), 100 * MB);
        // Standby shrank (a purge): none of the freed memory is cache.
        let e = Change::between(lists(1000, 900, 0), lists(1400, 500, 0), None);
        assert_eq!(e.freed, 0);
        assert_eq!(e.standby, -(400 * MB as i64));
        assert_eq!(e.cache_part(), 0);
    }

    #[test]
    fn readings_carry_the_hard_fault_proxy_from_after_the_run() {
        let reading = |zf: u64, reads: Option<f64>| RunMeasurement {
            zero_free: zf * MB,
            pages_input_per_sec: reads,
            ..Default::default()
        };
        let c = Change::from_readings(&reading(100, Some(1.0)), &reading(300, Some(40.0)));
        assert_eq!(c.freed, 200 * MB);
        assert_eq!(c.disk_reads, Some(40.0));
        assert_eq!(
            Change::from_readings(&reading(1, None), &reading(1, None)).disk_reads,
            None
        );
    }

    #[test]
    fn text_is_honest_about_cache_and_unmeasured_reads() {
        let c = Change::between(lists(1000, 500, 50), lists(1200, 900, 40), None);
        let t = c.text();
        assert!(t.starts_with("Freed 600 MB"), "{t}");
        assert!(
            t.contains("400 MB of that moved to the standby cache"),
            "{t}"
        );
        assert!(t.contains("Standby +400 MB, modified -10 MB."), "{t}");
        assert!(t.ends_with("Disk reads were not measured."), "{t}");
        let plain = Change::between(lists(1000, 500, 0), lists(1100, 500, 0), Some(7.4)).text();
        assert!(!plain.contains("standby cache"), "{plain}");
        assert!(
            plain.ends_with("Disk reads after the run: 7 pages/s."),
            "{plain}"
        );
    }

    #[test]
    fn per_app_line_says_what_is_measured_per_app() {
        let t = apps_text(2, 900 * MB, 400 * MB);
        assert!(t.starts_with("2 apps gave up 500 MB of working set"), "{t}");
        assert!(t.contains("whole run, not per app"));
        assert!(apps_text(1, 10, 20).starts_with("1 app gave up"));
    }

    #[test]
    fn per_region_lines_say_which_are_not_measured() {
        let c = Change::between(lists(1000, 500, 0), lists(1100, 600, 0), None);
        let lines = parts_text(&[
            ("Working set".to_string(), Some(c)),
            ("Combine".to_string(), None),
        ]);
        assert_eq!(lines[0], "Working set: freed 200 MB, standby +100 MB.");
        assert_eq!(lines[1], "Combine: not measured.");
        assert!(lines[2].contains("other programs"));
        assert!(parts_text(&[]).is_empty());
    }

    #[test]
    fn helper_json_round_trips_and_bad_json_means_not_measured() {
        let c = Change::between(lists(1000, 500, 5), lists(1100, 400, 9), None);
        let json = measured_to_json(Some(c), &[("Working set", Some(c)), ("Combine", None)]);
        let (whole, parts) = measured_from_json(&json);
        assert_eq!(whole, Some(c));
        assert_eq!(
            parts,
            vec![
                ("Working set".to_string(), Some(c)),
                ("Combine".to_string(), None)
            ]
        );
        let (whole, parts) = measured_from_json(&json!({"whole": {"freed": "x"}, "parts": 3}));
        assert_eq!((whole, parts.len()), (None, 0));
        assert_eq!(measured_from_json(&Value::Null), (None, Vec::new()));
        let (_, parts) = measured_from_json(&json!({"parts": [{"label": 5}, {"label": "A"}]}));
        assert_eq!(parts, vec![("A".to_string(), None)]);
    }

    #[test]
    fn headline_names_the_source_and_never_claims_unread_numbers() {
        let mut r = RunResult {
            trigger: Trigger::MemoryLimit,
            when: "14:03:11".into(),
            what: vec!["Idle apps".into()],
            change: None,
            detail: Vec::new(),
        };
        assert_eq!(
            r.headline(),
            "Auto limit, 14:03:11: The memory numbers could not be read, so nothing is claimed."
        );
        r.change = Some(Change::between(lists(1, 1, 1), lists(1, 1, 1), None));
        assert!(r.headline().starts_with("Auto limit, 14:03:11: Freed 0 MB"));
    }
}
