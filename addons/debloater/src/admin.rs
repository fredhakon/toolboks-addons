//! The administrator side of the Debloater: what the separate elevated helper validates, shows in
//! its own Confirm / Cancel window, and then runs. The drawing process never gets here with
//! rights; it only sends a request and shows the answer.
//!
//! Requests name catalog items by id. The preview text is built here from the curated catalog,
//! never from text that arrived in the request, and undo steps are accepted only for registry
//! locations the catalog itself knows.
use serde_json::{json, Value};
use toolboks_addon_sdk::{admin::AdminChange, AdminRequest};
use toolboks_debloat_engine::{
    backend::{self, WinRegistry},
    catalog,
    model::{Applicable, Env, Hive, Item, RegValue, Risk, State},
    plan::{self, Applied, Outcome, Run, UndoOp},
    restore::{self, RestoreStatus, WinRestore},
    services::{self, ServiceDef, StartType, WinServices},
    startup::{self, Class, Entry, Source, WinStartup},
    tasks::{self, TaskDef, WinTasks},
    ChangeError,
};

/// The one administrator operation name. The request's `kind` says which change.
pub const OPERATION: &str = "debloat";

/// Most items one request may name; the catalog is smaller than this.
const MAX_IDS: usize = 64;
/// Most undo steps one request may carry.
const MAX_STEPS: usize = 256;

// ---- the codec, shared by the page and the helper ---------------------------------------------

pub fn item_by_id(id: &str) -> Option<&'static Item> {
    catalog::items().iter().find(|item| item.id == id)
}

fn hive_name(hive: Hive) -> &'static str {
    match hive {
        Hive::CurrentUser => "user",
        Hive::LocalMachine => "machine",
    }
}

fn hive_from(name: &str) -> Option<Hive> {
    match name {
        "user" => Some(Hive::CurrentUser),
        "machine" => Some(Hive::LocalMachine),
        _ => None,
    }
}

fn value_json(value: &Option<RegValue>) -> Value {
    match value {
        None => Value::Null,
        Some(RegValue::Dword(n)) => json!({ "dword": n }),
        Some(RegValue::Sz(s)) => json!({ "sz": s }),
    }
}

fn value_from(value: &Value) -> Result<Option<RegValue>, String> {
    if value.is_null() {
        return Ok(None);
    }
    if let Some(n) = value["dword"].as_u64().and_then(|n| u32::try_from(n).ok()) {
        return Ok(Some(RegValue::Dword(n)));
    }
    if let Some(s) = value["sz"].as_str() {
        return Ok(Some(RegValue::Sz(s.to_owned())));
    }
    Err("An undo step has a value this add-on does not understand.".into())
}

/// One undo step as JSON.
pub fn undo_json(op: &UndoOp) -> Value {
    json!({
        "id": op.id,
        "hive": hive_name(op.hive),
        "path": op.path,
        "name": op.name,
        "previous": value_json(&op.previous),
    })
}

/// One undo step back from JSON. The step must name an item and one of that item's own registry
/// settings, so the static strings come from the catalog and nothing else can be written.
pub fn undo_from_json(value: &Value) -> Result<UndoOp, String> {
    let item = value["id"]
        .as_str()
        .and_then(item_by_id)
        .ok_or("An undo step names an item this add-on does not offer.")?;
    let hive = value["hive"]
        .as_str()
        .and_then(hive_from)
        .ok_or("An undo step has an unknown registry hive.")?;
    let (path, name) = (value["path"].as_str(), value["name"].as_str());
    let setting = item
        .settings
        .iter()
        .find(|s| s.hive == hive && Some(s.path) == path && Some(s.name) == name)
        .ok_or("An undo step names a registry value that is not part of that item.")?;
    Ok(UndoOp {
        id: item.id,
        hive,
        path: setting.path,
        name: setting.name,
        previous: value_from(&value["previous"])?,
    })
}

fn outcome_json(applied: &Applied) -> Value {
    let (outcome, detail) = match &applied.outcome {
        Outcome::Done => ("done", String::new()),
        Outcome::NotEffective => ("not_effective", String::new()),
        Outcome::Failed(reason) => ("failed", reason.clone()),
    };
    json!({ "id": applied.id, "outcome": outcome, "detail": detail })
}

/// The helper's answer for an apply, back into the engine's own result type.
pub fn run_from_json(value: &Value) -> Run {
    let mut run = Run::default();
    for entry in value["applied"].as_array().into_iter().flatten() {
        let Some(item) = entry["id"].as_str().and_then(item_by_id) else {
            continue;
        };
        let outcome = match entry["outcome"].as_str() {
            Some("done") => Outcome::Done,
            Some("not_effective") => Outcome::NotEffective,
            _ => Outcome::Failed(
                entry["detail"]
                    .as_str()
                    .unwrap_or("Windows refused")
                    .to_owned(),
            ),
        };
        run.applied.push(Applied {
            id: item.id,
            outcome,
        });
    }
    for step in value["undo"].as_array().into_iter().flatten() {
        if let Ok(op) = undo_from_json(step) {
            run.undo.push(op);
        }
    }
    run
}

pub fn steps_from_json(value: &Value) -> Vec<UndoOp> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|step| undo_from_json(step).ok())
        .collect()
}

/// Whether applying these items has to go through the helper: an item that writes the machine-wide
/// part of the registry, or a restore point, both need administrator rights.
pub fn needs_helper(ids: &[&str], restore_point: bool) -> bool {
    restore_point
        || ids
            .iter()
            .any(|id| item_by_id(id).is_some_and(Item::needs_admin))
}

pub fn registry_apply_request(ids: &[&str], restore_point: bool) -> String {
    json!({ "kind": "registry_apply", "ids": ids, "restore_point": restore_point }).to_string()
}

