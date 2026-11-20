//! The two clean preferences (ask before a manual trim, keep a log) and the optional local log
//! of cleans (issue #231).
//!
//! The log is off by default and lives in one small JSON file in this add-on's folder of the
//! Toolboks data folder. It is never read by anything but this add-on, never sent anywhere, and
//! holds only the source ("Manual", "Auto limit", ...), what was cleaned (region labels or
//! "Idle apps" / "Selected apps"), the freed bytes and the time. No process names, no paths.
//! It is bounded by `MAX_ENTRIES` and `MAX_BYTES`; the oldest entries are dropped first.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::{json, Value};

use super::freed::RunResult;
use super::ID;

pub const MAX_ENTRIES: usize = 200;
pub const MAX_BYTES: usize = 32 * 1024;
/// Longest text kept per field when reading, so a hand-edited file cannot grow the log.
const MAX_FIELD: usize = 64;
const FILE_NAME: &str = "clean-log.json";
const PREFERENCE_KEY: &str = "clean_options";

/// What the user chose. Asking is on and the log is off until they change it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// Ask before a manual trim. Automatic runs never ask.
    pub confirm_manual: bool,
    pub log: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            confirm_manual: true,
            log: false,
        }
    }
}

impl Options {
    /// The saved choices; a missing or unreadable value keeps the default.
    pub fn load(preferences: &BTreeMap<String, Value>) -> Self {
        let saved = preferences.get(PREFERENCE_KEY);
        let flag = |key: &str, default: bool| {
            saved
                .and_then(|v| v.get(key))
                .and_then(Value::as_bool)
                .unwrap_or(default)
        };
        let default = Self::default();
        Self {
            confirm_manual: flag("confirm_manual", default.confirm_manual),
            log: flag("log", default.log),
        }
    }

    /// Saves the choices; true if the preferences changed. The untouched default is not written.
    pub fn store(self, preferences: &mut BTreeMap<String, Value>) -> bool {
        let value = json!({ "confirm_manual": self.confirm_manual, "log": self.log });
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub source: String,
    pub regions: Vec<String>,
    /// `None` when the memory numbers could not be read.
    pub freed: Option<u64>,
    /// Unix seconds.
    pub time: u64,
}

impl Entry {
    pub fn new(result: &RunResult, time: u64) -> Self {
        Self {
            source: result.trigger.label().to_string(),
            regions: result.what.clone(),
            freed: result.change.map(|c| c.freed),
            time,
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "source": self.source,
            "regions": self.regions,
            "freed": self.freed,
            "time": self.time,
        })
    }

    fn from_json(value: &Value) -> Option<Self> {
        let text = |v: &Value| {
            v.as_str()
                .map(|s| s.chars().take(MAX_FIELD).collect::<String>())
        };
        Some(Self {
            source: text(value.get("source")?)?,
            regions: value
                .get("regions")?
                .as_array()?
                .iter()
                .filter_map(text)
                .collect(),
            freed: value.get("freed").and_then(Value::as_u64),
            time: value.get("time")?.as_u64()?,
        })
    }
}

/// Where the log text lives, so the bounds are tested without touching the disk.
pub trait Store {
    fn read(&self) -> Option<String>;
    fn write(&mut self, text: &str) -> Result<(), String>;
    fn delete(&mut self) -> Result<(), String>;
}

pub struct FileStore(PathBuf);

impl FileStore {
    /// `<Toolboks data folder>/addon-data/toolboks.memory-cleaner/clean-log.json`.
    pub fn default_location() -> Self {
        Self(
            toolboks_shared::storage::data_root()
                .join("addon-data")
                .join(ID)
                .join(FILE_NAME),
        )
    }
}

impl Store for FileStore {
    fn read(&self) -> Option<String> {
        std::fs::read_to_string(&self.0).ok()
    }

    fn write(&mut self, text: &str) -> Result<(), String> {
        toolboks_shared::storage::write_atomic(&self.0, text.as_bytes())
    }

