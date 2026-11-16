mod admin;
mod engine;

pub use admin::prepare_admin;

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ffi::OsStr,
    fs::File,
    io::{Read, Seek, SeekFrom},
    os::windows::ffi::OsStrExt,
};

use egui::{self, RichText};
use toolboks_addon_sdk::{
    tutorial, AdminOutcome, AdminRequest, Capability, HostRequest, NativeAddon, TutorialStep,
};
use windows::{core::PCWSTR, Win32::UI::Shell::ShellExecuteW};

use engine::{Action, Choice, Evidence, CHOICES, GROUPS};
use toolboks_shared::ui as theme;

pub const ID: &str = "toolboks.system-profiles";
const PAGE: &str = "Gaming";
/// Why a separate window appears when a profile is applied or restored.
const ADMIN_NOTE: &str =
    "A separate administrator window shows exactly what will change. Confirm or cancel there.";
/// A restore point and registry backup are made first, but neither promises a full return.
const RECOVERY_NOTE: &str = "A restore point and a registry backup are made first. They do not guarantee full recovery: Restore only puts back the registry values and power plan Toolboks changed, and leaves values changed by something else.";
const LOG_LINES: usize = 120;

#[derive(Default)]
pub struct Gaming {
    allowed: bool,
    initialized: bool,
    selected: BTreeSet<&'static str>,
    enable_protection: bool,
    request: Option<HostRequest>,
    /// The administrator request that has been sent and not answered yet.
    pending: Option<u64>,
    next_request: u64,
    log: VecDeque<String>,
    status: Option<Result<(), String>>,
    message: Option<String>,
    /// The last review. Apply needs one made for exactly the current state (see `ReviewKey`).
    preview: Option<Review>,
}

struct Guide {
    title: &'static str,
    instructions: &'static str,
    link: Option<(&'static str, &'static str)>,
}

const GUIDES: &[Guide] = &[
    Guide {
        title: "Windowed optimizations and GPU scheduling",
        instructions: "Open System → Display → Graphics. Turn on Optimizations for windowed games and Hardware-accelerated GPU scheduling if available. Restart Windows after changing GPU scheduling.",
        link: Some(("Open Graphics settings", "ms-settings:display-advancedgraphics")),
    },
    Guide {
        title: "NVIDIA global graphics settings",
        instructions: "Open NVIDIA Control Panel → Manage 3D Settings → Global Settings. Note your current values. Set Shader Cache Size to 10 GB, Vertical sync to Off, and Power management mode to Prefer maximum performance. Apply. These settings affect other 3D apps too.",
        link: Some(("NVIDIA settings reference", "https://www.nvidia.com/content/Control-Panel-Help/vLatest/en-us/mergedProjects/3D%20Settings/Manage_3D_Settings_%28reference%29.htm")),
    },
    Guide {
        title: "Memory Integrity",
        instructions: "Windows Security → Device security → Core isolation details. Turning off Memory integrity weakens Windows protection against malicious drivers. If you choose to change it, restart Windows. Return here to turn it back on.",
        link: Some(("Open Windows Security", "windowsdefender:")),
    },
    Guide {
        title: "Temporary files and Disk Cleanup",
        instructions: "Open Settings → System → Storage → Temporary files, or search Start for Disk Cleanup. Review the selected categories before deleting. Deleted files cannot be recovered using this profile.",
        link: Some(("Open Storage settings", "ms-settings:storagesense")),
    },
    Guide {
        title: "Storage Sense",
        instructions: "Open Settings → System → Storage and switch Storage Sense off to stop automatic cleanup.",
        link: Some(("Open Storage Sense", "ms-settings:storagepolicies")),
    },
    Guide {
        title: "Location",
        instructions: "Open Privacy & security → Location. Turn off Location services to disable location access for the device.",
        link: Some(("Open Location settings", "ms-settings:privacy-location")),
    },
    Guide {
        title: "Start and search app-launch tracking",
        instructions: "Under Privacy & security, open General or Recommendations & offers. Turn off the option that improves Start and search results by tracking app launches.",
        link: Some(("Open privacy settings", "ms-settings:privacy-general")),
    },
    Guide {
        title: "Notifications",
        instructions: "Open System → Notifications and switch Notifications off.",
        link: Some(("Open Notifications settings", "ms-settings:notifications")),
    },
    Guide {
        title: "Taskbar End Task",
        instructions: "Search Windows Settings for End Task and enable it under Advanced or For developers, depending on your Windows version. Right-click an app in the taskbar to find End task.",
        link: Some(("Open developer settings", "ms-settings:developers")),
    },
    Guide {
        title: "Windows Terminal default profile",
        instructions: "Install PowerShell 7 if needed. In Windows Terminal, open Settings → Startup, select PowerShell as the default profile, and save. Windows PowerShell is the older profile.",
        link: Some(("Windows Terminal setup", "https://learn.microsoft.com/windows/terminal/install")),
    },
    Guide {
        title: "Recall",
        instructions: "On a Copilot+ PC, open Privacy & security → Recall & snapshots and turn off Save snapshots. Removing Recall through Windows Features also permanently deletes its snapshots.",
        link: Some(("Microsoft Recall settings", "https://support.microsoft.com/en-us/windows/privacy/privacy-and-control-over-your-recall-experience")),
    },
    Guide {
        title: "Explorer automatic folder discovery",
        instructions: "To fix the view type of a folder, open its Properties → Customize and choose a template under Optimize this folder for. The global registry override is undocumented.",
        link: None,
    },
    Guide {
        title: "HomeGroup and Wi-Fi Sense",
        instructions: "Current Windows versions have removed these features. There is no setting to change.",
        link: None,
    },
];