pub fn registry_undo_request(steps: &[UndoOp]) -> String {
    json!({
        "kind": "registry_undo",
        "steps": steps.iter().map(undo_json).collect::<Vec<_>>(),
    })
    .to_string()
}

// ---- the helper ---------------------------------------------------------------------------

/// Read-only check of the request. Nothing changes until the user confirms in the helper's window.
pub fn prepare_admin(request: &AdminRequest) -> Result<Box<dyn AdminChange>, String> {
    if request.operation != OPERATION {
        return Err("This add-on does not support that administrator action.".into());
    }
    let value: Value = serde_json::from_str(&request.json)
        .map_err(|_| "The administrator request was not understood.".to_string())?;
    match value["kind"].as_str() {
        Some("registry_apply") => prepare_registry_apply(&value),
        Some("registry_undo") => prepare_registry_undo(&value),
        Some("service_set") => prepare_service_set(&value),
        Some("service_undo") => prepare_service_undo(&value),
        Some("task_set") => prepare_task_set(&value),
        Some("task_undo") => prepare_task_undo(&value),
        Some("startup_set") => prepare_startup_set(&value),
        Some("startup_undo") => prepare_startup_undo(&value),
        _ => Err("The administrator request was not understood.".into()),
    }
}

struct RegistryApply {
    ids: Vec<&'static str>,
    restore_point: bool,
    description: String,
}

struct RegistryUndo {
    steps: Vec<UndoOp>,
    description: String,
}

fn prepare_registry_apply(value: &Value) -> Result<Box<dyn AdminChange>, String> {
    let list = value["ids"]
        .as_array()
        .filter(|list| !list.is_empty() && list.len() <= MAX_IDS)
        .ok_or("Choose at least one change.")?;
    let mut ids: Vec<&'static str> = Vec::new();
    for id in list {
        let item = id
            .as_str()
            .and_then(item_by_id)
            .ok_or("The request names a change this add-on does not offer.")?;
        if !ids.contains(&item.id) {
            ids.push(item.id);
        }
    }
    let restore_point = value["restore_point"].as_bool().unwrap_or(false);
    // The helper is elevated, so every item is judged as it will really be applied.
    let env = Env {
        elevated: true,
        ..backend::detect_env()
    };
    let statuses = plan::statuses(catalog::items(), &WinRegistry, env);
    let changing: Vec<&'static Item> = catalog::items()
        .iter()
        .filter(|item| ids.contains(&item.id))
        .filter(|item| {
            statuses.iter().any(|s| {
                s.id == item.id && s.state == State::Active && s.applicable == Applicable::Yes
            })
        })
        .collect();
    if changing.is_empty() {
        return Err(
            "Nothing to change: the chosen items are already off or not available on this edition."
                .into(),
        );
    }
    Ok(Box::new(RegistryApply {
        ids,
        restore_point,
        description: apply_description(&changing, restore_point),
    }))
}

fn prepare_registry_undo(value: &Value) -> Result<Box<dyn AdminChange>, String> {
    let list = value["steps"]
        .as_array()
        .filter(|list| !list.is_empty() && list.len() <= MAX_STEPS)
        .ok_or("There is nothing to undo.")?;
    let steps = list
        .iter()
        .map(undo_from_json)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Box::new(RegistryUndo {
        description: undo_description(&steps),
        steps,
    }))
}

/// Keeps the window text well inside the helper's size limit.
fn limited(mut text: String) -> String {
    const MAX: usize = 6000;
    if text.len() > MAX {
        let mut end = MAX;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n...");
    }
    text
}

fn apply_description(items: &[&Item], restore_point: bool) -> String {
    let mut text = String::from("Change these Windows settings?\n");
    for item in items {
        let risk = match item.risk {
            Risk::Safe => "Safe",
            Risk::Careful => "Careful",
        };
        let scope = if item.needs_admin() {
            "all users of this PC"
        } else {
            "your account"
        };
        text.push_str(&format!(
            "\n\u{2022} {} ({risk}, {scope})\n  {}\n  Could stop working: {}\n",
            item.title, item.what_changes, item.could_break
        ));
    }
    if restore_point {
        text.push_str(
            "\nWindows will create a restore point first. This can take about 20 seconds.\n",
        );
    }
    text.push_str(
        "\nItems that are already off, or not available on this edition, are skipped.\nYou can undo this run in Toolboks until you close it.",
    );
    limited(text)
}

fn describe_value(value: &Option<RegValue>) -> String {
    match value {
        None => "removed".into(),
        Some(RegValue::Dword(n)) => format!("{n} (0x{n:x})"),
        Some(RegValue::Sz(s)) => format!("\"{s}\""),
    }
}

fn undo_description(steps: &[UndoOp]) -> String {
    let mut text = String::from("Put these Windows settings back?\n");
    for op in steps {
        let title = item_by_id(op.id).map_or(op.id, |item| item.title);
        let hive = match op.hive {
            Hive::CurrentUser => "HKCU",
            Hive::LocalMachine => "HKLM",
        };
        text.push_str(&format!(
            "\n\u{2022} {title}\n  {hive}\\{} \u{b7} {} \u{2192} {}\n",
            op.path,
            op.name,
            describe_value(&op.previous)
        ));
    }
    limited(text)
}

impl AdminChange for RegistryApply {
    fn capability(&self) -> &str {
        "system_tuning"
    }
    fn description(&self) -> String {
        self.description.clone()
    }
    fn apply(self: Box<Self>) -> Result<String, String> {
        let env = backend::detect_env();
        let mut restore_text = Value::Null;
        if self.restore_point {
            let status = restore::create(
                &mut WinRestore,
                "toolboks: privacy and ads changes",
                env.elevated,
            );
            restore_text = Value::String(restore::status_text(&status));
            if restore::is_blocking(&status) {
                return Ok(json!({
                    "restore": restore_text,
                    "blocked": true,
                    "applied": [],
                    "undo": [],
                })
                .to_string());
            }
        }
        let run = plan::apply(catalog::items(), &self.ids, &mut WinRegistry, env);
        Ok(json!({
            "restore": restore_text,
            "blocked": false,
            "applied": run.applied.iter().map(outcome_json).collect::<Vec<_>>(),
            "undo": run.undo.iter().map(undo_json).collect::<Vec<_>>(),
        })
        .to_string())
    }
}

