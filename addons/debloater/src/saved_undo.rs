//! The saved undo list: what Debloater needs to undo its own changes after Toolboks closes or
//! Windows restarts (decision Gusseti/toolboks#52, issue #81).
//!
//! One small file, `addon-data/toolboks.debloater/undo/undo.jsonl`, in the add-on's own data folder,
//! so it is deleted with the add-on when the person deletes its data. It is rewritten (temp file,
//! then rename) after every change and every undo, and removed when nothing is left to undo.
//!
//! The file has two lines: a header with the format name, the version, the body's length and a
//! checksum, then the body. Only what undo needs is kept: the previous value of each registry value,
//! service start type, task switch and startup approval Debloater changed, what Debloater set it to
//! (so an undo of an item that something else changed since is refused, never forced), and the
//! catalog ids of removed apps (they come back from the Microsoft Store). No user names, no paths,
//! no commands, no file contents. A file that is damaged, from a newer Debloater, or names
//! something this version does not offer is reported in plain words and never guessed at.
//!
//! TODO(Gusseti/toolboks#154): once a released host reads the manifest `data_note`, declare this
//! file there: "Keeps the list of changes you made so you can undo them later (registry values,
//! services, tasks, startup entries)." Not added yet: the pinned host does not know the field.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::admin;
use toolboks_debloat_engine::{
    apps,
    backend::Registry,
    model::RegValue,
    plan::{self, Outcome, Run, UndoOp},
    services::{self, ServiceState, Services, StartType},
    startup::{self, StartupStore},
    tasks::{self, TaskState, Tasks},
};

/// The file format this build writes and reads.
pub const VERSION: u64 = 1;
const FORMAT: &str = "toolboks.debloater.undo";
const FILE: &str = "undo.jsonl";
/// Damaged files are moved to `undo-damaged-1.jsonl` (newest) .. `-3`, so nothing is lost and they
/// are never read again. A fourth one drops the oldest copy.
const DAMAGED_COPIES: usize = 3;

fn damaged_name(slot: usize) -> String {
    format!("undo-damaged-{slot}.jsonl")
}
/// The list is a few kilobytes; anything near this is not a file Debloater wrote.
const MAX_BYTES: u64 = 1024 * 1024;

// ---- steps with the state Debloater left them in ------------------------------------------------

/// An undo step plus what Debloater set the item to. `after` is `None` for a step kept after a
/// change that failed and could not be rolled back: its state is not known, so its undo is the
/// retry the error message asked for and is not checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tracked<T, V> {
    pub step: T,
    pub after: Option<V>,
}

pub type RegStep = Tracked<UndoOp, RegValue>;
pub type ServiceStep = Tracked<services::UndoStep, StartType>;
/// `after`: the task was left switched on (`true`) or off.
pub type TaskStep = Tracked<tasks::UndoStep, bool>;
/// `after`: the entry was left switched on (`true`) or off.
pub type StartupStep = Tracked<startup::UndoStep, bool>;

/// A step whose change failed and was not rolled back; see [`Tracked`].
pub fn recovery<T, V>(step: T) -> Tracked<T, V> {
    Tracked { step, after: None }
}

/// The undo steps of an apply run. Values of an item that was fully applied hold the item's off
/// value; values of an item that failed are recovery steps.
pub fn track_run(run: &Run) -> Vec<RegStep> {
    run.undo
        .iter()
        .map(|op| {
            let done = run
                .applied
                .iter()
                .any(|a| a.id == op.id && a.outcome == Outcome::Done);
            let after = done
                .then(|| admin::item_by_id(op.id))
                .flatten()
                .and_then(|item| {
                    item.settings
                        .iter()
                        .find(|s| s.hive == op.hive && s.path == op.path && s.name == op.name)
                })
                .map(|s| RegValue::Dword(s.off_value));
            Tracked {
                step: op.clone(),
                after,
            }
        })
        .collect()
}

/// A service that was switched off: it was left at the catalog's target start type.
pub fn service_done(step: services::UndoStep) -> ServiceStep {
    let after = admin::service_by_id(step.id).map(|def| def.target);
    Tracked { step, after }
}

/// A task that was switched: `set_enabled` only changes a task to the opposite of what it was.
pub fn task_done(step: tasks::UndoStep) -> TaskStep {
    let after = Some(!step.was_enabled);
    Tracked { step, after }
}

/// A startup entry that was switched on (`enabled`) or off.
pub fn startup_done(step: startup::UndoStep, enabled: bool) -> StartupStep {
    Tracked {
        step,
        after: Some(enabled),
    }
}

// ---- the codec -------------------------------------------------------------------------------

fn with_after(mut value: Value, after: Value) -> Value {
    value["after"] = after;
    value
}

pub fn reg_json(step: &RegStep) -> Value {
    with_after(admin::undo_json(&step.step), admin::value_json(&step.after))
}

pub fn reg_from(value: &Value) -> Result<RegStep, String> {
    Ok(Tracked {
        step: admin::undo_from_json(value)?,
        after: admin::value_from(&value["after"])?,
    })
}

pub fn service_json(step: &ServiceStep) -> Value {
    let after = step.after.and_then(admin::start_name);
    with_after(admin::service_step_json(&step.step), json!(after))
}

pub fn service_from(value: &Value) -> Result<ServiceStep, String> {
    let after = match &value["after"] {
        Value::Null => None,
        Value::String(name) => Some(
            admin::start_from(name)
                .ok_or("An undo step has a start type this add-on does not write.")?,
        ),
        _ => return Err("An undo step has a start type this add-on does not write.".into()),
    };
    Ok(Tracked {
        step: admin::service_step_from(value)?,
        after,
    })
}

fn switch_after(value: &Value) -> Result<Option<bool>, String> {
    match &value["after"] {
        Value::Null => Ok(None),
        Value::Bool(on) => Ok(Some(*on)),
        _ => Err("An undo step does not say whether the item was left on or off.".into()),
    }
}

pub fn task_json(step: &TaskStep) -> Value {
    with_after(admin::task_step_json(&step.step), json!(step.after))
}