fn open(target: &OsStr) -> Result<(), String> {
    let target: Vec<u16> = target.encode_wide().chain(Some(0)).collect();
    // SAFETY: target is a NUL-terminated path or a fixed settings URI. No arguments are passed.
    let code = unsafe {
        ShellExecuteW(
            None,
            windows::core::w!("open"),
            PCWSTR(target.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        )
    };
    if (code.0 as isize) <= 32 {
        Err("Windows could not open that location.".into())
    } else {
        Ok(())
    }
}

fn heading(ui: &mut egui::Ui, title: &str) {
    ui.label(RichText::new(title).size(theme::TEXT_SECTION).strong());
}

/// Only choices the user saved are on. Nothing is preselected, whatever its evidence.
fn saved_selection(preferences: &BTreeMap<String, serde_json::Value>) -> BTreeSet<&'static str> {
    let ids = preferences
        .get("selected")
        .and_then(|value| value.as_array());
    CHOICES
        .iter()
        .filter(|choice| ids.is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(choice.id))))
        .map(|choice| choice.id)
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ReviewMode {
    Apply,
    Recovery,
}

/// What a review was made for. Apply unlocks only while this equals the current state.
#[derive(Clone, PartialEq, Eq, Debug)]
struct ReviewKey {
    mode: ReviewMode,
    selected: Vec<&'static str>,
    enable_protection: bool,
}

/// What would change, per heading, for the state in `key`.
struct Review {
    key: ReviewKey,
    groups: Vec<(String, Vec<String>)>,
}

const PROTECTION_LINE: &str =
    "Turned on for the Windows drive. Windows keeps it enabled after a profile restore.";

fn review_key(
    recovery: bool,
    selected: &BTreeSet<&'static str>,
    enable_protection: bool,
) -> ReviewKey {
    ReviewKey {
        mode: if recovery {
            ReviewMode::Recovery
        } else {
            ReviewMode::Apply
        },
        selected: selected.iter().copied().collect(),
        enable_protection,
    }
}

/// Keeps a review only if it was made for the current state.
fn current_review(review: Option<Review>, key: &ReviewKey) -> Option<Review> {
    review.filter(|review| review.key == *key)
}