impl AdminChange for RegistryUndo {
    fn capability(&self) -> &str {
        "system_tuning"
    }
    fn description(&self) -> String {
        self.description.clone()
    }
    fn apply(self: Box<Self>) -> Result<String, String> {
        let total = self.steps.len();
        let mut steps = self.steps;
        plan::undo(&mut steps, &mut WinRegistry);
        Ok(json!({
            "total": total,
            "remaining": steps.iter().map(undo_json).collect::<Vec<_>>(),
        })
        .to_string())
    }
}

// ---- restore points, services and tasks: the codec ------------------------------------------

/// A restore point result as JSON.
pub fn restore_json(status: &RestoreStatus) -> Value {
    match status {
        RestoreStatus::Created { sequence } => json!({ "status": "created", "sequence": sequence }),
        RestoreStatus::SkippedRecent { sequence } => {
            json!({ "status": "skipped", "sequence": sequence })
        }
        RestoreStatus::ProtectionOff => json!({ "status": "protection_off" }),
        RestoreStatus::NeedsAdmin => json!({ "status": "needs_admin" }),
        RestoreStatus::Failed(error) => json!({ "status": "failed", "error": error }),
    }
}

pub fn restore_from_json(value: &Value) -> Option<RestoreStatus> {
    let sequence = value["sequence"].as_u64().unwrap_or(0);
    match value["status"].as_str()? {
        "created" => Some(RestoreStatus::Created { sequence }),
        "skipped" => Some(RestoreStatus::SkippedRecent { sequence }),
        "protection_off" => Some(RestoreStatus::ProtectionOff),
        "needs_admin" => Some(RestoreStatus::NeedsAdmin),
        "failed" => Some(RestoreStatus::Failed(
            value["error"]
                .as_str()
                .unwrap_or("Windows refused")
                .to_owned(),
        )),
        _ => None,
    }
}

fn start_name(start: StartType) -> Option<&'static str> {
    match start {
        StartType::Auto => Some("auto"),
        StartType::AutoDelayed => Some("auto_delayed"),
        StartType::Manual => Some("manual"),
        StartType::Disabled => Some("disabled"),
        StartType::Other(_) => None,
    }
}

fn start_from(name: &str) -> Option<StartType> {
    match name {
        "auto" => Some(StartType::Auto),
        "auto_delayed" => Some(StartType::AutoDelayed),
        "manual" => Some(StartType::Manual),
        "disabled" => Some(StartType::Disabled),
        _ => None,
    }
}

pub fn service_by_id(id: &str) -> Option<&'static ServiceDef> {
    services::catalog().iter().find(|def| def.id == id)
}

pub fn task_by_id(id: &str) -> Option<&'static TaskDef> {
    tasks::catalog().iter().find(|def| def.id == id)
}

pub fn service_step_json(step: &services::UndoStep) -> Value {
    json!({ "id": step.id, "previous": start_name(step.previous) })
}

/// A service undo step back from JSON. The name comes from the catalog, never from the request.
pub fn service_step_from(value: &Value) -> Result<services::UndoStep, String> {
    let def = value["id"]
        .as_str()
        .and_then(service_by_id)
        .filter(|def| !services::is_never_offered(def.name))
        .ok_or("An undo step names a service this add-on does not offer.")?;
    let previous = value["previous"]
        .as_str()
        .and_then(start_from)
        .ok_or("An undo step has a start type this add-on does not write.")?;
    Ok(services::UndoStep {
        id: def.id,
        name: def.name,
        previous,
    })
}

pub fn task_step_json(step: &tasks::UndoStep) -> Value {
    json!({ "id": step.id, "was_enabled": step.was_enabled })
}

/// A task undo step back from JSON. The path comes from the catalog, never from the request.
pub fn task_step_from(value: &Value) -> Result<tasks::UndoStep, String> {
    let def = value["id"]
        .as_str()
        .and_then(task_by_id)
        .filter(|def| !tasks::is_never_offered(def.path))
        .ok_or("An undo step names a task this add-on does not offer.")?;
    let was_enabled = value["was_enabled"]
        .as_bool()
        .ok_or("An undo step does not say whether the task was on.")?;
    Ok(tasks::UndoStep {
        id: def.id,
        path: def.path,
        was_enabled,
    })
}

/// The outcome of one change, as the page needs it to rebuild the engine's own result.
fn change_json<T>(
    result: &Result<T, ChangeError<T>>,
    restore: Option<&RestoreStatus>,
    step: impl Fn(&T) -> Value,
) -> Value {
    let restore = restore.map(restore_json).unwrap_or(Value::Null);
    match result {
        Ok(done) => json!({ "ok": true, "step": step(done), "restore": restore }),
        Err(error) => json!({
            "ok": false,
            "message": error.message,
            "recovery": error.recovery.as_ref().map(&step),
            "restore": restore,
        }),
    }
}

/// The helper's answer back into `Result<UndoStep, ChangeError<UndoStep>>` plus the restore point.
#[allow(clippy::type_complexity)]
pub fn change_from_json<T>(
    value: &Value,
    step: impl Fn(&Value) -> Result<T, String>,
) -> (Result<T, ChangeError<T>>, Option<RestoreStatus>) {
    let restore = restore_from_json(&value["restore"]);
    if value["ok"].as_bool() == Some(true) {
        return match step(&value["step"]) {
            Ok(done) => (Ok(done), restore),
            Err(message) => (Err(message.into()), restore),
        };
    }
    let recovery = value["recovery"]
        .is_object()
        .then(|| step(&value["recovery"]).ok())
        .flatten();
    let message = value["message"]
        .as_str()
        .unwrap_or("The change stopped unexpectedly.")
        .to_owned();
    (Err(ChangeError { message, recovery }), restore)
}