pub fn task_from(value: &Value) -> Result<TaskStep, String> {
    Ok(Tracked {
        step: admin::task_step_from(value)?,
        after: switch_after(value)?,
    })
}

pub fn startup_json(step: &StartupStep) -> Value {
    with_after(admin::startup_step_json(&step.step), json!(step.after))
}

pub fn startup_from(value: &Value) -> Result<StartupStep, String> {
    Ok(Tracked {
        step: admin::startup_step_from(value)?,
        after: switch_after(value)?,
    })
}

// ---- undo, refusing items that something else changed since -----------------------------------

/// How one undo step ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepResult {
    Done,
    /// It did not work; the step stays for another try.
    Failed(String),
    /// The item is no longer the way Debloater left it, so it was not touched. The step is dropped:
    /// it no longer describes the item. The text says why, in plain words.
    Refused(String),
}

/// What an undo batch did.
#[derive(Debug, PartialEq, Eq)]
pub struct Tally<T> {
    pub done: usize,
    /// Steps that failed or were not tried, newest first (the order given).
    pub kept: Vec<T>,
    /// Why each refused step was left alone.
    pub refused: Vec<String>,
}

/// Undoes `steps` in the order given (newest first). A step that failed or was refused blocks the
/// older steps of the same item, so an older value is never put back over a newer one.
pub fn undo_steps<T, K: PartialEq>(
    steps: Vec<T>,
    key: impl Fn(&T) -> K,
    mut undo_one: impl FnMut(&T) -> StepResult,
) -> Tally<T> {
    let mut tally = Tally {
        done: 0,
        kept: Vec::new(),
        refused: Vec::new(),
    };
    let mut blocked: Vec<K> = Vec::new();
    for step in steps {
        let step_key = key(&step);
        if blocked.contains(&step_key) {
            tally.kept.push(step);
            continue;
        }
        match undo_one(&step) {
            StepResult::Done => tally.done += 1,
            StepResult::Failed(_) => {
                blocked.push(step_key);
                tally.kept.push(step);
            }
            StepResult::Refused(reason) => {
                blocked.push(step_key);
                tally.refused.push(reason);
            }
        }
    }
    tally
}

fn changed_since(title: &str) -> String {
    format!("{title} was changed by something else after Debloater changed it, so it was left as it is.")
}

fn gone(title: &str) -> String {
    format!("{title} is no longer on this PC, so it was left alone.")
}

/// Puts one registry value back, unless it no longer holds what Debloater wrote.
pub fn undo_registry(step: &RegStep, reg: &mut dyn Registry) -> StepResult {
    let op = &step.step;
    let title = admin::item_by_id(op.id).map_or(op.id, |item| item.title);
    if let Some(after) = &step.after {
        match reg.read(op.hive, op.path, op.name) {
            Err(error) => return StepResult::Failed(error),
            Ok(now) if now.as_ref() != Some(after) => {
                return StepResult::Refused(changed_since(title))
            }
            Ok(_) => {}
        }
    }
    let mut one = vec![op.clone()];
    plan::undo(&mut one, reg);
    if one.is_empty() {
        StepResult::Done
    } else {
        StepResult::Failed(format!("{title}: Windows did not take the previous value"))
    }
}

/// Puts a service's start type back, unless it no longer has the one Debloater set.
pub fn undo_service(step: &ServiceStep, svc: &mut dyn Services) -> StepResult {
    let title = admin::service_by_id(step.step.id).map_or(step.step.name, |def| def.title);
    if let Some(after) = step.after {
        match svc.state(step.step.name) {
            ServiceState::Present { start, .. } if start != after => {
                return StepResult::Refused(changed_since(title))
            }
            ServiceState::Present { .. } => {}
            ServiceState::NotPresent => return StepResult::Refused(gone(title)),
            ServiceState::Locked => return StepResult::Failed("Protected by Windows".into()),
            ServiceState::Unknown(error) => return StepResult::Failed(error),
        }
    }
    match services::undo(svc, &step.step) {
        Ok(()) => StepResult::Done,
        Err(error) => StepResult::Failed(error),
    }
}

/// Switches a task back, unless it is no longer switched the way Debloater left it.
pub fn undo_task(step: &TaskStep, list: &mut dyn Tasks) -> StepResult {
    let title = admin::task_by_id(step.step.id).map_or(step.step.path, |def| def.title);
    if let Some(after) = step.after {
        match list.state(step.step.path) {
            TaskState::Enabled if !after => return StepResult::Refused(changed_since(title)),
            TaskState::Disabled if after => return StepResult::Refused(changed_since(title)),
            TaskState::Enabled | TaskState::Disabled => {}
            TaskState::NotPresent => return StepResult::Refused(gone(title)),
            TaskState::Protected => return StepResult::Failed("Protected by Windows".into()),
            TaskState::Unknown(error) => return StepResult::Failed(error),
        }
    }
    match tasks::undo(list, &step.step) {
        Ok(()) => StepResult::Done,
        Err(error) => StepResult::Failed(error),
    }
}

/// The startup entry (its Run value or Startup folder file) is still there.
fn entry_exists(
    store: &dyn StartupStore,
    source: startup::Source,
    name: &str,
) -> Result<bool, String> {
    Ok(store
        .list()?
        .iter()
        .any(|entry| entry.source == source && entry.name == name))
}

/// Takes the steps from `from` on whose startup entry is gone out of `steps`, and says why for
/// each. A stale step is never sent to the helper, so it cannot block the undo of the others.
pub fn drop_gone_startup(
    steps: &mut Vec<StartupStep>,
    from: usize,
    exists: impl Fn(&StartupStep) -> bool,
) -> Vec<String> {
    let batch = steps.split_off(from.min(steps.len()));
    let mut reasons = Vec::new();
    for step in batch {
        if exists(&step) {
            steps.push(step);
        } else {
            reasons.push(gone(&step.step.name));
        }
    }
    reasons
}

/// Why a startup step whose entry is gone was left alone.
pub fn startup_gone(name: &str) -> String {
    gone(name)
}

