//! The Latency check page: an opt-in, short kernel recording through the administrator helper
//! that names the drivers with the longest DPC and ISR runs (Gusseti/toolboks#59).
//!
//! It never starts by itself. The drawing process only sends the chosen length; the helper
//! (`admin.rs`) does the rest after the person confirms in its own window. The result is held in
//! memory only and is gone when Event Watch closes.

use std::time::{Duration, Instant};

use egui::{self, RichText};
use toolboks_addon_sdk::{AdminOutcome, AdminRequest, Capability, HostRequest};
use toolboks_shared::ui::{self as look, AMBER, GAP, MUTED, RED, TEXT_CAPTION, TEXT_SECTION};

use crate::admin::{self, DriverLine, LatencyResult, TraceRequest, DURATIONS};
use crate::page::{muted, outline_button, primary_button};

/// What the page says after a run that produced no ranking.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Note {
    Info(String),
    Error(String),
}

pub struct Latency {
    /// The administrator capability was granted at install or update.
    allowed: bool,
    seconds: u32,
    next_id: u64,
    /// Waiting for the helper: (request id, when it was asked, length).
    pending: Option<(u64, Instant, u32)>,
    request: Option<HostRequest>,
    result: Option<LatencyResult>,
    note: Option<Note>,
}

impl Default for Latency {
    fn default() -> Self {
        Self {
            allowed: false,
            seconds: DURATIONS[0],
            next_id: 0,
            pending: None,
            request: None,
            result: None,
            note: None,
        }
    }
}

/// `412.0 µs`, `1.25 ms` (the same form as the explanation text).
fn duration_text(ns: u64) -> String {
    let us = ns as f64 / 1000.0;
    if us < 1000.0 {
        format!("{us:.1} µs")
    } else if us < 1_000_000.0 {
        format!("{:.2} ms", us / 1000.0)
    } else {
        format!("{:.1} s", us / 1_000_000.0)
    }
}