pub fn service_set_request(id: &str, restore_point: bool) -> String {
    json!({ "kind": "service_set", "id": id, "restore_point": restore_point }).to_string()
}

pub fn service_undo_request(steps: &[services::UndoStep]) -> String {
    json!({
        "kind": "service_undo",
        "steps": steps.iter().map(service_step_json).collect::<Vec<_>>(),
    })
    .to_string()
}

pub fn task_set_request(id: &str, enable: bool, restore_point: bool) -> String {
    json!({ "kind": "task_set", "id": id, "enable": enable, "restore_point": restore_point })
        .to_string()
}

pub fn task_undo_request(steps: &[tasks::UndoStep]) -> String {
    json!({
        "kind": "task_undo",
        "steps": steps.iter().map(task_step_json).collect::<Vec<_>>(),
    })
    .to_string()
}

/// How an undo batch ended: how many steps worked, and the ones to keep for another try.
pub fn undo_batch_from_json<T>(
    value: &Value,
    step: impl Fn(&Value) -> Result<T, String>,
) -> (usize, Vec<T>) {
    let kept: Vec<T> = value["kept"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| step(entry).ok())
        .collect();
    (value["done"].as_u64().unwrap_or(0) as usize, kept)
}

/// Undoes steps in the order given (newest first), skipping any step whose key already failed so
/// an older value is never put back over a newer one that is still in place.
fn undo_batch<T, K: PartialEq>(
    steps: Vec<T>,
    key: impl Fn(&T) -> K,
    mut undo_one: impl FnMut(&T) -> Result<(), String>,
) -> (usize, Vec<T>) {
    let (mut done, mut kept, mut blocked): (usize, Vec<T>, Vec<K>) = (0, Vec::new(), Vec::new());
    for step in steps {
        let step_key = key(&step);
        if !blocked.contains(&step_key) && undo_one(&step).is_ok() {
            done += 1;
        } else {
            blocked.push(step_key);
            kept.push(step);
        }
    }
    (done, kept)
}

// ---- services and tasks: the helper ----------------------------------------------------------

struct ServiceSet {
    def: &'static ServiceDef,
    restore_point: bool,
    description: String,
}

struct ServiceUndo {
    steps: Vec<services::UndoStep>,
    description: String,
}

struct TaskSet {
    def: &'static TaskDef,
    enable: bool,
    restore_point: bool,
    description: String,
}

struct TaskUndo {
    steps: Vec<tasks::UndoStep>,
    description: String,
}

fn risk_word(risk: Risk) -> &'static str {
    match risk {
        Risk::Safe => "Safe",
        Risk::Careful => "Careful",
    }
}

fn restore_line(restore_point: bool) -> &'static str {
    if restore_point {
        "\nWindows will create a restore point first. This can take about 20 seconds.\n"
    } else {
        ""
    }
}

fn service_set_description(def: &ServiceDef, restore_point: bool) -> String {
    limited(format!(
        "Switch this Windows service off?\n\n\u{2022} {} ({}, all users of this PC)\n  {}\n  Could stop working: {}\n  Its start type becomes: {}\n{}\nWindows checks the service again before changing it.\nYou can undo this in Toolboks until you close it.",
        def.title,
        risk_word(def.risk),
        def.what_changes,
        def.could_break,
        def.target.label(),
        restore_line(restore_point)
    ))
}

fn task_set_description(def: &TaskDef, enable: bool, restore_point: bool) -> String {
    limited(format!(
        "Switch this scheduled task {}?\n\n\u{2022} {} ({}, all users of this PC)\n  {}\n  Could stop working: {}\n  Task: {}\n{}\nWindows checks the task again before changing it.\nYou can undo this in Toolboks until you close it.",
        if enable { "on" } else { "off" },
        def.title,
        risk_word(def.risk),
        def.what_changes,
        def.could_break,
        def.path,
        restore_line(restore_point)
    ))
}

fn service_undo_description(steps: &[services::UndoStep]) -> String {
    let mut text = String::from("Put these Windows services back?\n");
    for step in steps {
        let title = service_by_id(step.id).map_or(step.id, |def| def.title);
        text.push_str(&format!(
            "\n\u{2022} {title}\n  Start type \u{2192} {}\n",
            step.previous.label()
        ));
    }
    limited(text)
}

fn task_undo_description(steps: &[tasks::UndoStep]) -> String {
    let mut text = String::from("Put these scheduled tasks back?\n");
    for step in steps {
        let title = task_by_id(step.id).map_or(step.id, |def| def.title);
        text.push_str(&format!(
            "\n\u{2022} {title}\n  \u{2192} {}\n",
            if step.was_enabled { "on" } else { "off" }
        ));
    }
    limited(text)
}

fn prepare_service_set(value: &Value) -> Result<Box<dyn AdminChange>, String> {
    let def = value["id"]
        .as_str()
        .and_then(service_by_id)
        .filter(|def| !services::is_never_offered(def.name))
        .ok_or("The request names a service this add-on does not offer.")?;
    let restore_point = value["restore_point"].as_bool().unwrap_or(false);
    Ok(Box::new(ServiceSet {
        def,
        restore_point,
        description: service_set_description(def, restore_point),
    }))
}

