//! The administrator side of System Profiles: what the separate elevated helper validates,
//! shows in its own Confirm / Cancel window, and then runs. The drawing process never gets here
//! with rights; it only sends the request and shows the answer.
//!
//! The request names choices by id. The preview text is built here from the add-on's own list
//! and from the registry, never from text that arrived in the request.
use crate::engine::{self, Action, CHOICES};
use serde_json::{json, Value};
use toolboks_addon_sdk::{admin::AdminChange, AdminRequest};

/// The one administrator operation this add-on has. The request's `kind` says apply or restore.
pub const OPERATION: &str = "gaming_profile";

/// The request for an action, as sent to the helper.
pub fn request_json(action: &Action) -> String {
    match action {
        Action::Apply {
            selected,
            enable_protection,
        } => json!({"kind": "apply", "selected": selected, "enable_protection": enable_protection}),
        Action::Restore => json!({"kind": "restore"}),
    }
    .to_string()
}

struct Prepared {
    action: Action,
    description: String,
}

/// Read-only check of the request. Nothing changes until the user confirms in the helper's window.
pub fn prepare_admin(request: &AdminRequest) -> Result<Box<dyn AdminChange>, String> {
    if request.operation != OPERATION {
        return Err("This add-on does not support that administrator action.".into());
    }
    let value: Value = serde_json::from_str(&request.json)
        .map_err(|_| "The administrator request was not understood.".to_string())?;
    match value["kind"].as_str() {
        Some("apply") => {
            let mut selected: Vec<&'static str> = Vec::new();
            for id in value["selected"].as_array().map_or(&[][..], Vec::as_slice) {
                let choice = CHOICES
                    .iter()
                    .find(|choice| id.as_str() == Some(choice.id))
                    .ok_or("The request names a change this add-on does not offer.")?;
                if !selected.contains(&choice.id) {
                    selected.push(choice.id);
                }
            }
            let enable_protection = value["enable_protection"].as_bool().unwrap_or(false);
            if selected.is_empty() && !enable_protection {
                return Err("Choose at least one change.".into());
            }
            if engine::has_backup() {
                return Err("A profile is already applied. Restore it first.".into());
            }
            let description = apply_description(&selected, enable_protection);
            Ok(Box::new(Prepared {
                action: Action::Apply {
                    selected,
                    enable_protection,
                },
                description,
            }))
        }
        Some("restore") => {
            if !engine::has_backup() {
                return Err("There is nothing to restore.".into());
            }
            let lines = engine::review_backup()?;
            Ok(Box::new(Prepared {
                action: Action::Restore,
                description: restore_description(&lines),
            }))
        }
        _ => Err("The administrator request was not understood.".into()),
    }
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

fn apply_description(selected: &[&'static str], enable_protection: bool) -> String {
    let mut text = String::from(
        "Apply the Gaming profile?\n\nBefore anything changes, Windows must confirm a new restore point, and the current values are saved to a registry backup.",
    );
    if enable_protection {
        text.push_str("\n\nSystem Protection will be turned on for the Windows drive.");
    }
    let lines = engine::preview(selected);
    if !lines.is_empty() {
        text.push_str("\n\nChanges (now, then new):\n");
        text.push_str(&lines.join("\n"));
    }
    text.push_str("\n\nRestore puts back the settings Toolboks applied.");
    limited(text)
}

fn restore_description(lines: &[String]) -> String {
    let mut text =
        String::from("Restore the settings Toolboks applied?\n\nRegistry edits to reverse:\n");
    text.push_str(&lines.join("\n"));
    limited(text)
}

impl AdminChange for Prepared {
    fn capability(&self) -> &str {
        "system_tuning"
    }
    fn description(&self) -> String {
        self.description.clone()
    }
    fn apply(self: Box<Self>) -> Result<String, String> {
        let Prepared { action, .. } = *self;
        let mut lines = Vec::new();
        let result = engine::run(action, |line| lines.push(line));
        Ok(json!({
            "ok": result.is_ok(),
            "error": result.err(),
            "lines": lines,
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

    #[test]
    fn the_request_round_trips_through_the_preparation() {
        let selected = vec![CHOICES[0].id];
        let json = request_json(&Action::Apply {
            selected,
            enable_protection: false,
        });
        // A profile may already be applied on this PC; either way the request is understood.
        match prepare_admin(&request(&json)) {
            Ok(change) => {
                assert_eq!(change.capability(), "system_tuning");
                let text = change.description();
                assert!(text.contains("restore point") && text.contains("registry backup"));
                assert!(text.contains(CHOICES[0].title) || text.contains("•"));
                assert!(text.len() < 8000);
            }
            Err(error) => assert!(error.contains("already applied"), "{error}"),
        }
    }

    #[test]
    fn unknown_operations_kinds_and_ids_are_refused() {
        let bad_operation = AdminRequest {
            id: 1,
            operation: "format_disk".into(),
            json: "{}".into(),
        };
        assert!(prepare_admin(&bad_operation).is_err());
        assert!(prepare_admin(&request("not json")).is_err());
        assert!(prepare_admin(&request(r#"{"kind":"launch"}"#)).is_err());
        assert!(
            prepare_admin(&request(r#"{"kind":"apply","selected":["not-a-choice"]}"#)).is_err()
        );
        assert!(prepare_admin(&request(r#"{"kind":"apply","selected":[]}"#)).is_err());
    }

    #[test]
    fn protection_alone_is_a_valid_request() {
        let result = prepare_admin(&request(
            r#"{"kind":"apply","selected":[],"enable_protection":true}"#,
        ));
        match result {
            Ok(change) => assert!(change.description().contains("System Protection")),
            Err(error) => assert!(error.contains("already applied"), "{error}"),
        }
    }

    #[test]
    fn long_text_is_cut_inside_the_limit() {
        let long = limited("é".repeat(10_000));
        assert!(long.len() < 8000 && long.ends_with("..."));
    }
}
