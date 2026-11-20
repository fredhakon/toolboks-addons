//! The "Advanced" block of the memory tool: the disruptive actions, an admin-only purge of the
//! low-priority cache and the opt-in "Clean selected regions" (`regions.rs`). They run in the
//! administrator helper, only when the user asks for them, and never automatically.

use toolboks_addon_sdk::{admin::AdminChange, AdminOutcome, AdminRequest, HostRequest};

use super::freed::{self, Change};
use super::memlist::{self, MemListError, StandbyBreakdown};
use super::regions::{self, Selection, WindowsVersion};
use super::report;
use super::style::{section, unproven_note, PURGE_EVIDENCE};
use super::Memory;
use toolboks_shared::format::size_plain;
use toolboks_shared::ui::RED;

/// Shown above the button.
pub const PURGE_WARNING: &str = "Drops the cached files Windows keeps in spare memory and values \
    least. This can cause short stutters while apps and games read from disk again. It never \
    changes the memory promised to programs (commit), and Windows refills the cache by itself. \
    It only runs when you press the button.";

/// Why a separate administrator window appears.
pub const NEEDS_ADMIN: &str = "Needs administrator rights, because it changes how Windows manages memory for the whole PC. A separate window asks you to confirm.";

/// What the confirmation window shows before anything changes.
const CONFIRM_TEXT: &str =
    "Clear the low-priority cache?\n\nWindows will drop the cached files it keeps \
    in spare memory and values least. This can cause short stutters while apps and games read \
    from disk again. It does not change the memory promised to programs (commit), and Windows \
    refills the cache by itself.\n\nToolboks cannot undo this, but it is safe to repeat.";

/// The one administrator operation this add-on has. Its payload says which change: `{}` is the
/// low-priority cache purge, `{"regions": <mask>}` is "Clean selected regions" (bit 0 is
/// region 1). Same pattern as the Debloater's single operation name with a `kind`.
pub const OPERATION: &str = "purge_standby";

/// Shown above the region ticks.
const REGIONS_INTRO: &str = "Tick the memory regions to clean, then press the button. Nothing \
    runs until you press it, and none of this is part of automatic cleaning. The ticks are \
    saved; they start as the same set Mem Reduct uses.";

/// A purge or a region clean waiting for the host to send it to the administrator helper.
pub struct PurgeJob {
    id: u64,
    /// "Clean selected regions" rather than the low-priority purge, so the answer lands under
    /// the right button.
    regions: bool,
}

