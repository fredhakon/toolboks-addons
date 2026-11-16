//! The Boot page of Event Watch: how long each boot took and which apps, services and drivers
//! Windows named as slow. The numbers come from `boot-perf` (see its docs for the source events);
//! this file only reads them and says them in plain words.
//!
//! A log that cannot be read, is missing, or is empty is always shown as exactly that, never as a
//! fast boot.

use std::sync::mpsc::{self, Receiver};

use boot_perf::{
    duration_text, median_boot_ms, summarize, Access, Boot, BootRead, SlowSummary, WinBootSource,
};
use egui::{self, Align, Layout, RichText, Sense, Vec2};
use toolboks_shared::ui::{self as look, AMBER, GAP, TEXT_BODY, TEXT_SECTION};

use super::{ms_to_text, muted, outline_button, tag_pill, DAY_MS, LOOKBACK_MS, ROW_H};

const DATE_W: f32 = 132.0;
const VALUE_W: f32 = 78.0;
const TAG_W: f32 = 112.0;
const BAR_H: f32 = 10.0;

pub struct BootView {
    rx: Receiver<BootRead>,
    tx: mpsc::Sender<BootRead>,
    started: bool,
    loading: bool,
    read: Option<BootRead>,
    slow: Vec<SlowSummary>,
}

impl BootView {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            rx,
            tx,
            started: false,
            loading: false,
            read: None,
            slow: Vec::new(),
        }
    }

    fn refresh(&mut self, ctx: &egui::Context) {
        if self.loading {
            return;
        }
        self.loading = true;
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(boot_perf::load(&WinBootSource, LOOKBACK_MS));
            ctx.request_repaint();
        });
    }

    fn drain(&mut self) {
        while let Ok(read) = self.rx.try_recv() {
            self.slow = summarize(&read.log.slow);
            self.read = Some(read);
            self.loading = false;
        }
    }

    /// Header line: refresh and what the page covers.
    pub fn header(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        self.drain();
        if !self.started {
            self.started = true;
            self.refresh(ctx);
        }
        ui.horizontal(|ui| {
            if self.loading {
                ui.label(muted("Reading the Windows boot log..."));
            } else if outline_button(ui, "Refresh").clicked() {
                self.refresh(ctx);
            }
            ui.label(muted(coverage_text(self.read.as_ref())));
        });
        ui.add_space(GAP);
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let Some(read) = &self.read else {
            return;
        };
        if let Some((title, body)) = problem_text(read) {
            look::card(ui, ui.available_width(), 0.0, |ui| {
                ui.label(RichText::new(title).strong().size(TEXT_SECTION));
                ui.add(egui::Label::new(muted(body)).wrap());
            });
            return;
        }
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = look::SPACE_XS;
                section_boots(ui, read);
                ui.add_space(look::SPACE_L);
                section_slow(ui, &self.slow);
                ui.add_space(look::SPACE_L);
                ui.label(muted(
                    "Boot times and slow items are what Windows itself measured and wrote to its log. Event Watch only reads them.",
                ));
            });
    }
}

// ---------------------------------------------------------------------------------------------
// Drawing

fn section_boots(ui: &mut egui::Ui, read: &BootRead) {
    ui.label(RichText::new("Boot time").strong().size(TEXT_SECTION));
    let boots = &read.log.boots;
    if boots.is_empty() {
        ui.label(muted(
            "Windows logged no boot time in this period. That does not mean your boots were fast.",
        ));
        return;
    }
    ui.label(muted(boots_summary(boots)));
    let max = boots.iter().map(|b| b.boot_ms).max().unwrap_or(1).max(1);
    look::card(ui, ui.available_width(), 0.0, |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        for (i, b) in boots.iter().enumerate() {
            if i > 0 {
                hairline_gap(ui);
            }
            boot_row(ui, b, max);
        }
    });
    ui.label(muted(
        "Times are UTC. Windows decides which boots it writes down, so a boot with no entry here was not necessarily fast.",
    ));
    for line in read_notes(read) {
        ui.label(muted(line));
    }
}

fn hairline_gap(ui: &mut egui::Ui) {
    ui.add(egui::Separator::default().spacing(0.0));
}

