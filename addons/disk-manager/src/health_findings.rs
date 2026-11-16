//! The drive-health check as a plain reading, ready to be reported to Home as a finding.
//!
//! Everything here is pure: a drive letter and what [`crate::health`] read for it go in, a
//! [`Reading`] comes out. `Reading` has exactly the fields of the host's `Finding`
//! (`docs/PLAN-health.md` section 1), so `From<Reading> for Finding` is the whole protocol
//! hookup. Text is plain words: no markup, no control characters, within the
//! host's limits (the host cuts anything longer).
//!
//! Cadence: read every fixed drive at start and again every [`REFRESH_EVERY_SECS`]; a reading
//! is out of date after [`STALE_AFTER_SECS`]. [`Reporter`] runs that cycle in the background and
//! hands Home the findings.

use crate::health::{self, Cache, Health, ReadError, ReadFn, Report, Verdict};
use std::sync::{Arc, Mutex};
use toolboks_addon_sdk::Finding;

/// The id prefix; the drive letter in lower case follows ("drive-health-c").
const ID_PREFIX: &str = "drive-health";
/// Read every fixed drive again after this long (6 hours).
pub const REFRESH_EVERY_SECS: u64 = 6 * 60 * 60;
/// A reading is out of date after this long (24 hours).
pub const STALE_AFTER_SECS: u32 = 24 * 60 * 60;

#[cfg(test)]
pub const ID_MAX: usize = 48;
pub const TITLE_MAX: usize = 80;
pub const OBSERVATION_MAX: usize = 240;
pub const LABEL_MAX: usize = 40;

const ACTION_LABEL: &str = "Open drive health";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ok,
    Attention,
    NotChecked,
    Unavailable,
}

