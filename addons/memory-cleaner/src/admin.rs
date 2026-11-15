//! The "Advanced" block of the memory tool: the one disruptive action, an admin-only purge of
//! the low-priority cache. It runs on a worker thread so the window never freezes, only when
//! the user asks for it, and never automatically.

use toolboks_addon_sdk::{admin::AdminChange, AdminOutcome, AdminRequest, HostRequest};

use super::memlist::{self, MemListError, StandbyBreakdown};
use super::style::section;
use super::Memory;
use toolboks_shared::format::size_plain;

/// Shown above the button.
pub const PURGE_WARNING: &str = "Drops the cached files Windows keeps in spare memory and values \
    least. This can cause short stutters while apps and games read from disk again. It never \
    changes the memory promised to programs (commit), and Windows refills the cache by itself. \
    It only runs when you press the button.";

/// Why a separate administrator window appears.
pub const NEEDS_ADMIN: &str = "Needs administrator rights, because it changes how Windows     manages memory for the whole PC. A separate window asks you to confirm.";

/// What the confirmation window shows before anything changes.
const CONFIRM_TEXT: &str =
    "Clear the low-priority cache?\n\nWindows will drop the cached files it keeps \
    in spare memory and values least. This can cause short stutters while apps and games read \
    from disk again. It does not change the memory promised to programs (commit), and Windows \
    refills the cache by itself.\n\nToolboks cannot undo this, but it is safe to repeat.";

/// The one administrator operation this add-on has.
pub const OPERATION: &str = "purge_standby";

/// A purge waiting for the host to send it to the administrator helper.
pub struct PurgeJob {
    id: u64,
}

fn failure_text(error: MemListError) -> String {
    match error {
        MemListError::NotElevated => "needs admin rights".to_string(),
        MemListError::Unavailable => "is not available on this version of Windows".to_string(),
        MemListError::Status(code) => format!("failed: Windows returned status {code:#010x}"),
    }
}

/// The result line: the cache before and after, or why it did not work.
fn purge_text(
    result: Result<(), MemListError>,
    before: Option<StandbyBreakdown>,
    after: Option<StandbyBreakdown>,
) -> String {
    match result {
        Ok(()) => match (before, after) {
            (Some(b), Some(a)) => format!(
                "Cache (standby) went from {} to {}. Commit is unchanged.",
                size_plain(b.total()),
                size_plain(a.total())
            ),
            _ => {
                "The low-priority cache was dropped. The cache size could not be read.".to_string()
            }
        },
        Err(error) => format!("Clearing the low-priority cache {}.", failure_text(error)),
    }
}

impl Memory {
    /// A purge, a "Trim selected" or an optimize run is in progress.
    pub(super) fn busy(&self) -> bool {
        self.purge.is_some()
            || self.run.is_some()
            || self.trim_job.is_some()
            || self.refresh_action_pending
    }

    /// Asks the host to run the purge in the separate administrator helper. The helper shows its
    /// own Confirm / Cancel window, so nothing here can approve it. Ignored while busy.
    fn start_purge(&mut self) {
        if self.busy() {
            return;
        }
        self.next_request = self.next_request.wrapping_add(1);
        self.purge = Some(PurgeJob {
            id: self.next_request,
        });
        self.request = Some(HostRequest::AdminOperation(AdminRequest {
            id: self.next_request,
            operation: OPERATION.into(),
            json: "{}".into(),
        }));
    }

    /// The helper answered. This only shows the outcome; it never authorizes anything.
    #[cfg(test)]
    fn take_request_for_test(&mut self) -> Option<HostRequest> {
        self.request.take()
    }

    pub(super) fn purge_finished(&mut self, id: u64, outcome: AdminOutcome) {
        if self.purge.as_ref().map(|job| job.id) != Some(id) {
            return;
        }
        self.purge = None;
        let text = match outcome {
            AdminOutcome::Cancelled => "Cancelled. Nothing was changed.".to_string(),
            AdminOutcome::Failed { reason } => reason,
            AdminOutcome::Completed { json } => serde_json::from_str::<serde_json::Value>(&json)
                .ok()
                .and_then(|v| v["text"].as_str().map(str::to_string))
                .unwrap_or_else(|| "The cache was cleared.".to_string()),
        };
        self.last_purge = Some(format!(
            "{}  {}",
            toolboks_shared::win::local_time_hms(),
            text
        ));
        self.refresh_after_purge = true;
    }