/// The apply review: the System Protection change (if chosen) first, then each choice's values.
fn apply_review(key: &ReviewKey, by_choice: Vec<(&'static str, Vec<String>)>) -> Review {
    let mut groups = Vec::new();
    if key.enable_protection {
        groups.push((
            "System Protection".to_string(),
            vec![PROTECTION_LINE.into()],
        ));
    }
    groups.extend(
        by_choice
            .into_iter()
            .map(|(title, lines)| (title.to_string(), lines)),
    );
    Review {
        key: key.clone(),
        groups,
    }
}

/// Apply is only offered after a review of exactly this apply selection. A recovery review or
/// a review of an older selection never counts.
fn can_apply(busy: bool, key: &ReviewKey, review: Option<&Review>) -> bool {
    !busy
        && key.mode == ReviewMode::Apply
        && (!key.selected.is_empty() || key.enable_protection)
        && review.is_some_and(|review| review.key == *key)
}

/// Text badge: the label carries the meaning, the colour only backs it up.
fn badge(ui: &mut egui::Ui, evidence: Option<Evidence>) {
    let (text, color) = match evidence {
        Some(Evidence::Measured) => (Evidence::Measured.label(), theme::GREEN),
        Some(Evidence::Mixed) => (Evidence::Mixed.label(), theme::AMBER),
        Some(Evidence::Unproven) => (Evidence::Unproven.label(), theme::RED),
        None => ("Not a speed tweak", theme::MUTED),
    };
    ui.label(
        RichText::new(text)
            .size(theme::TEXT_CAPTION)
            .strong()
            .color(color),
    );
}

fn choice_row(ui: &mut egui::Ui, choice: &Choice, on: &mut bool) -> bool {
    let mut changed = false;
    ui.horizontal_wrapped(|ui| {
        changed = ui.checkbox(on, choice.title).changed();
        badge(ui, choice.evidence);
    });
    ui.indent(choice.id, |ui| {
        ui.label(choice.why);
        ui.label(RichText::new(format!("Changes: {}", choice.detail)).color(theme::MUTED));
        if choice.source.starts_with("https://") {
            ui.hyperlink_to("Source: Microsoft Learn", choice.source);
        } else {
            ui.label(
                RichText::new(choice.source)
                    .size(theme::TEXT_CAPTION)
                    .color(theme::MUTED),
            );
        }
    });
    ui.add_space(theme::GAP);
    changed
}

impl Gaming {
    fn initialize(&mut self, preferences: &BTreeMap<String, serde_json::Value>) {
        if self.initialized {
            return;
        }
        self.initialized = true;
        self.selected = saved_selection(preferences);
        self.enable_protection = preferences
            .get("enable_protection")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        if let Ok(mut file) = File::open(engine::root().join(engine::LOG)) {
            let len = file.metadata().map_or(0, |meta| meta.len());
            let offset = len.saturating_sub(32 * 1024);
            let mut bytes = Vec::new();
            if file.seek(SeekFrom::Start(offset)).is_ok()
                && file.take(32 * 1024).read_to_end(&mut bytes).is_ok()
            {
                let text = String::from_utf8_lossy(&bytes);
                let log = if offset > 0 {
                    text.split_once('\n').map_or("", |(_, rest)| rest)
                } else {
                    &text
                };
                let mut recent: Vec<_> = log.lines().rev().take(LOG_LINES).collect();
                recent.reverse();
                self.log.extend(recent.into_iter().map(str::to_owned));
            }
        }
    }

    /// Sends one administrator request. The separate helper checks it, shows its own Confirm /
    /// Cancel window and only then changes anything; nothing here can approve it.
    fn start(&mut self, action: Action) {
        if !self.allowed || self.pending.is_some() {
            return;
        }
        self.status = None;
        self.message = None;
        self.preview = None;
        self.next_request = self.next_request.wrapping_add(1);
        self.pending = Some(self.next_request);
        self.request = Some(HostRequest::AdminOperation(AdminRequest {
            id: self.next_request,
            operation: admin::OPERATION.into(),
            json: admin::request_json(&action),
        }));
    }

    fn push_line(&mut self, line: String) {
        if self.log.len() == LOG_LINES {
            self.log.pop_front();
        }
        self.log.push_back(line);
    }

    /// The helper answered. This only shows the outcome; it never authorizes anything.
    fn finished(&mut self, id: u64, outcome: AdminOutcome) {
        if self.pending != Some(id) {
            return;
        }
        self.pending = None;
        match outcome {
            AdminOutcome::Cancelled => {
                self.message = Some("Cancelled. Nothing was changed.".into());
            }
            AdminOutcome::Failed { reason } => self.status = Some(Err(reason)),
            AdminOutcome::Completed { json } => {
                let value: serde_json::Value =
                    serde_json::from_str(&json).unwrap_or(serde_json::Value::Null);
                for line in value["lines"].as_array().into_iter().flatten() {
                    if let Some(line) = line.as_str() {
                        self.push_line(line.to_owned());
                    }
                }
                self.status = Some(if value["ok"].as_bool() == Some(true) {
                    Ok(())
                } else {
                    Err(value["error"]
                        .as_str()
                        .unwrap_or("The profile was interrupted. Check the log and recovery data.")
                        .to_owned())
                });
            }
        }
    }

    fn configuration(&mut self, ui: &mut egui::Ui) -> bool {
        let mut changed = false;
        let busy = self.pending.is_some();
        let recovery = engine::has_backup();
        heading(ui, "Profile changes");
        ui.label(RichText::new("Nothing is selected by default. Pick only what you want, then review the changes. Windows must confirm a new restore point before Toolboks changes the profile settings.").color(theme::MUTED));
        ui.add_enabled_ui(!busy && !recovery, |ui| {
            changed |= ui.checkbox(&mut self.enable_protection, "Turn on System Protection for the Windows drive").changed();
            ui.label(RichText::new("Enable this if System Protection is off. Windows keeps it enabled after a profile restore.").color(theme::MUTED));
            ui.separator();
            for group in GROUPS {
                ui.label(RichText::new(group.title()).size(theme::TEXT_SECTION).strong());
                ui.label(RichText::new(group.intro()).color(theme::MUTED));
                ui.add_space(theme::SPACE_XS);
                for choice in CHOICES.iter().filter(|choice| choice.group == group) {
                    let mut on = self.selected.contains(choice.id);
                    if choice_row(ui, choice, &mut on) {
                        changed = true;
                        if on {
                            self.selected.insert(choice.id);
                        } else {
                            self.selected.remove(choice.id);
                        }
                    }
                }
            }
        });
        let key = review_key(recovery, &self.selected, self.enable_protection);
        self.preview = current_review(self.preview.take(), &key);
        let has_changes = !self.selected.is_empty() || self.enable_protection;
        let reviewed = self.preview.is_some();
        let review_label = if recovery {
            "Review registry edits"
        } else {
            "Review changes"
        };
        let review_button = if reviewed {
            egui::Button::new(review_label)
        } else {
            primary(review_label)
        };
        if ui
            .add_enabled(!busy && (recovery || has_changes), review_button)
            .clicked()
        {
            self.preview = Some(if recovery {
                let lines = engine::review_backup().unwrap_or_else(|error| vec![error]);
                Review {
                    key: key.clone(),
                    groups: vec![("Saved registry edits".to_string(), lines)],
                }
            } else {
                let selected: Vec<_> = CHOICES
                    .iter()
                    .filter(|choice| self.selected.contains(choice.id))
                    .map(|choice| choice.id)
                    .collect();
                apply_review(&key, engine::preview_by_choice(&selected))
            });
        }
        let unlocked = can_apply(busy, &key, self.preview.as_ref());
        if let Some(Review { groups, .. }) = &self.preview {
            ui.label(
                RichText::new("Now, then new value")
                    .size(theme::TEXT_CAPTION)
                    .color(theme::MUTED),
            );
            for (title, lines) in groups {
                ui.label(RichText::new(title).strong());
                for line in lines {
                    ui.label(RichText::new(line).color(theme::MUTED));
                }
            }
        }
        ui.add_space(theme::SPACE_XS);
        ui.horizontal_wrapped(|ui| {
            if !recovery {
                let apply = if unlocked {
                    primary("Apply Gaming profile")
                } else {
                    egui::Button::new("Apply Gaming profile")
                };
                if ui.add_enabled(unlocked, apply).clicked() {
                    let selected = CHOICES
                        .iter()
                        .filter(|choice| self.selected.contains(choice.id))
                        .map(|choice| choice.id)
                        .collect();
                    self.start(Action::Apply {
                        selected,
                        enable_protection: self.enable_protection,
                    });
                }
            }
            if ui
                .add_enabled(!busy && recovery, egui::Button::new("Restore"))
                .clicked()
            {
                self.start(Action::Restore);
            }
            if ui
                .add_enabled(
                    engine::root().is_dir(),
                    egui::Button::new("Open backup location"),
                )
                .clicked()
            {
                self.message = open(engine::root().as_os_str()).err();
            }
        });
        if !recovery && !unlocked {
            ui.label(
                RichText::new("Review changes first. Apply unlocks for exactly what you reviewed.")
                    .color(theme::MUTED),
            );
        }
        ui.label(RichText::new(ADMIN_NOTE).color(theme::MUTED));
        if !recovery {
            ui.label(RichText::new(RECOVERY_NOTE).color(theme::MUTED));
        }
        if recovery {
            ui.label(RichText::new("Restore uses the saved values for registry and power plan changes made by Toolboks. Guided changes are reversed in their original apps.").color(theme::MUTED));
        }
        changed
    }

    fn activity(&self, ui: &mut egui::Ui) {
        heading(ui, "Activity log");
        if self.pending.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Waiting for the administrator window or Windows…");
            });
        }
        if let Some(status) = &self.status {
            match status {
                Ok(()) => {
                    ui.label(RichText::new("Finished").color(theme::GREEN));
                }
                Err(error) => {
                    ui.label(RichText::new(error).color(theme::AMBER));
                }
            }
        }
        if let Some(message) = &self.message {
            ui.label(RichText::new(message).color(theme::AMBER));
        }
        if self.log.is_empty() {
            ui.label(
                RichText::new("Backup, apply, and restore activity will appear here.")
                    .color(theme::MUTED),
            );
        } else {
            egui::ScrollArea::vertical()
                .id_salt("gaming-log")
                .max_height(220.0)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for line in &self.log {
                        ui.label(RichText::new(line).monospace().size(theme::TEXT_CAPTION));
                    }
                });
        }
    }

    fn guides(&mut self, ui: &mut egui::Ui) {
        heading(ui, "Guided settings");
        ui.label(RichText::new("Follow these steps in Windows or NVIDIA Control Panel. Record your original settings before changing them.").color(theme::MUTED));
        for guide in GUIDES {
            ui.collapsing(guide.title, |ui| {
                ui.label(guide.instructions);
                if let Some((label, link)) = guide.link {
                    if link.starts_with("https://") {
                        ui.hyperlink_to(label, link);
                    } else if ui.button(label).clicked() {
                        self.message = open(OsStr::new(link)).err();
                    }
                }
                if guide.title == "Temporary files and Disk Cleanup"
                    && ui.button("Open Disk Cleanup").clicked()
                {
                    self.message = toolboks_shared::win::windows_dir()
                        .and_then(|dir| open(dir.join("System32/cleanmgr.exe").as_os_str()))
                        .err();
                }
            });
        }
    }
}