fn boot_row(ui: &mut egui::Ui, b: &Boot, max: u64) {
    ui.horizontal(|ui| {
        ui.set_min_height(ROW_H);
        ui.spacing_mut().item_spacing.x = GAP;
        ui.add_sized(
            [DATE_W, ROW_H],
            egui::Label::new(muted(ms_to_text(b.logged_ms))),
        );
        let bar_w = (ui.available_width() - VALUE_W - TAG_W - 2.0 * GAP).max(40.0);
        let (rect, response) = ui.allocate_exact_size(Vec2::new(bar_w, ROW_H), Sense::hover());
        if ui.is_rect_visible(rect) {
            let track = egui::Rect::from_center_size(rect.center(), Vec2::new(bar_w, BAR_H));
            ui.painter().rect_filled(track, 3.0, look::BAR_DIVIDER);
            let w = bar_w * (b.boot_ms as f32 / max as f32).clamp(0.0, 1.0);
            let fill = egui::Rect::from_min_size(track.min, Vec2::new(w.max(2.0), BAR_H));
            let color = if b.slower_than_usual == Some(true) {
                AMBER
            } else {
                look::ACCENT
            };
            ui.painter().rect_filled(fill, 3.0, color);
        }
        response.on_hover_text(boot_hover(b));
        ui.allocate_ui_with_layout(
            Vec2::new(VALUE_W, ROW_H),
            Layout::right_to_left(Align::Center),
            |ui| {
                ui.label(RichText::new(duration_text(b.boot_ms)).size(TEXT_BODY));
            },
        );
        if b.slower_than_usual == Some(true) {
            tag_pill(ui, "Slower than usual", AMBER, TAG_W);
        } else {
            ui.add_space(TAG_W);
        }
    });
}

fn section_slow(ui: &mut egui::Ui, slow: &[SlowSummary]) {
    ui.label(
        RichText::new("Windows named these as slow to start")
            .strong()
            .size(TEXT_SECTION),
    );
    if slow.is_empty() {
        ui.label(muted(
            "Windows named no app, service, driver or device as slow in this period.",
        ));
        return;
    }
    look::card(ui, ui.available_width(), 0.0, |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        for (i, s) in slow.iter().enumerate() {
            if i > 0 {
                ui.add(egui::Separator::default().spacing(GAP));
            }
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&s.label).strong());
                ui.label(muted(format!("({})", s.kind.plain())));
            });
            ui.add(egui::Label::new(muted(slow_line(s))).wrap());
        }
    });
}

// ---------------------------------------------------------------------------------------------
// Words (pure, tested)

/// The sentence under the Refresh button.
fn coverage_text(read: Option<&BootRead>) -> String {
    let Some(read) = read else {
        return String::new();
    };
    if read.access != Access::Readable {
        return String::new();
    }
    match read.log.oldest_ms() {
        Some(oldest) => format!(
            "Windows boot log, entries from {} UTC (up to {} days kept)",
            ms_to_text(oldest),
            LOOKBACK_MS / DAY_MS
        ),
        None => format!("Windows boot log, last {} days", LOOKBACK_MS / DAY_MS),
    }
}

/// A title and body when there is nothing to show but the reason; `None` when the page can show
/// the log. An unreadable or missing log never reads as a fast boot.
fn problem_text(read: &BootRead) -> Option<(String, String)> {
    const NOT_A_RESULT: &str = "This says nothing about how fast your PC starts.";
    match &read.access {
        Access::Denied => Some((
            boot_perf::ADMIN_ONLY.into(),
            format!("Toolboks add-ons do not run as administrator, so Event Watch cannot show boot times here, and it changes no Windows setting to get around that. No boot time is shown because none could be read. {NOT_A_RESULT} {}", boot_perf::ALLOW_HOW),
        )),
        Access::Missing => Some((
            "This PC has no boot timing log".into(),
            format!("Windows has no boot performance log here, so there are no boot times to show. {NOT_A_RESULT}"),
        )),
        Access::Failed(why) => Some((
            "Windows could not read the boot timings".into(),
            format!("{} {NOT_A_RESULT}", sentence(why)),
        )),
        Access::Readable if read.log.is_empty() => Some((
            "Windows logged no boot timings in the last 30 days".into(),
            "The log may be switched off or cleared, or Windows may only write an entry for a boot that was slower than usual. That does not mean your boots were fast.".into(),
        )),
        Access::Readable => None,
    }
}