fn prepare_task_set(value: &Value) -> Result<Box<dyn AdminChange>, String> {
    let def = value["id"]
        .as_str()
        .and_then(task_by_id)
        .filter(|def| !tasks::is_never_offered(def.path))
        .ok_or("The request names a task this add-on does not offer.")?;
    let enable = value["enable"]
        .as_bool()
        .ok_or("The request does not say whether to switch the task on or off.")?;
    let restore_point = value["restore_point"].as_bool().unwrap_or(false);
    Ok(Box::new(TaskSet {
        def,
        enable,
        restore_point,
        description: task_set_description(def, enable, restore_point),
    }))
}

fn steps_list(value: &Value) -> Result<&Vec<Value>, String> {
    value["steps"]
        .as_array()
        .filter(|list| !list.is_empty() && list.len() <= MAX_STEPS)
        .ok_or_else(|| "There is nothing to undo.".to_string())
}

fn prepare_service_undo(value: &Value) -> Result<Box<dyn AdminChange>, String> {
    let steps = steps_list(value)?
        .iter()
        .map(service_step_from)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Box::new(ServiceUndo {
        description: service_undo_description(&steps),
        steps,
    }))
}

fn prepare_task_undo(value: &Value) -> Result<Box<dyn AdminChange>, String> {
    let steps = steps_list(value)?
        .iter()
        .map(task_step_from)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Box::new(TaskUndo {
        description: task_undo_description(&steps),
        steps,
    }))
}

/// Makes the restore point when asked. `Some(status)` is a restore point result for the page.
fn maybe_restore_point(restore_point: bool, what: &str) -> Option<RestoreStatus> {
    restore_point.then(|| {
        restore::create(
            &mut WinRestore,
            &format!("toolboks: {what}"),
            backend::detect_env().elevated,
        )
    })
}

fn blocked_by_restore(status: &Option<RestoreStatus>) -> Option<String> {
    status
        .as_ref()
        .filter(|status| restore::is_blocking(status))
        .map(restore::status_text)
}

impl AdminChange for ServiceSet {
    fn capability(&self) -> &str {
        "system_tuning"
    }
    fn description(&self) -> String {
        self.description.clone()
    }
    fn apply(self: Box<Self>) -> Result<String, String> {
        let point = maybe_restore_point(
            self.restore_point,
            &format!("switch off service {}", self.def.title),
        );
        let result: Result<services::UndoStep, ChangeError<services::UndoStep>> =
            match blocked_by_restore(&point) {
                Some(text) => Err(text.into()),
                None => services::set_off(&mut WinServices, self.def),
            };
        Ok(change_json(&result, point.as_ref(), service_step_json).to_string())
    }
}

impl AdminChange for ServiceUndo {
    fn capability(&self) -> &str {
        "system_tuning"
    }
    fn description(&self) -> String {
        self.description.clone()
    }
    fn apply(self: Box<Self>) -> Result<String, String> {
        let (done, kept) = undo_batch(
            self.steps,
            |step| step.name,
            |step| services::undo(&mut WinServices, step),
        );
        Ok(json!({
            "done": done,
            "kept": kept.iter().map(service_step_json).collect::<Vec<_>>(),
        })
        .to_string())
    }
}

impl AdminChange for TaskSet {
    fn capability(&self) -> &str {
        "system_tuning"
    }
    fn description(&self) -> String {
        self.description.clone()
    }
    fn apply(self: Box<Self>) -> Result<String, String> {
        let (def, enable, restore_point) = (self.def, self.enable, self.restore_point);
        // Task Scheduler uses COM, which must not be initialised on the window's thread.
        let worker = std::thread::spawn(move || {
            let point = maybe_restore_point(restore_point, &format!("switch task {}", def.title));
            let result = match blocked_by_restore(&point) {
                Some(text) => Err(text.into()),
                None => tasks::set_enabled(&mut WinTasks, def, enable),
            };
            change_json(&result, point.as_ref(), task_step_json).to_string()
        });
        worker
            .join()
            .map_err(|_| "The task change stopped unexpectedly.".to_string())
    }
}

impl AdminChange for TaskUndo {
    fn capability(&self) -> &str {
        "system_tuning"
    }
    fn description(&self) -> String {
        self.description.clone()
    }
    fn apply(self: Box<Self>) -> Result<String, String> {
        let steps = self.steps;
        let worker = std::thread::spawn(move || {
            let (done, kept) = undo_batch(
                steps,
                |step| step.path,
                |step| tasks::undo(&mut WinTasks, step),
            );
            json!({
                "done": done,
                "kept": kept.iter().map(task_step_json).collect::<Vec<_>>(),
            })
            .to_string()
        });
        worker
            .join()
            .map_err(|_| "The task undo stopped unexpectedly.".to_string())
    }
}

// ---- startup entries -----------------------------------------------------------------------

fn source_name(source: Source) -> &'static str {
    match source {
        Source::UserRun => "user_run",
        Source::MachineRun => "machine_run",
        Source::MachineRun32 => "machine_run32",
        Source::UserFolder => "user_folder",
        Source::CommonFolder => "common_folder",
    }
}

fn source_from(name: &str) -> Option<Source> {
    Source::ALL
        .into_iter()
        .find(|source| source_name(*source) == name)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).ok())
        .collect()
}

/// An entry name as it may appear in a request: non-empty, short, no control characters.
fn startup_name(value: &Value) -> Option<&str> {
    value
        .as_str()
        .filter(|name| !name.is_empty() && name.len() <= 260 && !name.chars().any(char::is_control))
}

pub fn startup_step_json(step: &startup::UndoStep) -> Value {
    json!({
        "source": source_name(step.source),
        "name": step.name,
        "previous": step.previous.as_deref().map(hex),
    })
}