/// Only used for ranking when the status is `Attention`; `Watch` otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Watch,
    Problem,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    OpenPage { label: String, route: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reading {
    pub id: String,
    pub title: String,
    pub observation: String,
    pub status: Status,
    pub severity: Severity,
    /// Unix seconds.
    pub measured_at: u64,
    pub stale_after_secs: u32,
    pub action: Option<Action>,
}

/// The drive letter in the form the Optimization page uses ("C:"), or `None` when `raw` is not
/// a single letter A to Z with an optional colon ("c", "C:").
pub fn normalize_letter(raw: &str) -> Option<String> {
    let mut chars = raw.trim().chars();
    let letter = chars.next().filter(char::is_ascii_alphabetic)?;
    match chars.as_str() {
        "" | ":" => Some(format!("{}:", letter.to_ascii_uppercase())),
        _ => None,
    }
}

/// The drives to read: valid letters only, upper case, each once, in letter order.
pub fn drives_to_check<S: AsRef<str>>(fixed: &[S]) -> Vec<String> {
    let mut out: Vec<String> = fixed
        .iter()
        .filter_map(|d| normalize_letter(d.as_ref()))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// True when the drives should be read now: at start (`last` is `None`), when the last read
/// is at least [`REFRESH_EVERY_SECS`] old, or when the clock went backwards.
pub fn should_refresh(now: u64, last: Option<u64>) -> bool {
    match last {
        None => true,
        Some(last) => now < last || now - last >= REFRESH_EVERY_SECS,
    }
}

/// Removes control characters and the characters that open markup (`<`, `>`, backtick), folds
/// runs of white space into one space, trims and cuts to `max` characters.
fn clean(text: &str, max: usize) -> String {
    let folded: String = text
        .chars()
        .filter(|c| !matches!(c, '<' | '>' | '`'))
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    folded
        .chars()
        .take(max)
        .collect::<String>()
        .trim_end()
        .to_string()
}

fn id_for(letter: &str) -> String {
    match normalize_letter(letter) {
        Some(l) => format!("{ID_PREFIX}-{}", l[..1].to_ascii_lowercase()),
        None => ID_PREFIX.to_string(),
    }
}

/// The reading for one drive from what was read for it. `letter` is "C:" or "C"; `now` is Unix
/// seconds.
pub fn check(letter: &str, read: &Result<Report, ReadError>, now: u64) -> Reading {
    let shown = normalize_letter(letter).unwrap_or_else(|| "This drive".to_string());
    let (status, severity, observation) = match read {
        Ok(report) => {
            let (verdict, reason) = crate::health::assess(&report.health);
            // Windows' own rating says in its first words whose rating it is; a drive's own
            // values get the verdict word in front.
            let direct = matches!(report.health, Health::Drive(_));
            let say = |word: &str| {
                if direct {
                    format!("{word}. {reason}")
                } else {
                    reason.clone()
                }
            };
            match verdict {
                Verdict::Healthy => (Status::Ok, Severity::Watch, say("Healthy")),
                Verdict::Watch => (Status::Attention, Severity::Watch, say("Watch")),
                Verdict::ReplaceSoon => (Status::Attention, Severity::Problem, say("Replace soon")),
                Verdict::Problem => (Status::Attention, Severity::Problem, say("Problem")),
            }
        }
        Err(ReadError::AccessDenied) => (
            Status::Unavailable,
            Severity::Watch,
            "Needs administrator rights. This drive only gives its health data to administrators."
                .to_string(),
        ),
        Err(ReadError::Unsupported) => (
            Status::Unavailable,
            Severity::Watch,
            "This drive does not report health. USB, memory card and virtual drives often do not."
                .to_string(),
        ),
        Err(ReadError::NotFound) => (
            Status::Unavailable,
            Severity::Watch,
            "This drive letter is not on a single physical drive Disk Manager can read."
                .to_string(),
        ),
        Err(ReadError::Other(e)) => (
            Status::Unavailable,
            Severity::Watch,
            format!("Drive health could not be read: {}", clean(e, 120)),
        ),
    };
    reading(letter, &shown, status, severity, &observation, now)
}

/// The reading while the first read of a drive is still running.
pub fn not_checked(letter: &str, now: u64) -> Reading {
    let shown = normalize_letter(letter).unwrap_or_else(|| "This drive".to_string());
    reading(
        letter,
        &shown,
        Status::NotChecked,
        Severity::Watch,
        "Drive health has not been read yet.",
        now,
    )
}

fn reading(
    letter: &str,
    shown: &str,
    status: Status,
    severity: Severity,
    observation: &str,
    now: u64,
) -> Reading {
    let action = normalize_letter(letter).map(|l| Action::OpenPage {
        label: clean(ACTION_LABEL, LABEL_MAX),
        route: format!("Optimization/{l}"),
    });
    Reading {
        id: id_for(letter),
        title: clean(&format!("Drive {shown} health"), TITLE_MAX),
        observation: clean(observation, OBSERVATION_MAX),
        status,
        severity,
        measured_at: now,
        stale_after_secs: STALE_AFTER_SECS,
        action,
    }
}

/// At most this many drives are reported (the host keeps 16 findings per add-on).
pub const MAX_DRIVES: usize = 16;

impl From<Reading> for Finding {
    fn from(r: Reading) -> Self {
        let finding = match r.status {
            Status::Ok => Finding::ok(r.id, r.title, r.observation),
            Status::Attention => {
                let severity = match r.severity {
                    Severity::Watch => toolboks_addon_sdk::Severity::Watch,
                    Severity::Problem => toolboks_addon_sdk::Severity::Problem,
                };
                Finding::attention(r.id, r.title, r.observation, severity)
            }
            Status::NotChecked => Finding::not_checked(r.id, r.title, r.observation),
            Status::Unavailable => Finding::unavailable(r.id, r.title, r.observation),
        };
        let finding = finding
            .stale_after(r.stale_after_secs)
            .measured_at(r.measured_at);
        match r.action {
            Some(Action::OpenPage { label, route }) => finding.open_page(label, route),
            None => finding,
        }
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The fixed drives (hard disks and SSDs) as "C:", in letter order. Cheap, but called from a
/// background thread like the reads.
fn fixed_drives() -> Vec<String> {
    #[cfg(windows)]
    {
        use windows::{
            core::PCWSTR,
            Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives},
        };
        const DRIVE_FIXED: u32 = 3;
        let mask = unsafe { GetLogicalDrives() };
        (0..26u8)
            .filter(|i| mask & (1 << i) != 0)
            .map(|i| format!("{}:", (b'A' + i) as char))
            .filter(|letter| {
                let root: Vec<u16> = format!("{letter}\\")
                    .encode_utf16()
                    .chain(Some(0))
                    .collect();
                (unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) }) == DRIVE_FIXED
            })
            .collect()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

fn no_drives() -> Vec<String> {
    Vec::new()
}

#[derive(Default)]
struct Cycle {
    /// The drives reported, once the first listing finished. Empty until then.
    drives: Vec<String>,
    /// Unix seconds when the last cycle started; `None` until the first.
    last: Option<u64>,
    /// A listing thread is running.
    listing: bool,
    /// The cache generation the last cycle saw; a different one means the Optimization page's
    /// "Refresh drive information" cleared the readings.
    generation: u64,
}

/// Reads drive health in the background and reports one finding per fixed drive.
///
/// It shares the Optimization page's [`Cache`], so a drive is read once per cycle whoever asks.
/// Nothing here blocks: [`Reporter::poll`] only starts threads, [`Reporter::findings`] only
/// copies what is stored.
pub struct Reporter {
    cache: Cache,
    cycle: Arc<Mutex<Cycle>>,
    read: ReadFn,
    list: fn() -> Vec<String>,
}

impl Reporter {
    pub fn new(cache: Cache) -> Self {
        // Unit tests of the add-on must not read the drives of the PC they run on.
        let list = if cfg!(test) { no_drives } else { fixed_drives };
        Self::with(cache, health::report, list)
    }

    /// With the read and the drive list replaced, for tests.
    pub fn with(cache: Cache, read: ReadFn, list: fn() -> Vec<String>) -> Self {
        Self {
            cache,
            cycle: Default::default(),
            read,
            list,
        }
    }

    /// Starts a cycle when one is due: at start, every [`REFRESH_EVERY_SECS`], and when the
    /// readings were cleared. Cheap; call it on every tick and frame.
    pub fn poll(&self) {
        self.poll_at(unix_now());
    }

    pub fn poll_at(&self, now: u64) {
        let generation = health::generation(&self.cache);
        let mut cycle = self.cycle.lock().unwrap();
        if cycle.listing || !(should_refresh(now, cycle.last) || cycle.generation != generation) {
            return;
        }
        cycle.listing = true;
        cycle.last = Some(now);
        cycle.generation = generation;
        drop(cycle);
        let (cache, shared, read, list) =
            (self.cache.clone(), self.cycle.clone(), self.read, self.list);
        std::thread::spawn(move || {
            let mut drives = drives_to_check(&list());
            drives.truncate(MAX_DRIVES);
            for drive in &drives {
                health::refresh(&cache, drive, read);
            }
            let mut cycle = shared.lock().unwrap();
            cycle.drives = drives;
            cycle.listing = false;
        });
    }

    /// The user asked Home to check again: read every drive again now.
    pub fn check_now(&self) {
        self.cycle.lock().unwrap().last = None;
        self.poll();
    }

    /// One finding per fixed drive, from what has been read so far. The same until a read
    /// finishes or a cycle lists different drives.
    pub fn findings(&self) -> Vec<Finding> {
        let (drives, started) = {
            let cycle = self.cycle.lock().unwrap();
            (cycle.drives.clone(), cycle.last.unwrap_or(0))
        };
        drives
            .iter()
            .map(|drive| match health::peek(&self.cache, drive) {
                Some((read, at)) => check(drive, &read, at).into(),
                None => not_checked(drive, started).into(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::{parse_ata, parse_nvme, PAGE};

    const NOW: u64 = 1_800_000_000;

    // The same recorded pages as the health.rs tests.
    fn nvme(warning: u8, kelvin: u16, spare: u8, used: u8, media: u64) -> Vec<u8> {
        let mut p = vec![0u8; PAGE];
        p[0] = warning;
        p[1..3].copy_from_slice(&kelvin.to_le_bytes());
        p[3] = spare;
        p[4] = 10;
        p[5] = used;
        p[128..136].copy_from_slice(&8_760u64.to_le_bytes());
        p[160..168].copy_from_slice(&media.to_le_bytes());
        p
    }

    fn ata(attrs: &[(u8, u8, u64)]) -> Vec<u8> {
        let mut p = vec![0u8; PAGE];
        p[0] = 0x10;
        for (i, (id, value, raw)) in attrs.iter().enumerate() {
            let e = &mut p[2 + i * 12..14 + i * 12];
            e[0] = *id;
            e[1] = 0x32;
            e[3] = *value;
            e[4] = *value;
            e[5..11].copy_from_slice(&raw.to_le_bytes()[..6]);
        }
        let sum = p[..511].iter().fold(0u8, |s, b| s.wrapping_add(*b));
        p[511] = sum.wrapping_neg();
        p
    }

    fn report(page: Result<crate::health::DriveHealth, crate::health::ParseError>) -> Report {
        Report {
            model: "Test drive".into(),
            health: Health::Drive(page.unwrap()),
        }
    }

    fn windows_report(health: Option<u16>, operational: &[u16], raid: bool) -> Report {
        use crate::health::{rate, WindowsRating, WindowsStatus};
        let status = WindowsStatus {
            health,
            operational: operational.to_vec(),
        };
        Report {
            model: "Test drive".into(),
            health: Health::Windows(WindowsRating {
                rating: rate(&status).expect("a rating"),
                status,
                raid,
            }),
        }
    }

    fn open_route(r: &Reading) -> &str {
        match r.action.as_ref().expect("an action") {
            Action::OpenPage { route, .. } => route,
        }
    }

    fn assert_limits(r: &Reading) {
        assert!((1..=ID_MAX).contains(&r.id.len()), "{}", r.id);
        assert!(r
            .id
            .chars()
            .all(|c| matches!(c, 'a'..='z' | '0'..='9' | '.' | '-')));
        assert!(!r.title.is_empty() && r.title.chars().count() <= TITLE_MAX);
        assert!(!r.observation.is_empty() && r.observation.chars().count() <= OBSERVATION_MAX);
        for text in [&r.title, &r.observation] {
            assert!(!text.chars().any(char::is_control), "{text:?}");
            assert!(!text.contains(['<', '>', '`', '*', '[', ']']), "{text}");
            assert!(!text.contains("://"), "{text}");
        }
        if let Some(Action::OpenPage { label, route }) = &r.action {
            assert!(!label.is_empty() && label.chars().count() <= LABEL_MAX);
            assert!(route.starts_with("Optimization/"));
        }
        assert_eq!(r.stale_after_secs, 86_400);
        assert_eq!(r.measured_at, NOW);
    }

    #[test]
    fn healthy_nvme_is_ok() {
        let r = check("C:", &Ok(report(parse_nvme(&nvme(0, 311, 100, 3, 0)))), NOW);
        assert_eq!(r.id, "drive-health-c");
        assert_eq!(r.title, "Drive C: health");
        assert_eq!(r.status, Status::Ok);
        assert!(r.observation.starts_with("Healthy."), "{}", r.observation);
        assert!(r.observation.contains("No warning signs"));
        assert_eq!(open_route(&r), "Optimization/C:");
        assert_limits(&r);
    }

    #[test]
    fn healthy_sata_is_ok() {
        let page = ata(&[(5, 100, 0), (9, 99, 12_345), (194, 67, 33), (197, 100, 0)]);
        let r = check("d", &Ok(report(parse_ata(&page))), NOW);
        assert_eq!((r.id.as_str(), r.status), ("drive-health-d", Status::Ok));
        assert_eq!(open_route(&r), "Optimization/D:");
        assert_limits(&r);
    }

    #[test]
    fn watch_is_attention_watch() {
        let r = check(
            "C:",
            &Ok(report(parse_nvme(&nvme(0, 318, 100, 74, 6)))),
            NOW,
        );
        assert_eq!((r.status, r.severity), (Status::Attention, Severity::Watch));
        assert!(r.observation.starts_with("Watch."), "{}", r.observation);
        assert_limits(&r);
        // SATA with a few pending sectors.
        let page = ata(&[(5, 100, 2), (197, 100, 8), (198, 100, 0)]);
        let r = check("E:", &Ok(report(parse_ata(&page))), NOW);
        assert_eq!((r.status, r.severity), (Status::Attention, Severity::Watch));
        assert!(r.observation.contains("reallocated"), "{}", r.observation);
        assert_limits(&r);
    }

    #[test]
    fn replace_soon_is_attention_problem() {
        let r = check(
            "C:",
            &Ok(report(parse_nvme(&nvme(0, 318, 100, 93, 6)))),
            NOW,
        );
        assert_eq!(
            (r.status, r.severity),
            (Status::Attention, Severity::Problem)
        );
        assert!(
            r.observation.starts_with("Replace soon. "),
            "{}",
            r.observation
        );
        assert!(r.observation.contains("93%"));
        assert_limits(&r);
        let page = ata(&[(5, 80, 140), (197, 100, 12)]);
        let r = check("D:", &Ok(report(parse_ata(&page))), NOW);
        assert_eq!(
            (r.status, r.severity),
            (Status::Attention, Severity::Problem)
        );
        assert_limits(&r);
    }

    #[test]
    fn the_worst_verdict_wins_over_the_text_length() {
        // Several findings at once still fit the observation limit.
        let page = ata(&[(5, 80, 140), (197, 100, 12), (231, 8, 0), (194, 20, 75)]);
        let r = check("D:", &Ok(report(parse_ata(&page))), NOW);
        assert_eq!(r.severity, Severity::Problem);
        assert_limits(&r);
    }

    #[test]
    fn administrator_and_unsupported_are_unavailable_with_the_plain_reason() {
        let r = check("C:", &Err(ReadError::AccessDenied), NOW);
        assert_eq!(r.status, Status::Unavailable);
        assert!(r.observation.starts_with("Needs administrator rights"));
        assert_limits(&r);
        let r = check("C:", &Err(ReadError::Unsupported), NOW);
        assert_eq!(r.status, Status::Unavailable);
        assert!(r
            .observation
            .starts_with("This drive does not report health"));
        assert_limits(&r);
        let r = check("C:", &Err(ReadError::NotFound), NOW);
        assert_eq!(r.status, Status::Unavailable);
        assert_limits(&r);
    }

    #[test]
    fn windows_rating_of_a_raid_drive_is_ok_and_says_what_it_is() {
        let r = check("C:", &Ok(windows_report(Some(0), &[2], true)), NOW);
        assert_eq!(r.status, Status::Ok);
        assert_eq!(
            r.observation,
            "Windows rates this drive Healthy. This PC runs its drives in RAID mode, so detailed drive data (wear, temperature, reallocated sectors) is not available."
        );
        assert!(!r.observation.contains("SMART"));
        assert!(!r.observation.starts_with("Healthy."));
        assert_limits(&r);
    }

    #[test]
    fn windows_rating_warning_and_unhealthy_need_attention() {
        let r = check("C:", &Ok(windows_report(Some(1), &[3], true)), NOW);
        assert_eq!((r.status, r.severity), (Status::Attention, Severity::Watch));
        assert!(r
            .observation
            .starts_with("Windows rates this drive Warning."));
        assert!(r.observation.contains("Degraded"));
        assert_limits(&r);
        let r = check("C:", &Ok(windows_report(Some(2), &[13], false)), NOW);
        assert_eq!(
            (r.status, r.severity),
            (Status::Attention, Severity::Problem)
        );
        assert!(r.observation.contains("Unhealthy"));
        assert!(r.observation.contains("Lost communication"));
        assert!(!r.observation.contains("RAID"));
        assert_limits(&r);
    }

    #[test]
    fn other_errors_are_cleaned_and_cut() {
        let nasty = format!("<b>bad</b>\u{7}\n\tline {}", "x".repeat(600));
        let r = check("C:", &Err(ReadError::Other(nasty)), NOW);
        assert_eq!(r.status, Status::Unavailable);
        assert!(r
            .observation
            .starts_with("Drive health could not be read: bbad/b line"));
        assert_limits(&r);
    }

    #[test]
    fn not_checked_before_the_first_read() {
        let r = not_checked("c", NOW);
        assert_eq!(r.status, Status::NotChecked);
        assert_eq!(r.id, "drive-health-c");
        assert_limits(&r);
    }

    #[test]
    fn ids_follow_the_letter_and_bad_letters_get_no_action() {
        for (raw, id) in [
            ("C:", "drive-health-c"),
            ("z", "drive-health-z"),
            (" Q: ", "drive-health-q"),
        ] {
            assert_eq!(check(raw, &Err(ReadError::Unsupported), NOW).id, id);
        }
        let r = check("C:\\Windows", &Err(ReadError::Unsupported), NOW);
        assert_eq!(r.id, "drive-health");
        assert_eq!(r.action, None);
        assert_limits(&Reading { action: None, ..r });
    }

    #[test]
    fn drives_are_normalized_sorted_and_unique() {
        assert_eq!(
            drives_to_check(&["d:", "C:", "c", "", "1:", "AB:", "C:\\x", "E:"]),
            vec!["C:", "D:", "E:"]
        );
        assert!(drives_to_check::<&str>(&[]).is_empty());
        assert_eq!(drives_to_check(&["C:".to_string()]), vec!["C:"]);
    }

    #[test]
    fn cadence_is_start_then_every_six_hours() {
        assert_eq!(REFRESH_EVERY_SECS, 21_600);
        assert!(should_refresh(NOW, None), "read at start");
        assert!(!should_refresh(NOW + 1, Some(NOW)));
        assert!(!should_refresh(NOW + REFRESH_EVERY_SECS - 1, Some(NOW)));
        assert!(should_refresh(NOW + REFRESH_EVERY_SECS, Some(NOW)));
        assert!(should_refresh(NOW - 5, Some(NOW)), "clock went backwards");
        // A reading read every 6 h is never stale before the next one.
        assert!(u64::from(STALE_AFTER_SECS) > REFRESH_EVERY_SECS);
    }

    // --- The reporter: one finding per fixed drive, read once per cycle, in the background. ---

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};
    use toolboks_addon_sdk::{Finding, FindingAction, FindingStatus, Severity as Sev};

    fn drive_list() -> Vec<String> {
        // Out of order, repeated, in lower case, and with entries that are not drives.
        ["h", "C:", "d:", "E:", "F:", "G:", "c", "1:", "C:\\x"]
            .map(String::from)
            .to_vec()
    }

    fn twenty_drives() -> Vec<String> {
        (b'A'..=b'T').map(|l| format!("{}:", l as char)).collect()
    }

    /// C healthy NVMe, D SATA without administrator, E unsupported, F NVMe to watch, G NVMe to
    /// replace, H healthy SATA.
    fn recorded(letter: &str) -> Result<Report, ReadError> {
        match letter {
            "C:" => Ok(report(parse_nvme(&nvme(0, 311, 100, 3, 0)))),
            "D:" => Err(ReadError::AccessDenied),
            "E:" => Err(ReadError::Unsupported),
            "F:" => Ok(report(parse_nvme(&nvme(0, 318, 100, 74, 6)))),
            "G:" => Ok(report(parse_nvme(&nvme(0, 318, 100, 93, 6)))),
            "H:" => Ok(report(parse_ata(&ata(&[
                (5, 100, 0),
                (9, 99, 12_345),
                (194, 67, 33),
                (197, 100, 0),
            ])))),
            _ => Err(ReadError::NotFound),
        }
    }

    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let start = Instant::now();
        while !done() {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "timed out: {what}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn settled(r: &Reporter) -> bool {
        let f = r.findings();
        !f.is_empty() && f.iter().all(|f| f.status != FindingStatus::NotChecked)
    }

    fn by_id<'a>(findings: &'a [Finding], id: &str) -> &'a Finding {
        findings.iter().find(|f| f.id == id).expect(id)
    }

    #[test]
    fn every_verdict_reaches_home_as_one_finding_per_drive() {
        static READS: AtomicUsize = AtomicUsize::new(0);
        fn read(letter: &str) -> Result<Report, ReadError> {
            READS.fetch_add(1, Ordering::SeqCst);
            recorded(letter)
        }
        let r = Reporter::with(Default::default(), read, drive_list);
        assert!(r.findings().is_empty(), "nothing before the first listing");
        r.poll_at(NOW);
        wait_until("first read", || settled(&r));
        let f = r.findings();
        let ids: Vec<&str> = f.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "drive-health-c",
                "drive-health-d",
                "drive-health-e",
                "drive-health-f",
                "drive-health-g",
                "drive-health-h"
            ]
        );
        let status = |id| {
            let f = by_id(&f, id);
            (f.status, f.severity)
        };
        assert_eq!(status("drive-health-c").0, FindingStatus::Ok);
        assert_eq!(status("drive-health-h").0, FindingStatus::Ok);
        assert_eq!(
            status("drive-health-f"),
            (FindingStatus::Attention, Sev::Watch)
        );
        assert_eq!(
            status("drive-health-g"),
            (FindingStatus::Attention, Sev::Problem)
        );
        // SATA without administrator rights and unsupported drives: Unavailable, plain reason.
        let d = by_id(&f, "drive-health-d");
        assert_eq!(d.status, FindingStatus::Unavailable);
        assert!(d.observation.starts_with("Needs administrator rights"));
        let e = by_id(&f, "drive-health-e");
        assert_eq!(e.status, FindingStatus::Unavailable);
        assert!(e
            .observation
            .starts_with("This drive does not report health"));
        for f in &f {
            assert!(Finding::valid_id(&f.id), "{}", f.id);
            assert_eq!(f.stale_after_secs, 86_400);
            assert!(!f.title.is_empty() && f.title.chars().count() <= TITLE_MAX);
            let letter =
                f.id.trim_start_matches("drive-health-")
                    .to_ascii_uppercase();
            assert!(matches!(
                &f.action,
                Some(FindingAction::OpenPage { route, label })
                    if *route == format!("Optimization/{letter}:") && label == "Open drive health"
            ));
        }
        assert_eq!(READS.load(Ordering::SeqCst), 6, "each drive read once");
    }

    #[test]
    fn findings_stay_the_same_between_ticks() {
        static READS: AtomicUsize = AtomicUsize::new(0);
        fn read(letter: &str) -> Result<Report, ReadError> {
            READS.fetch_add(1, Ordering::SeqCst);
            recorded(letter)
        }
        let r = Reporter::with(Default::default(), read, drive_list);
        r.poll_at(NOW);
        wait_until("first read", || settled(&r));
        let first = r.findings();
        // Many ticks, a little later: no new read, so exactly the same findings (the runtime
        // sends a message only when they change).
        for tick in 1..50 {
            r.poll_at(NOW + tick * 60);
            assert_eq!(r.findings(), first);
        }
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(r.findings(), first);
        assert_eq!(READS.load(Ordering::SeqCst), 6);
        // Just before six hours: still no read.
        r.poll_at(NOW + REFRESH_EVERY_SECS - 1);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(READS.load(Ordering::SeqCst), 6);
    }

    #[test]
    fn the_first_findings_say_not_checked_while_the_read_runs() {
        static RELEASE: AtomicBool = AtomicBool::new(false);
        fn slow(letter: &str) -> Result<Report, ReadError> {
            while !RELEASE.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(2));
            }
            recorded(letter)
        }
        let r = Reporter::with(Default::default(), slow, drive_list);
        r.poll_at(NOW);
        wait_until("listing", || !r.findings().is_empty());
        let waiting = r.findings();
        assert_eq!(waiting.len(), 6);
        assert!(waiting
            .iter()
            .all(|f| f.status == FindingStatus::NotChecked));
        assert!(waiting.iter().all(|f| f.measured_at == NOW));
        assert_eq!(r.findings(), waiting, "stable while waiting");
        RELEASE.store(true, Ordering::SeqCst);
        wait_until("reads", || settled(&r));
    }

    #[test]
    fn it_reads_every_six_hours_and_keeps_the_old_reading_meanwhile() {
        static READS: AtomicUsize = AtomicUsize::new(0);
        fn read(letter: &str) -> Result<Report, ReadError> {
            // C is healthy on the first read and worn out on the second.
            let n = READS.fetch_add(1, Ordering::SeqCst);
            if letter == "C:" && n >= 6 {
                return Ok(report(parse_nvme(&nvme(0, 311, 100, 93, 0))));
            }
            recorded(letter)
        }
        let r = Reporter::with(Default::default(), read, drive_list);
        r.poll_at(NOW);
        wait_until("first read", || settled(&r));
        assert_eq!(
            by_id(&r.findings(), "drive-health-c").status,
            FindingStatus::Ok
        );
        r.poll_at(NOW + REFRESH_EVERY_SECS);
        // Never back to "not checked" while the new reading is being made.
        wait_until("second read", || {
            let f = r.findings();
            assert!(f.iter().all(|f| f.status != FindingStatus::NotChecked));
            by_id(&f, "drive-health-c").status == FindingStatus::Attention
        });
        assert_eq!(READS.load(Ordering::SeqCst), 12);
        assert_eq!(
            by_id(&r.findings(), "drive-health-c").severity,
            Sev::Problem
        );
    }

    #[test]
    fn check_now_reads_again_now() {
        static READS: AtomicUsize = AtomicUsize::new(0);
        fn read(letter: &str) -> Result<Report, ReadError> {
            READS.fetch_add(1, Ordering::SeqCst);
            recorded(letter)
        }
        let r = Reporter::with(Default::default(), read, drive_list);
        r.poll_at(unix_now());
        wait_until("first read", || {
            settled(&r) && READS.load(Ordering::SeqCst) == 6
        });
        std::thread::sleep(Duration::from_millis(50));
        r.check_now();
        wait_until("second read", || READS.load(Ordering::SeqCst) == 12);
        wait_until("settled", || settled(&r));
    }

    #[test]
    fn clearing_the_page_readings_is_read_again_without_reading_twice() {
        static READS: AtomicUsize = AtomicUsize::new(0);
        fn read(letter: &str) -> Result<Report, ReadError> {
            READS.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(30));
            recorded(letter)
        }
        let cache = Cache::default();
        let r = Reporter::with(cache.clone(), read, drive_list);
        r.poll_at(unix_now());
        wait_until("first read", || {
            settled(&r) && READS.load(Ordering::SeqCst) == 6
        });
        // "Refresh drive information" on the Optimization page clears the cache and the page
        // asks for its selected drive again while the reporter asks for all of them.
        health::clear(&cache);
        r.poll();
        health::refresh(&cache, "C:", read);
        wait_until("settled again", || settled(&r));
        assert_eq!(
            READS.load(Ordering::SeqCst),
            12,
            "C: was read once, not twice"
        );
    }

    #[test]
    fn at_most_sixteen_drives_are_reported() {
        let r = Reporter::with(Default::default(), recorded, twenty_drives);
        r.poll_at(NOW);
        wait_until("listing", || !r.findings().is_empty());
        let f = r.findings();
        assert_eq!(f.len(), MAX_DRIVES);
        assert_eq!(f[0].id, "drive-health-a");
        assert_eq!(f[15].id, "drive-health-p");
        wait_until("reads", || settled(&r));
    }

    #[test]
    fn a_pc_with_no_fixed_drives_reports_nothing() {
        fn none() -> Vec<String> {
            Vec::new()
        }
        let r = Reporter::with(Default::default(), recorded, none);
        r.poll_at(NOW);
        std::thread::sleep(Duration::from_millis(50));
        assert!(r.findings().is_empty());
    }

    #[test]
    fn a_reading_converts_field_for_field() {
        let reading = check("D:", &Err(ReadError::AccessDenied), NOW);
        let finding = Finding::from(reading.clone());
        assert_eq!(
            (finding.id.as_str(), finding.title.as_str()),
            (reading.id.as_str(), reading.title.as_str())
        );
        assert_eq!(finding.observation, reading.observation);
        assert_eq!(finding.status, FindingStatus::Unavailable);
        assert_eq!(
            (finding.measured_at, finding.stale_after_secs),
            (NOW, 86_400)
        );
        let finding = Finding::from(not_checked("C:", NOW));
        assert_eq!(finding.status, FindingStatus::NotChecked);
        let bad = check("C:\\Windows", &Err(ReadError::Unsupported), NOW);
        assert_eq!(Finding::from(bad).action, None);
    }
}