fn sentence(text: &str) -> String {
    let t = text.trim();
    if t.ends_with('.') {
        t.to_string()
    } else {
        format!("{t}.")
    }
}

/// `Latest 1 min 12 s. Median 54 s. Slowest 2 min 3 s. 7 boots logged.`
fn boots_summary(boots: &[Boot]) -> String {
    let latest = boots.iter().max_by_key(|b| b.logged_ms).map(|b| b.boot_ms);
    let slowest = boots.iter().map(|b| b.boot_ms).max();
    let median = median_boot_ms(boots);
    let (Some(latest), Some(slowest), Some(median)) = (latest, slowest, median) else {
        return String::new();
    };
    format!(
        "Latest {}. Median {}. Slowest {}. {} logged.",
        duration_text(latest),
        duration_text(median),
        duration_text(slowest),
        if boots.len() == 1 {
            "1 boot".to_string()
        } else {
            format!("{} boots", boots.len())
        }
    )
}

fn boot_hover(b: &Boot) -> String {
    let mut parts = vec![format!("Whole boot: {}", duration_text(b.boot_ms))];
    if let Some(v) = b.main_path_ms {
        parts.push(format!("Until the desktop: {}", duration_text(v)));
    }
    if let Some(v) = b.post_boot_ms {
        parts.push(format!("After the desktop: {}", duration_text(v)));
    }
    if let Some(v) = b.startup_apps {
        parts.push(format!("Startup apps: {v}"));
    }
    parts.join("\n")
}

/// Notes about what the read could not cover.
fn read_notes(read: &BootRead) -> Vec<String> {
    let mut notes = Vec::new();
    if read.log.skipped > 0 {
        notes.push(format!(
            "{} left out because Windows' record could not be read.",
            if read.log.skipped == 1 {
                "1 record was".to_string()
            } else {
                format!("{} records were", read.log.skipped)
            }
        ));
    }
    if read.log.incomplete {
        notes.push("The read stopped early, so some entries may be missing.".to_string());
    }
    notes
}