/// Switches a startup entry back, unless the entry is gone or no longer switched the way
/// Debloater left it.
pub fn undo_startup(step: &StartupStep, store: &mut dyn StartupStore) -> StepResult {
    let (source, name) = (step.step.source, step.step.name.as_str());
    match entry_exists(store, source, name) {
        Err(error) => return StepResult::Failed(error),
        Ok(false) => return StepResult::Refused(gone(name)),
        Ok(true) => {}
    }
    if let Some(after) = step.after {
        match store.approval(source, name) {
            Err(error) => return StepResult::Failed(error),
            Ok(now) if now.is_none() || startup::is_enabled(now.as_deref()) != after => {
                return StepResult::Refused(changed_since(name))
            }
            Ok(_) => {}
        }
    }
    match startup::undo(store, &step.step) {
        Ok(()) => StepResult::Done,
        Err(error) => StepResult::Failed(error),
    }
}

// ---- removed apps ------------------------------------------------------------------------------

/// Notes the apps a removal run took away, by catalog id only (never the full package name).
/// Returns true if the list changed.
pub fn record_removed(list: &mut Vec<&'static str>, run: &apps::AppRun) -> bool {
    let before = list.len();
    for (id, _) in &run.removed {
        if !list.contains(id) {
            list.push(id);
        }
    }
    list.len() != before
}

/// Forgets the apps that are installed again. Returns true if the list changed.
pub fn forget_reinstalled(list: &mut Vec<&'static str>, installed: impl Fn(&str) -> bool) -> bool {
    let before = list.len();
    list.retain(|id| !installed(id));
    list.len() != before
}

// ---- the list and its file ---------------------------------------------------------------------

/// Everything that can still be undone, oldest first in every list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UndoList {
    /// Privacy and ads: one list per apply run.
    pub registry_runs: Vec<Vec<RegStep>>,
    pub startup: Vec<StartupStep>,
    pub tasks: Vec<TaskStep>,
    pub services: Vec<ServiceStep>,
    /// Catalog ids of apps Debloater removed; they come back from the Microsoft Store.
    pub removed_apps: Vec<&'static str>,
    /// Saved entries this version could not read (an item it no longer offers). Kept exactly as
    /// they were and never used.
    pub unreadable: Vec<Value>,
}

impl UndoList {
    pub fn is_empty(&self) -> bool {
        self.registry_runs.iter().all(Vec::is_empty)
            && self.startup.is_empty()
            && self.tasks.is_empty()
            && self.services.is_empty()
            && self.removed_apps.is_empty()
            && self.unreadable.is_empty()
    }

    fn body(&self) -> Value {
        json!({
            "registry_runs": self
                .registry_runs
                .iter()
                .filter(|run| !run.is_empty())
                .map(|run| run.iter().map(reg_json).collect::<Vec<_>>())
                .collect::<Vec<_>>(),
            "startup": self.startup.iter().map(startup_json).collect::<Vec<_>>(),
            "tasks": self.tasks.iter().map(task_json).collect::<Vec<_>>(),
            "services": self.services.iter().map(service_json).collect::<Vec<_>>(),
            "removed_apps": self.removed_apps,
            "unreadable": self.unreadable,
        })
    }

    /// Reads every entry it can. An entry this version cannot read goes to `unreadable` with the
    /// list it came from, so it is written back unchanged and never used.
    fn from_body(body: &Value) -> Result<UndoList, String> {
        if !body.is_object() {
            return Err("the list is not in the expected form".into());
        }
        let mut list = UndoList::default();
        let mut odd: Vec<Value> = Vec::new();
        for run in array(body, "registry_runs")? {
            let steps = run
                .as_array()
                .ok_or("a privacy and ads run is not a list")?;
            let mut read = Vec::new();
            for entry in steps {
                match reg_from(entry) {
                    Ok(step) => read.push(step),
                    Err(_) => odd.push(list_unreadable("registry", entry)),
                }
            }
            if !read.is_empty() {
                list.registry_runs.push(read);
            }
        }
        read_section(body, "startup", startup_from, &mut list.startup, &mut odd)?;
        read_section(body, "tasks", task_from, &mut list.tasks, &mut odd)?;
        read_section(body, "services", service_from, &mut list.services, &mut odd)?;
        for entry in array(body, "removed_apps")? {
            let known = entry
                .as_str()
                .and_then(|id| apps::curated().iter().find(|c| c.id == id));
            match known {
                Some(curated) if !list.removed_apps.contains(&curated.id) => {
                    list.removed_apps.push(curated.id)
                }
                Some(_) => {}
                None => odd.push(list_unreadable("removed_apps", entry)),
            }
        }
        for entry in array(body, "unreadable")? {
            odd.push(entry.clone());
        }
        list.unreadable = odd;
        Ok(list)
    }
}

fn list_unreadable(section: &str, entry: &Value) -> Value {
    json!({ "section": section, "entry": entry })
}

fn array<'a>(body: &'a Value, name: &str) -> Result<&'a Vec<Value>, String> {
    match &body[name] {
        Value::Array(items) => Ok(items),
        _ => Err(format!("the list has no '{name}' part")),
    }
}

fn read_section<T>(
    body: &Value,
    name: &str,
    read: impl Fn(&Value) -> Result<T, String>,
    into: &mut Vec<T>,
    odd: &mut Vec<Value>,
) -> Result<(), String> {
    for entry in array(body, name)? {
        match read(entry) {
            Ok(step) => into.push(step),
            Err(_) => odd.push(list_unreadable(name, entry)),
        }
    }
    Ok(())
}

/// FNV-1a, 64 bit: catches a cut-off or changed file. It is not a seal against someone who can
/// write the file; every step is still checked against the catalog before it is used.
fn checksum(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// The file's bytes: a header line, then the body.
pub fn encode(list: &UndoList) -> Vec<u8> {
    let body = list.body().to_string();
    let header = json!({
        "format": FORMAT,
        "version": VERSION,
        "bytes": body.len(),
        "fnv1a64": format!("{:016x}", checksum(body.as_bytes())),
    });
    let mut out = header.to_string().into_bytes();
    out.push(b'\n');
    out.extend_from_slice(body.as_bytes());
    out
}

/// Why a file could not be used.
#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Cut off, changed or not a Debloater undo file.
    Damaged(String),
    /// Written by a newer Debloater; this version must not read or replace it.
    Newer(u64),
}