    pub(super) fn advanced_block(&mut self, ui: &mut egui::Ui) {
        section(ui, "Advanced");
        ui.label(egui::RichText::new(PURGE_WARNING).weak().small());
        if self.purge.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Waiting for the administrator window\u{2026}");
            });
        }
        let cache = self
            .lists
            .map_or_else(|| "unknown".to_string(), |l| size_plain(l.standby));
        ui.label(egui::RichText::new(format!("Cache now: {cache}")).small());

        let button = ui
            .add_enabled(!self.busy(), egui::Button::new("Clear low-priority cache"))
            .on_hover_text("Drops only the lowest-priority cached pages.");
        if button.clicked() {
            self.start_purge();
        }
        ui.label(egui::RichText::new(NEEDS_ADMIN).weak().small());
        if let Some(text) = &self.last_purge {
            ui.label(egui::RichText::new(text).small());
        }
    }
}

/// The purge as the administrator helper sees it: validated, with nothing changed yet.
struct PurgeChange;

/// Read-only check of the request; the helper's own window asks the user before `apply` runs.
pub fn prepare_admin(request: &AdminRequest) -> Result<Box<dyn AdminChange>, String> {
    if request.operation != OPERATION {
        return Err("This add-on does not support that administrator action.".into());
    }
    Ok(Box::new(PurgeChange))
}

impl AdminChange for PurgeChange {
    fn capability(&self) -> &str {
        "system_tuning"
    }
    fn description(&self) -> String {
        CONFIRM_TEXT.to_string()
    }
    fn apply(self: Box<Self>) -> Result<String, String> {
        let before = memlist::standby_breakdown();
        let result = memlist::purge_low_priority_standby();
        let after = memlist::standby_breakdown();
        let text = purge_text(result, before, after);
        match result {
            Ok(()) => Ok(serde_json::json!({ "text": text }).to_string()),
            Err(_) => Err(text),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn breakdown(total_mb: u64) -> StandbyBreakdown {
        let mut by_priority = [0u64; 8];
        by_priority[0] = total_mb * 1024 * 1024;
        StandbyBreakdown { by_priority }
    }

    #[test]
    fn purge_text_shows_cache_before_and_after() {
        let text = purge_text(Ok(()), Some(breakdown(4096)), Some(breakdown(900)));
        assert!(text.contains("from 4.0 GB to 900 MB"), "{text}");
        assert!(text.contains("Commit is unchanged"));
        assert!(!text.to_lowercase().contains("freed"));
    }

    #[test]
    fn purge_text_handles_unreadable_sizes() {
        assert!(purge_text(Ok(()), None, Some(breakdown(1))).contains("could not be read"));
    }

    #[test]
    fn purge_failures_name_the_cause() {
        assert!(purge_text(Err(MemListError::NotElevated), None, None).contains("admin"));
        assert!(
            purge_text(Err(MemListError::Status(-1073741823)), None, None).contains("0xc0000001")
        );
        assert!(failure_text(MemListError::Unavailable).contains("not available"));
    }

    #[test]
    fn warning_covers_stutters_and_commit() {
        assert!(PURGE_WARNING.contains("short stutters"));
        assert!(PURGE_WARNING.contains("commit"));
        assert!(NEEDS_ADMIN.contains("administrator"));
    }

    #[test]
    fn only_the_purge_is_prepared_and_it_needs_the_system_tuning_capability() {
        let request = |operation: &str| AdminRequest {
            id: 1,
            operation: operation.into(),
            json: "{}".into(),
        };
        assert!(prepare_admin(&request("delete_everything")).is_err());
        let change = prepare_admin(&request(OPERATION)).ok().unwrap();
        assert_eq!(change.capability(), "system_tuning");
        let text = change.description();
        assert!(text.contains("short stutters") && text.contains("commit"));
        assert!(text.len() < 8000);
    }

    #[test]
    fn a_stale_or_cancelled_answer_changes_nothing_wrong() {
        let mut memory = Memory::new();
        memory.start_purge();
        let id = memory.next_request;
        assert!(memory.busy());
        assert!(matches!(
            memory.take_request_for_test(),
            Some(HostRequest::AdminOperation(r)) if r.id == id && r.operation == OPERATION
        ));
        // An answer for another request is ignored and the purge stays pending.
        memory.purge_finished(id + 1, AdminOutcome::Cancelled);
        assert!(memory.purge.is_some() && memory.last_purge.is_none());
        memory.purge_finished(id, AdminOutcome::Cancelled);
        assert!(memory.purge.is_none());
        assert!(memory
            .last_purge
            .as_deref()
            .unwrap()
            .contains("Nothing was changed"));
        assert!(memory.refresh_after_purge);
    }
}