/// What Windows measured for one item, in words.
fn slow_line(s: &SlowSummary) -> String {
    let time = match (s.worst_delay_ms, s.worst_total_ms) {
        (Some(delay), Some(total)) => format!(
            "up to {} longer than Windows expected (took {})",
            duration_text(delay),
            duration_text(total)
        ),
        (Some(delay), None) => format!(
            "up to {} longer than Windows expected",
            duration_text(delay)
        ),
        (None, Some(total)) => format!("took up to {}", duration_text(total)),
        (None, None) => "no time was recorded".to_string(),
    };
    let times = if s.times == 1 {
        "named once".to_string()
    } else {
        format!("named {} times", s.times)
    };
    format!(
        "{time}, {times}, last on {}",
        ms_to_text(s.last_ms).get(..10).unwrap_or("")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use boot_perf::{BootLog, SlowKind};

    fn boot(logged: i64, ms: u64, slow: Option<bool>) -> Boot {
        Boot {
            logged_ms: logged,
            boot_ms: ms,
            main_path_ms: Some(ms / 2),
            post_boot_ms: Some(ms / 2),
            startup_apps: Some(7),
            slower_than_usual: slow,
        }
    }

    fn read(access: Access, log: BootLog) -> BootRead {
        BootRead { log, access }
    }

    #[test]
    fn an_unreadable_log_says_so_and_never_claims_a_fast_boot() {
        for access in [
            Access::Denied,
            Access::Missing,
            Access::Failed("Windows could not read the boot performance log (error 87)".into()),
        ] {
            let (title, body) = problem_text(&read(access, BootLog::default())).unwrap();
            assert!(!title.to_lowercase().contains("fast"), "{title}");
            assert!(body.contains("nothing about how fast"), "{body}");
        }
        let (title, body) = problem_text(&read(Access::Denied, BootLog::default())).unwrap();
        assert_eq!(
            title,
            "Windows only lets administrators read the boot log on this PC"
        );
        assert!(body.contains(boot_perf::ALLOW_HOW), "{body}");
        assert!(
            body.contains("never changes that Windows setting"),
            "{body}"
        );
    }

    #[test]
    fn an_empty_readable_log_does_not_read_as_fast() {
        let (title, body) = problem_text(&read(Access::Readable, BootLog::default())).unwrap();
        assert!(title.contains("no boot timings"));
        assert!(body.contains("does not mean your boots were fast"));
    }

    #[test]
    fn a_log_with_data_has_no_problem_text() {
        let log = BootLog {
            boots: vec![boot(1, 60_000, None)],
            ..Default::default()
        };
        assert!(problem_text(&read(Access::Readable, log)).is_none());
    }

    #[test]
    fn summary_line() {
        let boots = [
            boot(300, 72_000, Some(true)),
            boot(200, 50_000, None),
            boot(100, 130_000, None),
        ];
        assert_eq!(
            boots_summary(&boots),
            "Latest 1 min 12 s. Median 1 min 12 s. Slowest 2 min 10 s. 3 boots logged."
        );
        assert_eq!(
            boots_summary(&boots[..1]),
            "Latest 1 min 12 s. Median 1 min 12 s. Slowest 1 min 12 s. 1 boot logged."
        );
        assert_eq!(boots_summary(&[]), "");
    }

    #[test]
    fn slow_line_words() {
        let s = |delay, total, times| SlowSummary {
            kind: SlowKind::App,
            label: "x".into(),
            name: "x.exe".into(),
            friendly_name: None,
            path: None,
            times,
            worst_total_ms: total,
            worst_delay_ms: delay,
            last_ms: 1_762_761_600_000,
        };
        assert_eq!(
            slow_line(&s(Some(14_000), Some(21_000), 3)),
            "up to 14 s longer than Windows expected (took 21 s), named 3 times, last on 2025-11-10"
        );
        assert_eq!(
            slow_line(&s(None, Some(9_000), 1)),
            "took up to 9.0 s, named once, last on 2025-11-10"
        );
        assert_eq!(
            slow_line(&s(None, None, 1)),
            "no time was recorded, named once, last on 2025-11-10"
        );
    }

    #[test]
    fn notes_and_coverage() {
        let mut r = read(
            Access::Readable,
            BootLog {
                boots: vec![boot(1_762_761_600_000, 60_000, None)],
                skipped: 2,
                incomplete: true,
                ..Default::default()
            },
        );
        let notes = read_notes(&r);
        assert_eq!(notes.len(), 2);
        assert!(notes[0].starts_with("2 records were left out"));
        assert!(coverage_text(Some(&r)).contains("2025-11-10"));
        r.access = Access::Denied;
        assert_eq!(coverage_text(Some(&r)), "");
        assert_eq!(coverage_text(None), "");
    }

    #[test]
    fn the_page_draws_every_state_without_panicking() {
        let ctx = egui::Context::default();
        let boots: Vec<Boot> = (0..40)
            .map(|i| {
                boot(
                    1_762_000_000_000 + i * 86_400_000,
                    40_000 + i as u64 * 1_000,
                    (i % 3 == 0).then_some(true),
                )
            })
            .collect();
        let slow = vec![SlowSummary {
            kind: SlowKind::Service,
            label: "Windows Search".into(),
            name: "WSearch".into(),
            friendly_name: None,
            path: None,
            times: 2,
            worst_total_ms: Some(20_000),
            worst_delay_ms: None,
            last_ms: 1_762_000_000_000,
        }];
        let states = [
            read(Access::Denied, BootLog::default()),
            read(Access::Missing, BootLog::default()),
            read(Access::Failed("boom".into()), BootLog::default()),
            read(Access::Readable, BootLog::default()),
            read(
                Access::Readable,
                BootLog {
                    boots,
                    slow: Vec::new(),
                    ..Default::default()
                },
            ),
        ];
        for state in states {
            let mut view = BootView::new();
            view.started = true;
            view.read = Some(state);
            view.slow = slow.clone();
            for _ in 0..2 {
                let mut output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(760.0, 600.0),
                        )),
                        ..Default::default()
                    },
                    |ui| {
                        view.header(ui, &ui.ctx().clone());
                        view.ui(ui);
                    },
                );
                output.textures_delta.clear();
            }
        }
    }
}