/// A startup undo step back from JSON. The approval value is exactly 12 bytes or absent.
pub fn startup_step_from(value: &Value) -> Result<startup::UndoStep, String> {
    let source = value["source"]
        .as_str()
        .and_then(source_from)
        .ok_or("An undo step names an unknown startup location.")?;
    let name = startup_name(&value["name"]).ok_or("An undo step has an invalid entry name.")?;
    let previous = match &value["previous"] {
        Value::Null => None,
        Value::String(text) => Some(
            unhex(text)
                .filter(|bytes| bytes.len() == 12)
                .ok_or("An undo step has an invalid startup value.")?,
        ),
        _ => return Err("An undo step has an invalid startup value.".into()),
    };
    Ok(startup::UndoStep {
        source,
        name: name.to_owned(),
        previous,
    })
}

pub fn startup_set_request(entry: &Entry, enable: bool) -> String {
    json!({
        "kind": "startup_set",
        "source": source_name(entry.source),
        "name": entry.name,
        "enable": enable,
    })
    .to_string()
}

pub fn startup_undo_request(steps: &[startup::UndoStep]) -> String {
    json!({
        "kind": "startup_undo",
        "steps": steps.iter().map(startup_step_json).collect::<Vec<_>>(),
    })
    .to_string()
}

/// The entries as they really are right now. A request is checked against this list, so it can
/// only name an entry that exists, never write an arbitrary startup value.
fn current_entries() -> Result<Vec<Entry>, String> {
    startup::entries(&WinStartup, &startup::publisher_of_command)
}

struct StartupSet {
    entry: Entry,
    enable: bool,
    description: String,
}

struct StartupUndo {
    steps: Vec<startup::UndoStep>,
    description: String,
}

fn startup_set_description(entry: &Entry, enable: bool) -> String {
    let care = match entry.class {
        Class::Careful => {
            "\n  This may be hardware software, a VPN or something unknown. Switching it off can stop that from working."
        }
        _ => "",
    };
    limited(format!(
        "Switch this startup item {}?\n\n\u{2022} {} ({})\n  Publisher: {}\n  Runs: {}{care}\n\nThe entry stays where it is; only whether it starts with Windows changes.\nYou can undo this in Toolboks until you close it.",
        if enable { "on" } else { "off" },
        entry.name,
        entry.source.label(),
        entry.publisher.as_deref().unwrap_or("unknown"),
        entry.command,
    ))
}

fn startup_undo_description(steps: &[startup::UndoStep]) -> String {
    let mut text = String::from("Put these startup items back?\n");
    for step in steps {
        text.push_str(&format!(
            "\n\u{2022} {} ({})\n  Its start-up setting goes back to what it was.\n",
            step.name,
            step.source.label()
        ));
    }
    limited(text)
}

fn prepare_startup_set(value: &Value) -> Result<Box<dyn AdminChange>, String> {
    let source = value["source"]
        .as_str()
        .and_then(source_from)
        .ok_or("The request names an unknown startup location.")?;
    let name = startup_name(&value["name"]).ok_or("The request has an invalid entry name.")?;
    let enable = value["enable"]
        .as_bool()
        .ok_or("The request does not say whether to switch the item on or off.")?;
    let entry = current_entries()?
        .into_iter()
        .find(|entry| entry.source == source && entry.name == name)
        .ok_or("That startup item is no longer there.")?;
    if entry.class == Class::Never {
        return Err(format!(
            "{} is a security or system component and cannot be changed here.",
            entry.name
        ));
    }
    Ok(Box::new(StartupSet {
        description: startup_set_description(&entry, enable),
        entry,
        enable,
    }))
}

fn prepare_startup_undo(value: &Value) -> Result<Box<dyn AdminChange>, String> {
    let steps = steps_list(value)?
        .iter()
        .map(startup_step_from)
        .collect::<Result<Vec<_>, _>>()?;
    let entries = current_entries()?;
    if let Some(step) = steps.iter().find(|step| {
        !entries
            .iter()
            .any(|entry| entry.source == step.source && entry.name == step.name)
    }) {
        return Err(format!("{} is no longer a startup item.", step.name));
    }
    Ok(Box::new(StartupUndo {
        description: startup_undo_description(&steps),
        steps,
    }))
}

/// Windows FILETIME now: 100 ns steps since 1601.
fn filetime_now() -> u64 {
    const UNIX_EPOCH_AS_FILETIME: u64 = 116_444_736_000_000_000;
    let since_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    UNIX_EPOCH_AS_FILETIME + (since_unix.as_nanos() / 100) as u64
}

impl AdminChange for StartupSet {
    fn capability(&self) -> &str {
        "system_tuning"
    }
    fn description(&self) -> String {
        self.description.clone()
    }
    fn apply(self: Box<Self>) -> Result<String, String> {
        let result =
            startup::set_enabled(&mut WinStartup, &self.entry, self.enable, filetime_now());
        Ok(change_json(&result, None, startup_step_json).to_string())
    }
}