fn primary(label: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(label).color(egui::Color32::WHITE))
        .fill(theme::HOST_ACCENT_FILL)
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

impl NativeAddon for Gaming {
    fn set_capabilities(&mut self, capabilities: &[Capability]) {
        self.allowed = capabilities.contains(&Capability::SystemTuning);
    }

    fn prepare_admin(
        &mut self,
        request: &AdminRequest,
    ) -> Result<Box<dyn toolboks_addon_sdk::admin::AdminChange>, String> {
        admin::prepare_admin(request)
    }

    fn admin_result(&mut self, id: u64, outcome: AdminOutcome) {
        self.finished(id, outcome);
    }

    fn take_host_request(&mut self) -> Option<HostRequest> {
        self.request.take()
    }

    fn ui(
        &mut self,
        ui: &mut egui::Ui,
        preferences: &mut BTreeMap<String, serde_json::Value>,
    ) -> bool {
        style_once(ui.ctx());
        if !self.allowed {
            ui.label("The System tuning capability is required.");
            return false;
        }
        self.initialize(preferences);
        ui.label(RichText::new(PAGE).size(theme::TEXT_TITLE).strong());
        ui.label(
            RichText::new(
                "Choose Windows settings for your gaming setup and keep a recovery record.",
            )
            .color(theme::MUTED),
        );
        ui.add_space(theme::SPACE_XS);
        let mut changed = false;
        egui::ScrollArea::vertical()
            .id_salt("gaming-profile")
            .show(ui, |ui| {
                let width = ui.available_width();
                tutorial::region(ui, "gaming-activity", |ui| {
                    theme::card(ui, width, 0.0, |ui| self.activity(ui));
                });
                ui.add_space(theme::GAP);
                tutorial::region(ui, "gaming-changes", |ui| {
                    theme::card(ui, width, 0.0, |ui| {
                        changed = self.configuration(ui);
                    });
                });
                ui.add_space(theme::GAP);
                tutorial::region(ui, "gaming-guides", |ui| {
                    theme::card(ui, width, 0.0, |ui| self.guides(ui));
                });
            });
        if changed {
            preferences.insert("selected".into(), serde_json::json!(self.selected));
            preferences.insert(
                "enable_protection".into(),
                serde_json::json!(self.enable_protection),
            );
        }
        changed
    }