fn runs_text(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The second line under a driver: how often it ran and when its longest run was.
fn driver_detail(d: &DriverLine) -> String {
    let mut text = format!(
        "{}, {}.",
        runs_text(d.dpc_runs, "DPC run", "DPC runs"),
        runs_text(d.isr_runs, "ISR run", "ISR runs")
    );
    if let Some(w) = d.worst.first() {
        text.push_str(&format!(
            " Longest at {:.3} s on CPU {} ({}).",
            w.at_ms as f64 / 1000.0,
            w.cpu,
            w.kind
        ));
    }
    text
}

impl Latency {
    pub fn set_capabilities(&mut self, capabilities: &[Capability]) {
        self.allowed = capabilities.contains(&Capability::Administrator);
    }

    pub fn take_request(&mut self) -> Option<HostRequest> {
        self.request.take()
    }

    pub fn busy(&self) -> bool {
        self.pending.is_some()
    }

    /// Asks the host for the administrator helper. Nothing runs until the person confirms there.
    fn start(&mut self) {
        if self.busy() || !self.allowed || !DURATIONS.contains(&self.seconds) {
            return;
        }
        self.next_id = self.next_id.wrapping_add(1);
        self.pending = Some((self.next_id, Instant::now(), self.seconds));
        self.note = None;
        self.request = Some(HostRequest::AdminOperation(AdminRequest {
            id: self.next_id,
            operation: admin::OPERATION.into(),
            json: TraceRequest {
                seconds: self.seconds,
            }
            .to_json(),
        }));
    }

    /// The helper's answer. Informational only: it authorizes nothing.
    pub fn admin_result(&mut self, id: u64, outcome: AdminOutcome) {
        if self.pending.map(|(pending, _, _)| pending) != Some(id) {
            return;
        }
        self.pending = None;
        match outcome {
            AdminOutcome::Cancelled => {
                self.note = Some(Note::Info("Cancelled. Nothing was recorded.".into()))
            }
            AdminOutcome::Failed { reason } => {
                self.note = Some(Note::Error(reason.chars().take(1000).collect()))
            }
            AdminOutcome::Completed { json } => match LatencyResult::parse(&json) {
                Ok(result) => {
                    self.result = Some(result);
                    self.note = None;
                }
                Err(e) => self.note = Some(Note::Error(e)),
            },
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        if self.pending.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }
        let side = egui::Frame::NONE.inner_margin(look::CARD_PAD);
        egui::CentralPanel::default().frame(side).show(ui, |ui| {
            toolboks_addon_sdk::tutorial::region(ui, "table", |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| self.body(ui));
            });
        });
    }

    fn body(&mut self, ui: &mut egui::Ui) {
        ui.spacing_mut().item_spacing.y = look::SPACE_XS;
        ui.label(
            RichText::new("Find drivers that hold up the processor")
                .strong()
                .size(TEXT_SECTION),
        );
        ui.add(
            egui::Label::new(muted(
                "Windows records for a short time how long each driver's interrupt work runs. \
                 Long runs can cause stutter, audio crackle or input lag. Run it while the \
                 problem is happening.",
            ))
            .wrap(),
        );
        ui.add_space(GAP);

        if !self.allowed {
            look::card(ui, ui.available_width(), 0.0, |ui| {
                ui.label(RichText::new("The latency check is turned off").strong());
                ui.add(
                    egui::Label::new(muted(
                        "This version of Event Watch was installed without administrator access. \
                         Review the Event Watch update to allow the latency check. You can still \
                         say no; the rest of Event Watch works without it.",
                    ))
                    .wrap(),
                );
                ui.add_space(look::SPACE_XS);
                if outline_button(ui, "Review Event Watch update").clicked() {
                    self.request = Some(HostRequest::ReviewUpdate);
                }
            });
            return;
        }

        self.start_card(ui);

        if let Some(note) = &self.note {
            ui.add_space(GAP);
            let (text, color) = match note {
                Note::Info(t) => (t.as_str(), MUTED),
                Note::Error(t) => (t.as_str(), RED),
            };
            ui.add(egui::Label::new(RichText::new(text).color(color)).wrap());
        }

        if let Some(result) = &self.result {
            ui.add_space(look::SPACE_L);
            result_view(ui, result);
        }
    }

    fn start_card(&mut self, ui: &mut egui::Ui) {
        look::card(ui, ui.available_width(), 0.0, |ui| {
            ui.spacing_mut().item_spacing.y = look::SPACE_XS;
            ui.label(RichText::new("Before you start").strong());
            for line in [
                "It needs administrator rights. Windows asks you to confirm each time; it never \
                 runs on its own."
                    .to_string(),
                format!(
                    "It records for about {} seconds, then reads the recording. The processor \
                     is a little busier while it records.",
                    self.seconds
                ),
                "Nothing is kept. The recording is deleted as soon as it has been read, and the \
                 result stays only until you close Event Watch."
                    .to_string(),
            ] {
                ui.add(egui::Label::new(muted(line)).wrap());
            }
            ui.add_space(look::SPACE_XS);
            ui.horizontal(|ui| {
                ui.label(muted("Length"));
                for seconds in DURATIONS {
                    let selected = self.seconds == seconds;
                    let response = ui
                        .add_enabled_ui(!self.busy(), |ui| {
                            look::pill(ui, &format!("{seconds} s"), selected)
                        })
                        .inner;
                    if response.clicked() {
                        self.seconds = seconds;
                    }
                }
            });
            ui.add_space(look::SPACE_XS);
            match self.pending {
                Some((_, since, seconds)) => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(muted(pending_text(since.elapsed(), seconds)));
                    });
                }
                None => {
                    let label = if self.result.is_some() {
                        "Run the latency check again"
                    } else {
                        "Start latency check"
                    };
                    if primary_button(ui, label).clicked() {
                        self.start();
                    }
                }
            }
        });
    }
}

/// The line shown while waiting for the helper. The confirmation window comes first, so the time
/// is only a rough guide.
fn pending_text(elapsed: Duration, seconds: u32) -> String {
    format!(
        "Confirm in the administrator window. Then Windows records for {seconds} seconds and \
         reads the recording. Waiting {} s.",
        elapsed.as_secs()
    )
}