impl AdminChange for StartupUndo {
    fn capability(&self) -> &str {
        "system_tuning"
    }
    fn description(&self) -> String {
        self.description.clone()
    }
    fn apply(self: Box<Self>) -> Result<String, String> {
        let (done, kept) = undo_batch(
            self.steps,
            |step| (step.source, step.name.clone()),
            |step| startup::undo(&mut WinStartup, step),
        );
        Ok(json!({
            "done": done,
            "kept": kept.iter().map(startup_step_json).collect::<Vec<_>>(),
        })
        .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(json: &str) -> AdminRequest {
        AdminRequest {
            id: 1,
            operation: OPERATION.into(),
            json: json.into(),
        }
    }

    fn machine_item() -> &'static Item {
        catalog::items()
            .iter()
            .find(|item| item.needs_admin())
            .expect("the catalog has a machine-wide item")
    }

    fn step_for(item: &'static Item) -> UndoOp {
        let setting = &item.settings[0];
        UndoOp {
            id: item.id,
            hive: setting.hive,
            path: setting.path,
            name: setting.name,
            previous: Some(RegValue::Dword(1)),
        }
    }

    #[test]
    fn undo_steps_round_trip_through_json() {
        for previous in [
            None,
            Some(RegValue::Dword(7)),
            Some(RegValue::Sz("x".into())),
        ] {
            let mut op = step_for(machine_item());
            op.previous = previous;
            assert_eq!(undo_from_json(&undo_json(&op)).unwrap(), op);
        }
    }

    #[test]
    fn undo_steps_outside_the_catalog_are_refused() {
        let good = undo_json(&step_for(machine_item()));
        for (field, value) in [
            ("id", json!("not-an-item")),
            ("path", json!("Software\\Evil")),
            ("name", json!("Anything")),
            ("hive", json!("sideways")),
        ] {
            let mut bad = good.clone();
            bad[field] = value;
            assert!(undo_from_json(&bad).is_err(), "{field} was accepted");
        }
        let mut bad_value = good.clone();
        bad_value["previous"] = json!({ "binary": "00" });
        assert!(undo_from_json(&bad_value).is_err());
    }

    #[test]
    fn a_machine_item_or_a_restore_point_needs_the_helper() {
        let machine = machine_item().id;
        assert!(needs_helper(&[machine], false));
        assert!(needs_helper(&[], true));
        assert!(!needs_helper(&["not-an-item"], false));
        let user_only = catalog::items().iter().find(|item| !item.needs_admin());
        if let Some(item) = user_only {
            assert!(!needs_helper(&[item.id], false));
        }
    }

    #[test]
    fn bad_requests_are_refused() {
        let bad_operation = AdminRequest {
            id: 1,
            operation: "format_disk".into(),
            json: "{}".into(),
        };
        assert!(prepare_admin(&bad_operation).is_err());
        assert!(prepare_admin(&request("not json")).is_err());
        assert!(prepare_admin(&request(r#"{"kind":"launch"}"#)).is_err());
        assert!(prepare_admin(&request(r#"{"kind":"registry_apply","ids":[]}"#)).is_err());
        assert!(prepare_admin(&request(r#"{"kind":"registry_apply","ids":["nope"]}"#)).is_err());
        assert!(prepare_admin(&request(r#"{"kind":"registry_undo","steps":[]}"#)).is_err());
    }

    #[test]
    fn the_apply_preview_comes_from_the_catalog() {
        let item = machine_item();
        let text = apply_description(&[item], true);
        assert!(text.contains(item.title));
        assert!(text.contains(item.what_changes) && text.contains(item.could_break));
        assert!(text.contains("all users of this PC"));
        assert!(text.contains("restore point"));
        assert!(!apply_description(&[item], false).contains("restore point"));
        assert!(text.len() < 8000);
    }

    #[test]
    fn the_undo_preview_names_each_value() {
        let op = step_for(machine_item());
        let text = undo_description(std::slice::from_ref(&op));
        assert!(text.contains(op.name) && text.contains("HKLM") && text.contains("1 (0x1)"));
        let mut removed = op;
        removed.previous = None;
        assert!(undo_description(&[removed]).contains("removed"));
    }

    #[test]
    fn a_registry_request_for_a_real_item_is_understood() {
        let item = machine_item();
        let json = registry_apply_request(&[item.id], false);
        match prepare_admin(&request(&json)) {
            Ok(change) => {
                assert_eq!(change.capability(), "system_tuning");
                assert!(change.description().contains(item.title));
            }
            // The item may already be off on this PC.
            Err(error) => assert!(error.contains("Nothing to change"), "{error}"),
        }
        let undo = registry_undo_request(&[step_for(item)]);
        assert!(prepare_admin(&request(&undo)).is_ok());
    }

    #[test]
    fn the_helper_answer_becomes_the_engines_result_again() {
        let item = machine_item();
        let op = step_for(item);
        let answer = json!({
            "applied": [
                { "id": item.id, "outcome": "done", "detail": "" },
                { "id": "not-an-item", "outcome": "done", "detail": "" },
            ],
            "undo": [undo_json(&op)],
        });
        let run = run_from_json(&answer);
        assert_eq!(run.applied.len(), 1);
        assert_eq!(run.applied[0].outcome, Outcome::Done);
        assert_eq!(run.undo, vec![op]);
    }

    #[test]
    fn restore_results_round_trip() {
        for status in [
            RestoreStatus::Created { sequence: 3 },
            RestoreStatus::SkippedRecent { sequence: 4 },
            RestoreStatus::ProtectionOff,
            RestoreStatus::NeedsAdmin,
            RestoreStatus::Failed("no".into()),
        ] {
            assert_eq!(restore_from_json(&restore_json(&status)), Some(status));
        }
        assert_eq!(restore_from_json(&Value::Null), None);
    }

    #[test]
    fn service_and_task_steps_come_back_from_the_catalog() {
        let def = services::catalog()
            .iter()
            .find(|def| !services::is_never_offered(def.name))
            .unwrap();
        let step = services::UndoStep {
            id: def.id,
            name: def.name,
            previous: StartType::AutoDelayed,
        };
        assert_eq!(service_step_from(&service_step_json(&step)).unwrap(), step);
        let mut other = service_step_json(&step);
        other["id"] = json!("nope");
        assert!(service_step_from(&other).is_err());
        let mut odd = service_step_json(&step);
        odd["previous"] = json!("boot");
        assert!(service_step_from(&odd).is_err());

        let def = tasks::catalog()
            .iter()
            .find(|def| !tasks::is_never_offered(def.path))
            .unwrap();
        let step = tasks::UndoStep {
            id: def.id,
            path: def.path,
            was_enabled: true,
        };
        assert_eq!(task_step_from(&task_step_json(&step)).unwrap(), step);
        let mut other = task_step_json(&step);
        other["id"] = json!("nope");
        assert!(task_step_from(&other).is_err());
    }

    #[test]
    fn a_change_result_survives_the_trip_with_its_recovery_step() {
        let def = tasks::catalog().first().unwrap();
        let step = tasks::UndoStep {
            id: def.id,
            path: def.path,
            was_enabled: false,
        };
        let ok: Result<tasks::UndoStep, ChangeError<tasks::UndoStep>> = Ok(step.clone());
        let json = change_json(&ok, Some(&RestoreStatus::ProtectionOff), task_step_json);
        let (back, point) = change_from_json(&json, task_step_from);
        assert_eq!(back.unwrap(), step);
        assert_eq!(point, Some(RestoreStatus::ProtectionOff));

        let failed: Result<tasks::UndoStep, ChangeError<tasks::UndoStep>> = Err(ChangeError {
            message: "Windows refused".into(),
            recovery: Some(step.clone()),
        });
        let json = change_json(&failed, None, task_step_json);
        let (back, point) = change_from_json(&json, task_step_from);
        let error = back.unwrap_err();
        assert_eq!(error.message, "Windows refused");
        assert_eq!(error.recovery, Some(step));
        assert_eq!(point, None);
    }

    #[test]
    fn an_undo_batch_never_puts_an_older_value_over_a_failed_newer_one() {
        // Newest first: b2 fails, so the older b1 for the same key must be kept, not applied.
        let steps = vec![("a", 1), ("b", 2), ("a", 3)];
        let (done, kept) = undo_batch(
            steps,
            |step| step.0,
            |step| {
                if step.1 == 1 {
                    Err("no".into())
                } else {
                    Ok(())
                }
            },
        );
        assert_eq!(done, 1);
        assert_eq!(kept, vec![("a", 1), ("a", 3)]);
        let (done_json, kept_json) = (
            json!({ "done": done, "kept": [] }),
            undo_batch_from_json(&json!({ "done": 1, "kept": [] }), |_| Ok(1)),
        );
        assert_eq!(done_json["done"], 1);
        assert_eq!(kept_json.0, 1);
    }

    #[test]
    fn service_and_task_requests_are_validated_and_previewed_from_the_catalog() {
        let service = services::catalog()
            .iter()
            .find(|def| !services::is_never_offered(def.name))
            .unwrap();
        let change = prepare_admin(&request(&service_set_request(service.id, true))).unwrap();
        let text = change.description();
        assert!(text.contains(service.title) && text.contains(service.could_break));
        assert!(text.contains("restore point") && text.len() < 8000);
        assert!(prepare_admin(&request(&service_set_request("nope", false))).is_err());

        let task = tasks::catalog()
            .iter()
            .find(|def| !tasks::is_never_offered(def.path))
            .unwrap();
        let change = prepare_admin(&request(&task_set_request(task.id, false, false))).unwrap();
        assert!(change.description().contains(task.path));
        assert!(change.description().contains("off"));
        assert!(prepare_admin(&request(&task_set_request("nope", true, false))).is_err());
        assert!(prepare_admin(&request(r#"{"kind":"task_set","id":"x"}"#)).is_err());

        let step = services::UndoStep {
            id: service.id,
            name: service.name,
            previous: StartType::Manual,
        };
        let undo = prepare_admin(&request(&service_undo_request(&[step]))).unwrap();
        assert!(undo.description().contains("Manual"));
        assert!(prepare_admin(&request(r#"{"kind":"task_undo","steps":[]}"#)).is_err());
    }

    #[test]
    fn startup_steps_round_trip_and_bad_ones_are_refused() {
        for previous in [None, Some(vec![2, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8])] {
            let step = startup::UndoStep {
                source: Source::MachineRun,
                name: "Some App".into(),
                previous,
            };
            assert_eq!(startup_step_from(&startup_step_json(&step)).unwrap(), step);
        }
        let good = json!({ "source": "machine_run", "name": "x", "previous": null });
        assert!(startup_step_from(&good).is_ok());
        for bad in [
            json!({ "source": "sideways", "name": "x", "previous": null }),
            json!({ "source": "machine_run", "name": "", "previous": null }),
            json!({ "source": "machine_run", "name": "a\nb", "previous": null }),
            json!({ "source": "machine_run", "name": "x", "previous": "00ff" }),
            json!({ "source": "machine_run", "name": "x", "previous": 5 }),
            json!({ "source": "machine_run", "name": "x", "previous": "zz0000000000000000000000" }),
        ] {
            assert!(startup_step_from(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_startup_preview_comes_from_the_real_entry() {
        let entry = Entry {
            source: Source::MachineRun,
            name: "Updater".into(),
            command: "C:\\Apps\\updater.exe --silent".into(),
            enabled: true,
            publisher: Some("Example Corp".into()),
            class: Class::Careful,
        };
        let text = startup_set_description(&entry, false);
        assert!(text.contains("off") && text.contains("Updater"));
        assert!(text.contains("Example Corp") && text.contains("updater.exe --silent"));
        assert!(text.contains("All users") && text.contains("VPN"));
        assert!(startup_set_description(&entry, true).contains("on?"));
    }

    #[test]
    fn startup_requests_for_things_that_are_not_there_are_refused() {
        assert!(prepare_admin(&request(
            r#"{"kind":"startup_set","source":"machine_run","name":"No Such Entry Exists 12345","enable":false}"#
        ))
        .is_err());
        assert!(prepare_admin(&request(
            r#"{"kind":"startup_set","source":"nowhere","name":"x","enable":false}"#
        ))
        .is_err());
        assert!(prepare_admin(&request(
            r#"{"kind":"startup_set","source":"machine_run","name":"x"}"#
        ))
        .is_err());
        let step = startup::UndoStep {
            source: Source::MachineRun,
            name: "No Such Entry Exists 12345".into(),
            previous: None,
        };
        assert!(prepare_admin(&request(&startup_undo_request(&[step]))).is_err());
    }
}
