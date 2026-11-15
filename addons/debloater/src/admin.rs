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
    restore::{self, WinRestore},
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
}