pub fn decode(bytes: &[u8]) -> Result<UndoList, DecodeError> {
    let damaged = |why: &str| DecodeError::Damaged(why.to_string());
    let split = bytes
        .iter()
        .position(|byte| *byte == b'\n')
        .ok_or_else(|| damaged("it has no header"))?;
    let (header, body) = (&bytes[..split], &bytes[split + 1..]);
    let header: Value =
        serde_json::from_slice(header).map_err(|_| damaged("its header cannot be read"))?;
    if header["format"].as_str() != Some(FORMAT) {
        return Err(damaged("it is not a Debloater undo list"));
    }
    let version = header["version"]
        .as_u64()
        .ok_or_else(|| damaged("it has no version"))?;
    if version > VERSION {
        return Err(DecodeError::Newer(version));
    }
    if version != VERSION {
        return Err(damaged("it has an unknown version"));
    }
    if header["bytes"].as_u64() != Some(body.len() as u64) {
        return Err(damaged("it is cut off or has extra data"));
    }
    if header["fnv1a64"].as_str() != Some(format!("{:016x}", checksum(body)).as_str()) {
        return Err(damaged("its contents changed after it was saved"));
    }
    let body: Value =
        serde_json::from_slice(body).map_err(|_| damaged("its list cannot be read"))?;
    UndoList::from_body(&body).map_err(DecodeError::Damaged)
}

/// What was found on disk.
#[derive(Debug, PartialEq, Eq)]
pub enum Loaded {
    /// No saved list.
    Nothing,
    List(UndoList),
    /// Not used. It was moved to `undo-damaged-1.jsonl` in the same folder (`kept`), so it is never
    /// read again and new changes can be saved. If it could not be moved, it is left in place and
    /// must never be replaced.
    Damaged {
        reason: String,
        kept: bool,
    },
    /// From a newer Debloater: left untouched, and not overwritten.
    Newer(u64),
    /// The file is there but could not be read (locked, no access): left untouched.
    Unavailable(String),
}

/// The undo folder of the add-on.
pub struct UndoStore {
    dir: PathBuf,
}

/// Installed add-ons run from `<root>/addons/<id>/<version>/`, so the host's real data root
/// (including Preview's) is known from the exe. A build run from elsewhere uses the shared root.
/// The same rule as Disk Manager's saved scans.
fn directory_for(exe: &Path, fallback: PathBuf) -> PathBuf {
    let installed = exe
        .parent()
        .and_then(Path::parent)
        .filter(|p| p.file_name().is_some_and(|n| n == crate::ID))
        .and_then(Path::parent)
        .filter(|p| p.file_name().is_some_and(|n| n == "addons"))
        .and_then(Path::parent);
    installed
        .map(Path::to_owned)
        .unwrap_or(fallback)
        .join("addon-data")
        .join(crate::ID)
        .join("undo")
}