fn result_view(ui: &mut egui::Ui, r: &LatencyResult) {
    ui.label(
        RichText::new("Drivers with the longest runs")
            .strong()
            .size(TEXT_SECTION),
    );
    let coverage = r
        .coverage_percent
        .map(|p| format!(" {p:.0}% of the measured time was matched to a driver."))
        .unwrap_or_default();
    ui.add(
        egui::Label::new(muted(format!(
            "Recorded {:.1} s: {} and {}.{coverage}",
            r.recorded_ms as f64 / 1000.0,
            runs_text(r.dpc_runs, "DPC run", "DPC runs"),
            runs_text(r.isr_runs, "ISR run", "ISR runs"),
        )))
        .wrap(),
    );
    ui.add_space(look::SPACE_XS);
    if r.drivers.is_empty() {
        ui.label(muted("No driver could be named from this recording."));
    } else {
        look::card(ui, ui.available_width(), 0.0, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for (i, d) in r.drivers.iter().enumerate() {
                if i > 0 {
                    ui.add(egui::Separator::default().spacing(GAP));
                } else {
                    ui.add_space(look::SPACE_XS);
                }
                driver_row(ui, i, d);
            }
            ui.add_space(look::SPACE_XS);
        });
        if r.drivers_not_shown > 0 {
            ui.label(muted(format!(
                "{} with shorter runs not listed.",
                runs_text(r.drivers_not_shown as u64, "more driver", "more drivers")
            )));
        }
    }
    if let Some(warning) = &r.cleanup_warning {
        ui.add_space(GAP);
        ui.add(egui::Label::new(RichText::new(warning).size(TEXT_CAPTION).color(AMBER)).wrap());
    }

    ui.add_space(look::SPACE_L);
    ui.label(RichText::new("What this means").strong().size(TEXT_SECTION));
    ui.add(egui::Label::new(muted(r.summary.trim_end())).wrap());
}

fn driver_row(ui: &mut egui::Ui, index: usize, d: &DriverLine) {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(format!("{}. {}", index + 1, d.name)).strong());
        let elapsed = d
            .elapsed_ns
            .map(|e| format!(" (elapsed {})", duration_text(e)))
            .unwrap_or_default();
        ui.label(format!(
            "longest run {}{elapsed}",
            duration_text(d.longest_ns)
        ));
        if d.above_guideline {
            ui.label(
                RichText::new("Longer than Microsoft's guidance")
                    .size(TEXT_CAPTION)
                    .color(AMBER),
            );
        }
    });
    ui.add(egui::Label::new(muted(driver_detail(d))).wrap());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completed(page: &mut Latency) -> u64 {
        page.set_capabilities(&[Capability::Administrator]);
        page.start();
        match page.take_request() {
            Some(HostRequest::AdminOperation(r)) => {
                assert_eq!(r.operation, "latency_trace");
                assert_eq!(TraceRequest::parse(&r.json).unwrap().seconds, page.seconds);
                r.id
            }
            _ => panic!("no administrator request"),
        }
    }

    #[test]
    fn nothing_is_requested_without_the_capability_or_a_click() {
        let mut page = Latency::default();
        assert!(page.take_request().is_none());
        page.start();
        assert!(
            page.take_request().is_none(),
            "started without the capability"
        );
        page.set_capabilities(&[Capability::LocalPreferences]);
        page.start();
        assert!(page.take_request().is_none());
    }

    #[test]
    fn one_request_at_a_time_and_only_its_answer_counts() {
        let mut page = Latency {
            seconds: 20,
            ..Latency::default()
        };
        let id = completed(&mut page);
        page.start();
        assert!(
            page.take_request().is_none(),
            "a second run started while one is open"
        );
        // A stale or forged answer for another id is ignored.
        page.admin_result(id + 7, AdminOutcome::Cancelled);
        assert!(page.busy());
        page.admin_result(id, AdminOutcome::Cancelled);
        assert!(!page.busy());
        assert_eq!(
            page.note,
            Some(Note::Info("Cancelled. Nothing was recorded.".into()))
        );
        assert!(page.result.is_none());
    }

    #[test]
    fn failures_and_unreadable_results_are_shown_as_errors() {
        let mut page = Latency::default();
        let id = completed(&mut page);
        page.admin_result(
            id,
            AdminOutcome::Failed {
                reason: "Another program is already recording".into(),
            },
        );
        assert!(matches!(page.note, Some(Note::Error(_))));
        let id = completed(&mut page);
        page.admin_result(id, AdminOutcome::Completed { json: "{}".into() });
        assert!(matches!(page.note, Some(Note::Error(_))));
        assert!(page.result.is_none());
    }

    #[test]
    fn durations_read_like_the_explanation() {
        assert_eq!(duration_text(412_000), "412.0 µs");
        assert_eq!(duration_text(1_250_000), "1.25 ms");
    }
}