/// The failure as a clause that follows what was attempted ("... needs admin rights").
pub(crate) fn failure_text(error: MemListError) -> String {
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
        self.start_admin("{}".into(), false);
    }

    /// Asks for the ticked regions that can run on this Windows. Ignored while busy or when
    /// none is left to run.
    fn start_regions(&mut self) {
        let selection = self.regions.available(self.windows_version);
        if selection.is_empty() {
            return;
        }
        let json = serde_json::json!({ "regions": selection.bits() }).to_string();
        self.start_admin(json, true);
    }

    fn start_admin(&mut self, json: String, regions: bool) {
        if self.busy() {
            return;
        }
        self.next_request = self.next_request.wrapping_add(1);
        self.purge = Some(PurgeJob {
            id: self.next_request,
            regions,
        });
        self.request = Some(HostRequest::AdminOperation(AdminRequest {
            id: self.next_request,
            operation: OPERATION.into(),
            json,
        }));
    }

    /// The helper answered. This only shows the outcome; it never authorizes anything.
    #[cfg(test)]
    fn take_request_for_test(&mut self) -> Option<HostRequest> {
        self.request.take()
    }

    pub(super) fn purge_finished(&mut self, id: u64, outcome: AdminOutcome) {
        let Some(job) = self.purge.as_ref().filter(|job| job.id == id) else {
            return;
        };
        let regions = job.regions;
        self.purge = None;
        let text = match outcome {
            AdminOutcome::Cancelled => "Cancelled. Nothing was changed.".to_string(),
            AdminOutcome::Failed { reason } => reason,
            AdminOutcome::Completed { json } => {
                let value = serde_json::from_str::<serde_json::Value>(&json).unwrap_or_default();
                if regions {
                    let (whole, parts) = freed::measured_from_json(&value["measured"]);
                    self.record_result(
                        report::Trigger::Manual,
                        parts.iter().map(|(label, _)| label.clone()).collect(),
                        whole,
                        freed::parts_text(&parts),
                    );
                }
                value["text"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        if regions {
                            "The selected regions were cleaned.".to_string()
                        } else {
                            "The cache was cleared.".to_string()
                        }
                    })
            }
        };
        let line = Some(format!(
            "{}  {}",
            toolboks_shared::win::local_time_hms(),
            text
        ));
        if regions {
            self.last_regions = line;
        } else {
            self.last_purge = line;
        }
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
        unproven_note(ui, PURGE_EVIDENCE);
        if let Some(text) = &self.last_purge {
            ui.label(egui::RichText::new(text).small());
        }
        self.regions_block(ui);
    }

    /// The eight regions, each with its note, evidence label and warning, and the one button.
    fn regions_block(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        section(ui, "Clean selected regions");
        ui.label(egui::RichText::new(REGIONS_INTRO).weak().small());
        for region in regions::ALL {
            let reason = region.unavailable(self.windows_version);
            let mut on = reason.is_none() && self.regions.contains(region);
            ui.add_enabled_ui(reason.is_none(), |ui| {
                if ui.checkbox(&mut on, region.label()).changed() {
                    self.regions.set(region, on);
                }
            });
            ui.label(egui::RichText::new(region.note()).weak().small());
            if let Some(warning) = region.freeze_warning() {
                ui.label(egui::RichText::new(warning).small().color(RED));
            }
            if let Some(reason) = reason {
                ui.label(egui::RichText::new(format!("Not available: {reason}.")).small());
            }
            unproven_note(ui, region.evidence());
        }
        let ready = !self.regions.available(self.windows_version).is_empty();
        let button = ui
            .add_enabled(
                ready && !self.busy(),
                egui::Button::new("Clean selected regions"),
            )
            .on_hover_text("Cleans only the ticked regions, after you confirm.");
        if button.clicked() {
            self.start_regions();
        }
        ui.label(egui::RichText::new(NEEDS_ADMIN).weak().small());
        if let Some(text) = &self.last_regions {
            ui.label(egui::RichText::new(text).small());
        }
    }
}

/// The purge as the administrator helper sees it: validated, with nothing changed yet.
struct PurgeChange;

/// "Clean selected regions" as the administrator helper sees it: validated, nothing changed yet.
struct RegionsChange {
    selection: Selection,
}

/// Read-only check of the request; the helper's own window asks the user before `apply` runs.
pub fn prepare_admin(request: &AdminRequest) -> Result<Box<dyn AdminChange>, String> {
    prepare_with(request, regions::windows_version())
}

fn prepare_with(
    request: &AdminRequest,
    version: Option<WindowsVersion>,
) -> Result<Box<dyn AdminChange>, String> {
    if request.operation != OPERATION {
        return Err("This add-on does not support that administrator action.".into());
    }
    let payload: serde_json::Value = serde_json::from_str(&request.json)
        .map_err(|_| "The administrator request could not be read.".to_string())?;
    // Only an empty object is the old purge; anything else without "regions" is refused.
    let Some(mask) = payload.get("regions") else {
        return match payload.as_object() {
            Some(object) if object.is_empty() => Ok(Box::new(PurgeChange)),
            _ => Err("The administrator request is not valid.".into()),
        };
    };
    let bits = mask
        .as_u64()
        .and_then(|bits| u8::try_from(bits).ok())
        .filter(|bits| *bits != 0)
        .ok_or("The list of regions to clean is not valid.")?;
    let selection = Selection::from_bits(bits);
    if let Some((region, reason)) = selection
        .regions()
        .find_map(|region| Some((region, region.unavailable(version)?)))
    {
        return Err(format!("{} {reason}.", region.label()));
    }
    Ok(Box::new(RegionsChange { selection }))
}