impl UndoStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// `addon-data/toolboks.debloater/undo` under the host's data folder. No disk access.
    pub fn default_dir() -> PathBuf {
        directory_for(
            &std::env::current_exe().unwrap_or_default(),
            toolboks_shared::storage::data_root(),
        )
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn file(&self) -> PathBuf {
        self.dir.join(FILE)
    }

    pub fn load(&self) -> Loaded {
        let path = self.file();
        let size = match fs::metadata(&path) {
            Ok(meta) => meta.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Loaded::Nothing,
            Err(error) => return Loaded::Unavailable(error.to_string()),
        };
        let result = if size > MAX_BYTES {
            Err(DecodeError::Damaged(
                "it is far larger than an undo list".into(),
            ))
        } else {
            match fs::read(&path) {
                Ok(bytes) => decode(&bytes),
                Err(error) => return Loaded::Unavailable(error.to_string()),
            }
        };
        match result {
            Ok(list) => Loaded::List(list),
            Err(DecodeError::Newer(version)) => Loaded::Newer(version),
            Err(DecodeError::Damaged(reason)) => Loaded::Damaged {
                reason,
                kept: self.set_aside(&path),
            },
        }
    }

    /// Moves a damaged file to slot 1, after shifting older copies up; the oldest copy is the only
    /// one ever deleted. A copy only moves into an empty slot, so nothing is overwritten. Returns
    /// false if the file had to stay where it is.
    fn set_aside(&self, path: &Path) -> bool {
        let _ = fs::remove_file(self.dir.join(damaged_name(DAMAGED_COPIES)));
        for slot in (1..DAMAGED_COPIES).rev() {
            let (from, to) = (
                self.dir.join(damaged_name(slot)),
                self.dir.join(damaged_name(slot + 1)),
            );
            if from.exists() && !to.exists() {
                let _ = fs::rename(&from, &to);
            }
        }
        let first = self.dir.join(damaged_name(1));
        !first.exists() && fs::rename(path, &first).is_ok()
    }

    /// Writes the list (temp file, flushed to disk, then renamed over the old one), or removes the
    /// file when nothing is left to undo.
    pub fn save(&self, list: &UndoList) -> Result<(), String> {
        let path = self.file();
        if list.is_empty() {
            return match fs::remove_file(&path) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    Err(error.to_string())
                }
                _ => {
                    let _ = fs::remove_dir(&self.dir);
                    Ok(())
                }
            };
        }
        fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        let temp = self.dir.join(format!("{FILE}.pending"));
        let written = fs::File::create(&temp).and_then(|mut file| {
            file.write_all(&encode(list))?;
            file.sync_all()
        });
        if let Err(error) = written.and_then(|()| fs::rename(&temp, &path)) {
            let _ = fs::remove_file(&temp);
            return Err(error.to_string());
        }
        Ok(())
    }

    /// Removes the saved list, the damaged copies and the folder.
    pub fn delete(&self) -> Result<(), String> {
        let mut names = vec![FILE.to_string(), format!("{FILE}.pending")];
        names.extend((1..=DAMAGED_COPIES).map(damaged_name));
        for name in names {
            match fs::remove_file(self.dir.join(name)) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    return Err(error.to_string())
                }
                _ => {}
            }
        }
        let _ = fs::remove_dir(&self.dir);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use toolboks_debloat_engine::{
        catalog,
        model::{Edition, Env, Group, Hive, Item},
        startup::{Class, Entry, RawEntry, Source},
    };

    /// A fresh folder per test, removed afterwards.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "toolboks-debloater-undo-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&dir);
            Self(dir)
        }
        fn store(&self) -> UndoStore {
            UndoStore::new(self.0.join("undo"))
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    // ---- fakes ----

    #[derive(Default)]
    struct FakeRegistry {
        values: HashMap<(Hive, String, String), RegValue>,
        /// Value names whose writes and deletes fail.
        broken: Vec<&'static str>,
        /// Value names whose writes "work" but change nothing (a policy wins).
        ignored: Vec<&'static str>,
    }

    impl Registry for FakeRegistry {
        fn read(&self, hive: Hive, path: &str, name: &str) -> Result<Option<RegValue>, String> {
            Ok(self.values.get(&(hive, path.into(), name.into())).cloned())
        }
        fn write(
            &mut self,
            hive: Hive,
            path: &str,
            name: &str,
            value: &RegValue,
        ) -> Result<(), String> {
            if self.broken.contains(&name) {
                return Err("access denied".into());
            }
            if !self.ignored.contains(&name) {
                self.values
                    .insert((hive, path.into(), name.into()), value.clone());
            }
            Ok(())
        }
        fn delete(&mut self, hive: Hive, path: &str, name: &str) -> Result<(), String> {
            if self.broken.contains(&name) {
                return Err("access denied".into());
            }
            self.values.remove(&(hive, path.into(), name.into()));
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeServices(HashMap<String, StartType>);

    impl Services for FakeServices {
        fn state(&self, name: &str) -> ServiceState {
            match self.0.get(name) {
                Some(start) => ServiceState::Present {
                    start: *start,
                    running: false,
                    active_dependents: Vec::new(),
                },
                None => ServiceState::NotPresent,
            }
        }
        fn set_start(&mut self, name: &str, start: StartType) -> Result<(), String> {
            self.0.insert(name.into(), start);
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeTasks(HashMap<String, bool>);

    impl Tasks for FakeTasks {
        fn state(&self, path: &str) -> TaskState {
            match self.0.get(path) {
                Some(true) => TaskState::Enabled,
                Some(false) => TaskState::Disabled,
                None => TaskState::NotPresent,
            }
        }
        fn set_enabled(&mut self, path: &str, enabled: bool) -> Result<(), String> {
            self.0.insert(path.into(), enabled);
            Ok(())
        }
    }

    /// Approval values, and the entries that exist.
    #[derive(Default)]
    struct FakeStartup(HashMap<(Source, String), Vec<u8>>, Vec<(Source, String)>);

    impl FakeStartup {
        fn with(entries: &[(Source, &str)]) -> Self {
            Self(
                HashMap::new(),
                entries
                    .iter()
                    .map(|(source, name)| (*source, name.to_string()))
                    .collect(),
            )
        }
    }

    impl StartupStore for FakeStartup {
        fn list(&self) -> Result<Vec<RawEntry>, String> {
            Ok(self
                .1
                .iter()
                .map(|(source, name)| RawEntry {
                    source: *source,
                    name: name.clone(),
                    command: String::new(),
                })
                .collect())
        }
        fn approval(&self, source: Source, name: &str) -> Result<Option<Vec<u8>>, String> {
            Ok(self.0.get(&(source, name.to_string())).cloned())
        }
        fn set_approval(
            &mut self,
            source: Source,
            name: &str,
            bytes: Option<&[u8]>,
        ) -> Result<(), String> {
            match bytes {
                Some(bytes) => self.0.insert((source, name.into()), bytes.to_vec()),
                None => self.0.remove(&(source, name.into())),
            };
            Ok(())
        }
    }

    const ENV: Env = Env {
        edition: Edition::Unknown,
        elevated: false,
    };

    /// Privacy and ads items that only touch the current user's registry.
    fn user_items() -> Vec<&'static Item> {
        catalog::items()
            .iter()
            .filter(|item| item.group == Group::PrivacyAds && !item.needs_admin())
            .collect()
    }

    fn service_def() -> &'static services::ServiceDef {
        services::catalog()
            .iter()
            .find(|def| !services::is_never_offered(def.name))
            .expect("the catalog offers a service")
    }

    fn task_def() -> &'static tasks::TaskDef {
        tasks::catalog()
            .iter()
            .find(|def| !tasks::is_never_offered(def.path))
            .expect("the catalog offers a task")
    }

    const ENTRY_NAME: &str = "Chat Helper";

    fn entry() -> Entry {
        Entry {
            source: Source::UserRun,
            name: ENTRY_NAME.into(),
            command: "\"C:\\Users\\someone\\AppData\\Local\\Chat\\helper.exe\" --tray".into(),
            enabled: true,
            publisher: Some("Example Corp".into()),
            class: Class::Suggest,
        }
    }

    fn reopen(dir: &TempDir) -> UndoList {
        match dir.store().load() {
            Loaded::List(list) => list,
            other => panic!("expected a list, got {other:?}"),
        }
    }

    fn newest_first<T: Clone>(steps: &[T]) -> Vec<T> {
        steps.iter().rev().cloned().collect()
    }

    // ---- apply, close Toolboks, start again, undo ----

    #[test]
    fn every_kind_of_change_can_be_undone_after_a_restart() {
        let dir = TempDir::new();
        let item = user_items()[0];
        let mut reg = FakeRegistry::default();
        let run = plan::apply(catalog::items(), &[item.id], &mut reg, ENV);
        assert!(run.applied.iter().all(|a| a.outcome == Outcome::Done));
        let reg_steps = track_run(&run);
        assert!(!reg_steps.is_empty() && reg_steps.iter().all(|s| s.after.is_some()));

        let def = service_def();
        let mut svc = FakeServices::default();
        svc.0.insert(def.name.into(), StartType::Auto);
        let service = service_done(services::set_off(&mut svc, def).unwrap());

        let task = task_def();
        let mut list = FakeTasks::default();
        list.0.insert(task.path.into(), true);
        let task_step = task_done(tasks::set_enabled(&mut list, task, false).unwrap());

        let mut store = FakeStartup::with(&[(Source::UserRun, ENTRY_NAME)]);
        let startup = startup_done(
            startup::set_enabled(&mut store, &entry(), false, 133_000_000_000_000_000).unwrap(),
            false,
        );

        let saved = UndoList {
            registry_runs: vec![reg_steps],
            startup: vec![startup],
            tasks: vec![task_step],
            services: vec![service],
            ..UndoList::default()
        };
        dir.store().save(&saved).unwrap();

        // Toolboks closes; a new process reads the file.
        let back = reopen(&dir);
        assert_eq!(back, saved);

        let tally = undo_steps(
            newest_first(&back.registry_runs[0]),
            |s| (s.step.hive, s.step.path, s.step.name),
            |s| undo_registry(s, &mut reg),
        );
        assert_eq!((tally.kept.len(), tally.refused.len()), (0, 0));
        assert!(reg.values.is_empty(), "the values did not exist before");

        assert_eq!(undo_service(&back.services[0], &mut svc), StepResult::Done);
        assert_eq!(svc.0[def.name], StartType::Auto);
        assert_eq!(undo_task(&back.tasks[0], &mut list), StepResult::Done);
        assert!(list.0[task.path]);
        assert_eq!(undo_startup(&back.startup[0], &mut store), StepResult::Done);
        assert!(store.0.is_empty(), "there was no approval value before");

        // Nothing left: saving the empty list removes the file and the folder.
        dir.store().save(&UndoList::default()).unwrap();
        assert!(!dir.store().dir().exists());
        assert_eq!(dir.store().load(), Loaded::Nothing);
    }

    #[test]
    fn only_what_undo_needs_is_written() {
        let dir = TempDir::new();
        let mut store = FakeStartup::default();
        let step = startup_done(
            startup::set_enabled(&mut store, &entry(), false, 1).unwrap(),
            false,
        );
        let mut removed = Vec::new();
        let run = apps::AppRun {
            removed: vec![(
                "clipchamp",
                "Clipchamp.Clipchamp_2.9.1.0_neutral__yxz26nhyzhsrt".into(),
            )],
            failed: Vec::new(),
        };
        assert!(record_removed(&mut removed, &run));
        let list = UndoList {
            startup: vec![step],
            removed_apps: removed,
            ..UndoList::default()
        };
        dir.store().save(&list).unwrap();
        let text = fs::read_to_string(dir.store().dir().join(FILE)).unwrap();
        assert!(text.contains(ENTRY_NAME) && text.contains("clipchamp"));
        for private in [
            "someone",
            "helper.exe",
            "Example Corp",
            "yxz26nhyzhsrt",
            "2.9.1.0",
        ] {
            assert!(!text.contains(private), "{private} was written");
        }
        if let Ok(user) = std::env::var("USERNAME") {
            assert!(!text.contains(&user), "the user name was written");
        }
        assert!(!text.contains(&*dir.0.to_string_lossy()));
    }

    // ---- damaged and newer files ----

    #[test]
    fn a_damaged_file_is_never_used_and_is_kept_aside() {
        let dir = TempDir::new();
        let task = task_def();
        let list = UndoList {
            tasks: vec![task_done(tasks::UndoStep {
                id: task.id,
                path: task.path,
                was_enabled: true,
            })],
            ..UndoList::default()
        };
        let good = encode(&list);
        let file = dir.store().dir().join(FILE);
        let cut = good[..good.len() - 5].to_vec();
        // One changed byte near the end of the body: still the same length, caught by the
        // checksum.
        let mut flipped = good.clone();
        let at = flipped.len() - 10;
        flipped[at] ^= 0x01;
        let bads = [
            cut,
            flipped,
            b"not an undo list".to_vec(),
            Vec::new(),
            b"{\"format\":\"toolboks.debloater.undo\",\"version\":0}\n{}".to_vec(),
        ];
        for bad in &bads {
            fs::create_dir_all(dir.store().dir()).unwrap();
            fs::write(&file, bad).unwrap();
            match dir.store().load() {
                Loaded::Damaged { kept, .. } => assert!(kept),
                other => panic!("damaged file read as {other:?}"),
            }
            assert!(!file.exists());
            assert_eq!(
                &fs::read(dir.store().dir().join(damaged_name(1))).unwrap(),
                bad
            );
            assert_eq!(dir.store().load(), Loaded::Nothing);
        }
        // The three newest damaged copies are kept, newest first; none was overwritten.
        for (slot, bad) in (1..=DAMAGED_COPIES).zip(bads.iter().rev()) {
            assert_eq!(
                &fs::read(dir.store().dir().join(damaged_name(slot))).unwrap(),
                bad
            );
        }
        assert!(!dir.store().dir().join(damaged_name(4)).exists());
        // A good file still reads.
        fs::write(&file, &good).unwrap();
        assert_eq!(reopen(&dir), list);
    }

    #[test]
    fn a_file_from_a_newer_debloater_is_left_untouched() {
        let dir = TempDir::new();
        let bytes =
            b"{\"format\":\"toolboks.debloater.undo\",\"version\":2,\"bytes\":2,\"fnv1a64\":\"x\"}\n{}"
                .to_vec();
        fs::create_dir_all(dir.store().dir()).unwrap();
        let file = dir.store().dir().join(FILE);
        fs::write(&file, &bytes).unwrap();
        assert_eq!(dir.store().load(), Loaded::Newer(2));
        assert_eq!(fs::read(&file).unwrap(), bytes, "the file must not change");
        assert!(!dir.store().dir().join(damaged_name(1)).exists());
        assert_eq!(decode(&bytes), Err(DecodeError::Newer(2)));
    }

    #[test]
    fn entries_this_version_cannot_read_are_kept_but_never_used() {
        let dir = TempDir::new();
        let task = task_def();
        let good = task_json(&task_done(tasks::UndoStep {
            id: task.id,
            path: task.path,
            was_enabled: false,
        }));
        let mut gone = good.clone();
        gone["id"] = json!("an-item-a-newer-catalog-dropped");
        let body = json!({
            "registry_runs": [],
            "startup": [],
            "tasks": [good, gone],
            "services": [],
            "removed_apps": ["not-an-app"],
            "unreadable": [],
        })
        .to_string();
        let header = json!({
            "format": FORMAT,
            "version": VERSION,
            "bytes": body.len(),
            "fnv1a64": format!("{:016x}", checksum(body.as_bytes())),
        });
        fs::create_dir_all(dir.store().dir()).unwrap();
        fs::write(dir.store().dir().join(FILE), format!("{header}\n{body}")).unwrap();
        let list = reopen(&dir);
        assert_eq!(list.tasks.len(), 1);
        assert!(list.removed_apps.is_empty());
        assert_eq!(list.unreadable.len(), 2);
        // Written back unchanged on the next save, and still not used.
        dir.store().save(&list).unwrap();
        assert_eq!(reopen(&dir), list);
    }

    // ---- partial apply and recovery steps ----

    #[test]
    fn a_partial_apply_keeps_the_steps_it_needs_and_marks_the_failed_ones() {
        let dir = TempDir::new();
        let items = user_items();
        let failing = items
            .iter()
            .find(|item| item.settings.len() == 1)
            .expect("an item with one setting");
        let fine = items
            .iter()
            .find(|item| {
                item.id != failing.id
                    && item
                        .settings
                        .iter()
                        .all(|s| s.name != failing.settings[0].name)
            })
            .expect("a second item");
        let mut reg = FakeRegistry {
            broken: vec![failing.settings[0].name],
            ..FakeRegistry::default()
        };
        let run = plan::apply(catalog::items(), &[failing.id, fine.id], &mut reg, ENV);
        assert!(run
            .applied
            .iter()
            .any(|a| a.id == failing.id && matches!(a.outcome, Outcome::Failed(_))));
        assert!(run
            .applied
            .iter()
            .any(|a| a.id == fine.id && a.outcome == Outcome::Done));
        let steps = track_run(&run);
        assert!(steps.iter().any(|s| s.step.id == failing.id));
        assert!(steps
            .iter()
            .filter(|s| s.step.id == failing.id)
            .all(|s| s.after.is_none()));
        assert!(steps
            .iter()
            .filter(|s| s.step.id == fine.id)
            .all(|s| s.after.is_some()));

        let list = UndoList {
            registry_runs: vec![steps],
            ..UndoList::default()
        };
        dir.store().save(&list).unwrap();
        let back = reopen(&dir);
        assert_eq!(back, list);

        // The value can be written again: the recovery step is a retry and is not checked.
        reg.broken.clear();
        let tally = undo_steps(
            newest_first(&back.registry_runs[0]),
            |s| (s.step.hive, s.step.path, s.step.name),
            |s| undo_registry(s, &mut reg),
        );
        assert_eq!(
            (tally.done, tally.kept.len()),
            (back.registry_runs[0].len(), 0)
        );
        assert!(tally.refused.is_empty());
    }

    // ---- something else changed the item since ----

    #[test]
    fn undo_of_an_item_changed_since_is_refused_and_nothing_is_written() {
        let item = user_items()[0];
        let mut reg = FakeRegistry::default();
        let run = plan::apply(catalog::items(), &[item.id], &mut reg, ENV);
        let steps = track_run(&run);
        let s = &steps[0].step;
        reg.values
            .insert((s.hive, s.path.into(), s.name.into()), RegValue::Dword(77));
        match undo_registry(&steps[0], &mut reg) {
            StepResult::Refused(why) => assert!(why.contains(item.title), "{why}"),
            other => panic!("not refused: {other:?}"),
        }
        assert_eq!(
            reg.read(s.hive, s.path, s.name).unwrap(),
            Some(RegValue::Dword(77))
        );

        let def = service_def();
        let mut svc = FakeServices::default();
        svc.0.insert(def.name.into(), StartType::Auto);
        let step = service_done(services::set_off(&mut svc, def).unwrap());
        svc.0.insert(def.name.into(), StartType::AutoDelayed);
        assert!(matches!(
            undo_service(&step, &mut svc),
            StepResult::Refused(_)
        ));
        assert_eq!(svc.0[def.name], StartType::AutoDelayed);
        svc.0.clear();
        assert!(matches!(
            undo_service(&step, &mut svc),
            StepResult::Refused(_)
        ));

        let task = task_def();
        let mut list = FakeTasks::default();
        list.0.insert(task.path.into(), true);
        let step = task_done(tasks::set_enabled(&mut list, task, false).unwrap());
        list.0.insert(task.path.into(), true);
        assert!(matches!(
            undo_task(&step, &mut list),
            StepResult::Refused(_)
        ));

        let mut store = FakeStartup::with(&[(Source::UserRun, ENTRY_NAME)]);
        let step = startup_done(
            startup::set_enabled(&mut store, &entry(), false, 1).unwrap(),
            false,
        );
        let key = (Source::UserRun, ENTRY_NAME.to_string());
        store
            .0
            .insert(key.clone(), startup::enabled_bytes().to_vec());
        assert!(matches!(
            undo_startup(&step, &mut store),
            StepResult::Refused(_)
        ));
        assert_eq!(store.0[&key], startup::enabled_bytes().to_vec());
    }

    #[test]
    fn a_damaged_file_that_cannot_be_moved_aside_is_reported_as_not_kept() {
        let dir = TempDir::new();
        let file = dir.store().dir().join(FILE);
        fs::create_dir_all(dir.store().dir()).unwrap();
        fs::write(&file, b"damaged").unwrap();
        // The oldest slot is a folder that cannot be removed, so no copy can move up, slot 1
        // stays taken, and the file has to stay where it is. No copy is overwritten.
        fs::create_dir_all(dir.store().dir().join(damaged_name(DAMAGED_COPIES))).unwrap();
        fs::write(
            dir.store()
                .dir()
                .join(damaged_name(DAMAGED_COPIES))
                .join("x"),
            b"",
        )
        .unwrap();
        for slot in 1..DAMAGED_COPIES {
            fs::write(dir.store().dir().join(damaged_name(slot)), b"old").unwrap();
        }
        match dir.store().load() {
            Loaded::Damaged { kept, .. } => assert!(!kept),
            other => panic!("damaged file read as {other:?}"),
        }
        assert_eq!(fs::read(&file).unwrap(), b"damaged");
        for slot in 1..DAMAGED_COPIES {
            assert_eq!(
                fs::read(dir.store().dir().join(damaged_name(slot))).unwrap(),
                b"old"
            );
        }
    }

    #[test]
    fn a_stale_all_users_startup_step_is_dropped_and_never_blocks_the_others() {
        let real = startup_done(
            startup::UndoStep {
                source: Source::MachineRun,
                name: "Real App".into(),
                previous: None,
            },
            false,
        );
        let stale = startup_done(
            startup::UndoStep {
                source: Source::MachineRun,
                name: "Uninstalled App".into(),
                previous: None,
            },
            false,
        );
        let mut store = FakeStartup::with(&[(Source::MachineRun, "Real App")]);
        store.0.insert(
            (Source::MachineRun, "Real App".into()),
            startup::disabled_bytes(1).to_vec(),
        );

        // The page takes stale steps out before anything is sent ("Undo all": from 0).
        let mut steps = vec![real.clone(), stale.clone()];
        let exists = |s: &StartupStep| {
            store
                .1
                .iter()
                .any(|(source, name)| *source == s.step.source && *name == s.step.name)
        };
        let reasons = drop_gone_startup(&mut steps, 0, exists);
        assert_eq!(steps, vec![real.clone()]);
        assert_eq!(reasons, vec![startup_gone("Uninstalled App")]);

        // The undo itself refuses a gone entry too (the helper path), and undoes the real one.
        let tally = undo_steps(
            vec![stale, real],
            |s| (s.step.source, s.step.name.clone()),
            |s| undo_startup(s, &mut store),
        );
        assert_eq!((tally.done, tally.kept.len()), (1, 0));
        assert_eq!(tally.refused, vec![startup_gone("Uninstalled App")]);
        assert!(
            store.0.is_empty(),
            "the real entry went back to no approval value"
        );
        assert!(!store
            .0
            .contains_key(&(Source::MachineRun, "Uninstalled App".to_string())));
    }

    #[test]
    fn a_registry_item_windows_ignores_leaves_no_undo_step() {
        // NotEffective: written, read back different, put back. The engine keeps no step for it,
        // so there is nothing to mark; if it could not be put back the outcome is Failed and the
        // steps are recovery steps (no "left at" state, so the retry is not refused).
        let item = user_items()[0];
        let mut reg = FakeRegistry {
            ignored: item.settings.iter().map(|s| s.name).collect(),
            ..FakeRegistry::default()
        };
        let run = plan::apply(catalog::items(), &[item.id], &mut reg, ENV);
        assert_eq!(run.applied[0].outcome, Outcome::NotEffective);
        assert!(track_run(&run).is_empty());
    }

    #[test]
    fn a_task_switched_twice_is_undone_in_order_with_each_step_checked() {
        let task = task_def();
        let mut list = FakeTasks::default();
        list.0.insert(task.path.into(), true);
        let off = task_done(tasks::set_enabled(&mut list, task, false).unwrap());
        let on = task_done(tasks::set_enabled(&mut list, task, true).unwrap());
        let tally = undo_steps(vec![on, off], |s| s.step.path, |s| undo_task(s, &mut list));
        assert_eq!((tally.done, tally.kept.len()), (2, 0));
        assert!(list.0[task.path]);
    }

    // ---- deletion and where the file lives ----

    #[test]
    fn delete_removes_the_list_a_damaged_copy_and_the_folder() {
        let dir = TempDir::new();
        let list = UndoList {
            removed_apps: vec!["clipchamp"],
            ..UndoList::default()
        };
        dir.store().save(&list).unwrap();
        for slot in 1..=DAMAGED_COPIES {
            fs::write(dir.store().dir().join(damaged_name(slot)), b"old").unwrap();
        }
        dir.store().delete().unwrap();
        assert!(!dir.store().dir().exists());
        assert_eq!(dir.store().load(), Loaded::Nothing);
        // Deleting twice is fine.
        dir.store().delete().unwrap();
    }

    #[test]
    fn the_list_lives_in_the_add_ons_own_data_folder() {
        let root = Path::new(r"C:\Data\Toolboks Preview");
        let exe = root
            .join("addons")
            .join(crate::ID)
            .join("0.1.0")
            .join("debloater.exe");
        assert_eq!(
            directory_for(&exe, PathBuf::from(r"C:\Other")),
            root.join("addon-data").join(crate::ID).join("undo")
        );
        assert_eq!(
            directory_for(
                Path::new(r"D:\build\debloater.exe"),
                PathBuf::from(r"C:\Fallback")
            ),
            Path::new(r"C:\Fallback")
                .join("addon-data")
                .join("toolboks.debloater")
                .join("undo")
        );
    }

    #[test]
    fn removed_apps_are_forgotten_once_installed_again() {
        let mut list = vec!["clipchamp", "solitaire"];
        assert!(!record_removed(
            &mut list,
            &apps::AppRun {
                removed: vec![("clipchamp", "x".into())],
                failed: Vec::new(),
            }
        ));
        assert!(forget_reinstalled(&mut list, |id| id == "clipchamp"));
        assert_eq!(list, vec!["solitaire"]);
        assert!(!forget_reinstalled(&mut list, |_| false));
    }
}