    fn has_background_work(&self) -> bool {
        self.pending.is_some()
    }

    fn tutorial_location(&self) -> String {
        PAGE.into()
    }

    fn tutorial_steps(&self) -> Vec<TutorialStep> {
        vec![
            TutorialStep::new(PAGE, "gaming-changes", "Choose changes", "Select the settings you want and enable System Protection if it is off. Toolboks checks for a new restore point before applying changes."),
            TutorialStep::new(PAGE, "gaming-activity", "Watch the activity log", "The log shows the registry export location, changes applied, and verification results."),
            TutorialStep::new(PAGE, "gaming-guides", "Follow guided settings", "Open Windows and NVIDIA settings from this list. Reverse these changes in their original apps."),
            TutorialStep::new(PAGE, "gaming-changes", "Restore earlier settings", "Use Restore for settings Toolboks applied and Open backup location to view the saved registry export."),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::Group;

    #[test]
    fn every_choice_explains_itself() {
        for choice in CHOICES {
            assert!(!choice.why.trim().is_empty(), "{} has no why", choice.id);
            assert!(
                !choice.source.trim().is_empty(),
                "{} has no source",
                choice.id
            );
        }
    }

    #[test]
    fn choice_ids_are_unique() {
        let ids: BTreeSet<_> = CHOICES.iter().map(|choice| choice.id).collect();
        assert_eq!(ids.len(), CHOICES.len());
    }

    #[test]
    fn unproven_choices_exist_and_nothing_is_preselected() {
        assert!(CHOICES
            .iter()
            .any(|choice| choice.evidence == Some(Evidence::Unproven)));
        assert!(saved_selection(&BTreeMap::new()).is_empty());
    }

    #[test]
    fn only_performance_choices_claim_speed() {
        for choice in CHOICES {
            assert_eq!(
                choice.evidence.is_some(),
                choice.group == Group::Performance,
                "{}",
                choice.id
            );
        }
    }

    #[test]
    fn saved_choices_are_restored_and_unknown_ids_dropped() {
        let mut preferences = BTreeMap::new();
        preferences.insert("selected".to_string(), serde_json::json!(["power", "nope"]));
        assert_eq!(saved_selection(&preferences), BTreeSet::from(["power"]));
    }

    #[test]
    fn measured_or_mixed_needs_a_named_source() {
        for choice in CHOICES {
            if matches!(choice.evidence, Some(Evidence::Measured | Evidence::Mixed)) {
                assert!(
                    !choice.source.contains("No controlled measurement"),
                    "{} claims evidence without a source",
                    choice.id
                );
            }
        }
    }

    fn ids(list: &[&'static str]) -> BTreeSet<&'static str> {
        list.iter().copied().collect()
    }

    fn reviewed(key: &ReviewKey) -> Review {
        apply_review(key, Vec::new())
    }

    #[test]
    fn a_review_of_the_current_apply_selection_unlocks_apply() {
        let key = review_key(false, &ids(&["power"]), false);
        assert!(!can_apply(false, &key, None));
        let review = reviewed(&key);
        assert!(can_apply(false, &key, Some(&review)));
        assert!(!can_apply(true, &key, Some(&review)));
    }

    #[test]
    fn changing_the_selection_or_protection_invalidates_the_review() {
        let before = review_key(false, &ids(&["power"]), false);
        let review = reviewed(&before);
        for after in [
            review_key(false, &ids(&["power", "location"]), false),
            review_key(false, &ids(&[]), false),
            review_key(false, &ids(&["power"]), true),
        ] {
            assert!(!can_apply(false, &after, Some(&review)));
            assert!(current_review(Some(reviewed(&before)), &after).is_none());
        }
        assert!(current_review(Some(reviewed(&before)), &before).is_some());
    }

    #[test]
    fn selection_order_does_not_change_the_key() {
        assert_eq!(
            review_key(false, &ids(&["power", "location"]), false),
            review_key(false, &ids(&["location", "power"]), false)
        );
    }

    #[test]
    fn a_recovery_review_never_enables_apply() {
        let recovery = review_key(true, &ids(&["power"]), false);
        let review = Review {
            key: recovery.clone(),
            groups: Vec::new(),
        };
        assert!(!can_apply(false, &recovery, Some(&review)));
        let apply = review_key(false, &ids(&["power"]), false);
        assert!(!can_apply(false, &apply, Some(&review)));
        assert!(current_review(Some(review), &apply).is_none());
    }

    #[test]
    fn protection_alone_can_be_reviewed_then_applied() {
        let key = review_key(false, &ids(&[]), true);
        assert!(!can_apply(false, &key, None));
        let review = reviewed(&key);
        assert_eq!(review.groups.len(), 1);
        assert_eq!(review.groups[0].0, "System Protection");
        assert!(can_apply(false, &key, Some(&review)));
        let nothing = review_key(false, &ids(&[]), false);
        assert!(!can_apply(false, &nothing, Some(&reviewed(&nothing))));
    }

    #[test]
    fn protection_is_listed_before_the_choices() {
        let key = review_key(false, &ids(&["power"]), true);
        let review = apply_review(
            &key,
            vec![("Activate Ultimate Performance", vec!["x".into()])],
        );
        let titles: Vec<_> = review
            .groups
            .iter()
            .map(|(title, _)| title.as_str())
            .collect();
        assert_eq!(
            titles,
            ["System Protection", "Activate Ultimate Performance"]
        );
        let key = review_key(false, &ids(&["power"]), false);
        assert_eq!(apply_review(&key, Vec::new()).groups.len(), 0);
    }

    #[test]
    fn a_saved_selection_starts_unreviewed() {
        let mut preferences = BTreeMap::new();
        preferences.insert("selected".to_string(), serde_json::json!(["power"]));
        let key = review_key(false, &saved_selection(&preferences), false);
        assert!(Gaming::default().preview.is_none());
        assert!(!can_apply(false, &key, Gaming::default().preview.as_ref()));
    }
}