impl AdminChange for RegionsChange {
    fn capability(&self) -> &str {
        "system_tuning"
    }
    fn description(&self) -> String {
        let mut text = String::from(
            "Clean the selected memory regions?\n\nIt does not change the memory promised to \
            programs (commit), and Windows refills its caches by itself. Apps and games can be \
            slower for a moment while that happens. None of these regions has a measured benefit \
            (Unproven).\n",
        );
        for region in self.selection.regions() {
            text.push_str(&format!("\n- {}: {}", region.label(), region.note()));
            if let Some(warning) = region.freeze_warning() {
                text.push_str(&format!("\n  Warning: {warning}"));
            }
        }
        text.push_str("\n\nToolboks cannot undo this, but it is safe to repeat.");
        text
    }
    fn apply(self: Box<Self>) -> Result<String, String> {
        let before = memlist::query();
        let outcomes = regions::clean(self.selection);
        let whole = before
            .zip(memlist::query())
            .map(|(b, a)| Change::between(b, a, None));
        let (text, any_done) = regions::summary(&outcomes);
        if any_done {
            let parts: Vec<_> = outcomes
                .iter()
                .filter(|o| o.result.is_ok())
                .map(|o| (o.region.label(), o.change))
                .collect();
            Ok(serde_json::json!({
                "text": text,
                "measured": freed::measured_to_json(whole, &parts),
            })
            .to_string())
        } else {
            Err(text)
        }
    }
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

    fn regions_request(json: &str) -> AdminRequest {
        AdminRequest {
            id: 1,
            operation: OPERATION.into(),
            json: json.into(),
        }
    }

    const WINDOWS_11: Option<WindowsVersion> = Some(WindowsVersion {
        major: 10,
        minor: 0,
    });

    #[test]
    fn a_region_request_is_prepared_with_the_warnings_of_the_ticked_regions() {
        let all = regions_request(r#"{"regions": 255}"#);
        let change = prepare_with(&all, WINDOWS_11).ok().unwrap();
        assert_eq!(change.capability(), "system_tuning");
        let text = change.description();
        assert!(text.contains("Modified page list") && text.contains("Standby list (everything)"));
        assert_eq!(text.matches("Warning: ").count(), 2);
        assert!(text.len() < 8000);

        // Without the two freezing regions, no warning is shown.
        let default = regions_request(&format!(
            r#"{{"regions": {}}}"#,
            Selection::default().bits()
        ));
        let text = prepare_with(&default, WINDOWS_11)
            .ok()
            .unwrap()
            .description();
        assert!(!text.contains("Warning: "), "{text}");
    }

    #[test]
    fn region_requests_that_are_not_valid_or_not_available_are_refused() {
        let refused = |json: &str, version| prepare_with(&regions_request(json), version).err();
        for json in [
            r#"{"regions": 0}"#,
            r#"{"regions": 256}"#,
            r#"{"regions": "all"}"#,
            r#"{"regions": -1}"#,
            "not json",
        ] {
            assert!(refused(json, WINDOWS_11).is_some(), "{json}");
        }
        let old = Some(WindowsVersion { major: 6, minor: 1 });
        assert!(refused(r#"{"regions": 64}"#, old)
            .unwrap()
            .contains("Windows 8.1"));
        assert!(refused(r#"{"regions": 128}"#, old)
            .unwrap()
            .contains("Windows 10"));
        assert!(
            refused(r#"{"regions": 63}"#, old).is_none(),
            "regions 1 to 6 run anywhere"
        );
        // The payload-less purge keeps working next to the new one, but nothing else becomes it.
        assert!(prepare_with(&regions_request("{}"), old).is_ok());
        for json in ["[]", "5", "null", r#"{"region": 1}"#, r#"{"x": 1}"#] {
            assert!(refused(json, WINDOWS_11).is_some(), "{json}");
        }
    }

    #[test]
    fn the_region_button_sends_only_what_can_run_and_its_answer_lands_under_it() {
        let mut memory = Memory::new();
        memory.windows_version = Some(WindowsVersion { major: 6, minor: 1 });
        memory.regions = Selection::from_bits(0xFF);
        memory.start_regions();
        let id = memory.next_request;
        let Some(HostRequest::AdminOperation(request)) = memory.take_request_for_test() else {
            panic!("no request was queued");
        };
        // Regions 7 and 8 are dropped on Windows 7: 0xFF minus bits 6 and 7.
        assert_eq!(request.json, r#"{"regions":63}"#);
        memory.purge_finished(
            id,
            AdminOutcome::Failed {
                reason: "no".into(),
            },
        );
        assert!(memory.last_regions.is_some() && memory.last_purge.is_none());

        // Nothing runnable ticked: nothing is sent.
        memory.regions = Selection::from_bits(0b1100_0000);
        memory.start_regions();
        assert!(memory.purge.is_none() && memory.take_request_for_test().is_none());
    }
}