    fn delete(&mut self) -> Result<(), String> {
        match std::fs::remove_file(&self.0) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
            _ => Ok(()),
        }
    }
}

/// The entries in the file; a missing, corrupt or wrongly shaped file is an empty log, and
/// entries that do not parse are skipped.
pub fn load(store: &dyn Store) -> Vec<Entry> {
    store
        .read()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|v| {
            v.as_array()
                .map(|a| a.iter().filter_map(Entry::from_json).collect())
        })
        .unwrap_or_default()
}

fn render(entries: &[Entry]) -> String {
    Value::Array(entries.iter().map(Entry::to_json).collect()).to_string()
}

/// Adds one entry, drops the oldest until both caps hold, and returns how many are kept.
pub fn append(store: &mut dyn Store, entry: Entry) -> Result<usize, String> {
    let mut entries = load(store);
    entries.push(entry);
    entries.drain(..entries.len().saturating_sub(MAX_ENTRIES));
    let mut text = render(&entries);
    while text.len() > MAX_BYTES && !entries.is_empty() {
        entries.remove(0);
        text = render(&entries);
    }
    store.write(&text)?;
    Ok(entries.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::freed::Change;
    use crate::memlist::Lists;
    use crate::report::Trigger;

    #[derive(Default)]
    struct Mem {
        text: Option<String>,
        fail_write: bool,
        fail_delete: bool,
    }

    impl Store for Mem {
        fn read(&self) -> Option<String> {
            self.text.clone()
        }
        fn write(&mut self, text: &str) -> Result<(), String> {
            if self.fail_write {
                return Err("disk full".into());
            }
            self.text = Some(text.to_string());
            Ok(())
        }
        fn delete(&mut self) -> Result<(), String> {
            if self.fail_delete {
                return Err("locked".into());
            }
            self.text = None;
            Ok(())
        }
    }

    fn entry(time: u64) -> Entry {
        Entry {
            source: "Manual".into(),
            regions: vec!["Working set".into(), "Standby list".into()],
            freed: Some(time * 1000),
            time,
        }
    }

    #[test]
    fn entries_round_trip_in_order() {
        let mut store = Mem::default();
        assert_eq!(append(&mut store, entry(1)), Ok(1));
        assert_eq!(append(&mut store, entry(2)), Ok(2));
        assert_eq!(load(&store), vec![entry(1), entry(2)]);
        let none = Entry {
            freed: None,
            ..entry(3)
        };
        append(&mut store, none.clone()).unwrap();
        assert_eq!(load(&store).last(), Some(&none));
    }

    #[test]
    fn the_entry_cap_drops_the_oldest() {
        let mut store = Mem::default();
        for t in 0..(MAX_ENTRIES as u64 + 5) {
            append(&mut store, entry(t)).unwrap();
        }
        let kept = load(&store);
        assert_eq!(kept.len(), MAX_ENTRIES);
        assert_eq!(kept[0].time, 5);
        assert_eq!(kept.last().unwrap().time, MAX_ENTRIES as u64 + 4);
    }

    #[test]
    fn the_byte_cap_drops_the_oldest() {
        let mut store = Mem::default();
        // Hand-written long entries (fields are cut to 64 chars on read, so write them raw).
        let long = |t: u64| Entry {
            source: "x".repeat(MAX_FIELD),
            regions: vec!["y".repeat(MAX_FIELD); 8],
            freed: Some(t),
            time: t,
        };
        for t in 0..100 {
            let kept = append(&mut store, long(t)).unwrap();
            assert!(store.text.as_ref().unwrap().len() <= MAX_BYTES);
            assert_eq!(kept, load(&store).len());
        }
        let kept = load(&store);
        assert!(kept.len() < 100, "byte cap never bit: {}", kept.len());
        assert_eq!(kept.last().unwrap().time, 99);
        assert_eq!(kept[0].time, 100 - kept.len() as u64);
    }

    #[test]
    fn a_corrupt_or_odd_file_is_an_empty_log() {
        for text in ["not json", "{\"a\":1}", "[1,2,3]", "", "[{\"source\":5}]"] {
            let mut store = Mem {
                text: Some(text.into()),
                ..Default::default()
            };
            assert!(load(&store).is_empty(), "{text}");
            // The next clean replaces it with a good file.
            assert_eq!(append(&mut store, entry(1)), Ok(1), "{text}");
            assert_eq!(load(&store), vec![entry(1)]);
        }
        assert!(load(&Mem::default()).is_empty());
        // One bad entry does not hide the good ones.
        let mixed = Mem {
            text: Some(format!("[{{\"x\":1}},{}]", entry(7).to_json())),
            ..Default::default()
        };
        assert_eq!(load(&mixed), vec![entry(7)]);
    }

    #[test]
    fn long_hand_edited_fields_are_cut() {
        let store = Mem {
            text: Some(format!(
                "[{{\"source\":\"{}\",\"regions\":[\"{}\"],\"freed\":1,\"time\":2}}]",
                "s".repeat(500),
                "r".repeat(500)
            )),
            ..Default::default()
        };
        let kept = load(&store);
        assert_eq!(kept[0].source.len(), MAX_FIELD);
        assert_eq!(kept[0].regions[0].len(), MAX_FIELD);
    }

    #[test]
    fn a_failed_write_is_reported_and_keeps_the_old_log() {
        let mut store = Mem::default();
        append(&mut store, entry(1)).unwrap();
        store.fail_write = true;
        assert_eq!(append(&mut store, entry(2)), Err("disk full".to_string()));
        assert_eq!(load(&store), vec![entry(1)]);
    }

    #[test]
    fn delete_empties_the_log_and_reports_a_failure() {
        let mut store = Mem::default();
        append(&mut store, entry(1)).unwrap();
        store.fail_delete = true;
        assert_eq!(store.delete(), Err("locked".to_string()));
        assert_eq!(load(&store).len(), 1);
        store.fail_delete = false;
        store.delete().unwrap();
        assert!(load(&store).is_empty());
    }

    #[test]
    fn the_real_file_store_writes_reads_and_deletes() {
        let dir = std::env::temp_dir().join(format!("tb-memory-log-{}", std::process::id()));
        let mut store = FileStore(dir.join("sub").join(FILE_NAME));
        assert!(load(&store).is_empty());
        // Deleting a log that was never written is fine.
        store.delete().unwrap();
        append(&mut store, entry(1)).unwrap();
        assert_eq!(load(&store), vec![entry(1)]);
        store.delete().unwrap();
        assert!(load(&store).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_entry_holds_only_source_regions_freed_and_time() {
        let change = Change::between(
            Lists {
                free: 0,
                standby: 0,
                modified: 0,
            },
            Lists {
                free: 5,
                standby: 0,
                modified: 0,
            },
            None,
        );
        let result = RunResult {
            trigger: Trigger::Interval,
            when: "10:00:00".into(),
            what: vec!["Idle apps".into()],
            change: Some(change),
            detail: vec!["2 apps gave up 5 MB".into()],
        };
        let json = Entry::new(&result, 42).to_json();
        let mut keys: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["freed", "regions", "source", "time"]);
        assert_eq!(json["source"], "Auto interval");
        assert_eq!(json["freed"], 5);
        assert_eq!(json["time"], 42);
    }

    #[test]
    fn options_default_to_asking_with_no_log_and_store_only_changes() {
        let mut prefs = BTreeMap::new();
        let defaults = Options::load(&prefs);
        assert!(defaults.confirm_manual && !defaults.log);
        assert!(!defaults.store(&mut prefs));
        assert!(prefs.is_empty());
        let chosen = Options {
            confirm_manual: false,
            log: true,
        };
        assert!(chosen.store(&mut prefs));
        assert!(!chosen.store(&mut prefs));
        assert_eq!(Options::load(&prefs), chosen);
        // Switching back to the default after saving is a change that must be written.
        assert!(defaults.store(&mut prefs));
        prefs.insert(PREFERENCE_KEY.into(), json!("junk"));
        assert_eq!(Options::load(&prefs), defaults);
    }
}
