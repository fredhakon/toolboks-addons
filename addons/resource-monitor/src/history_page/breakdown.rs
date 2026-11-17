//! "What used the resources" (#43): the rows of the apps table, the plain sentence above it and
//! the table as text. Pure functions over a [`Breakdown`]; the page draws them.

use toolboks_shared::format::{rate, size_plain};

use super::time;
use crate::recording::query::{
    AppRow, Breakdown, Figure, Resource, CPU, DISK_READ, DISK_WRITE, GPU, MEM,
};

/// Apps listed by name; the rest go into "Everything else".
pub const APP_ROWS: usize = 12;

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    /// Exe name.
    App(String),
    /// Apps below the recording thresholds and apps past the listed rows.
    Else,
    /// Part of the period nothing was recorded.
    NotRecorded,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub kind: Kind,
    pub label: String,
    /// 0 to 1 of all apps together; `None` where a share means nothing.
    pub share: Option<f64>,
    pub avg: String,
    pub peak: String,
    pub active: String,
}

pub fn resource_name(by: Resource) -> &'static str {
    match by {
        Resource::Cpu => "CPU",
        Resource::Memory => "Memory",
        Resource::Gpu => "GPU",
        Resource::Disk => "Disk",
    }
}

fn pct(v: f64) -> String {
    if v > 0.0 && v < 1.0 {
        format!("{v:.1} %")
    } else {
        format!("{v:.0} %")
    }
}

/// Use of a resource over the period (for shares) and its average and peak as text.
fn use_of(by: Resource, f: &[Figure; 5]) -> (f64, String, String) {
    match by {
        Resource::Cpu => (f[CPU].total, pct(f[CPU].avg), pct(f[CPU].peak)),
        Resource::Gpu => (f[GPU].total, pct(f[GPU].avg), pct(f[GPU].peak)),
        Resource::Memory => (
            f[MEM].total,
            size_plain(f[MEM].avg as u64),
            size_plain(f[MEM].peak as u64),
        ),
        Resource::Disk => (
            f[DISK_READ].total + f[DISK_WRITE].total,
            rate(f[DISK_READ].avg + f[DISK_WRITE].avg),
            rate(f[DISK_READ].peak.max(f[DISK_WRITE].peak)),
        ),
    }
}

/// The table: the ranked apps, "Everything else" and "Not recorded". The shares of the apps
/// and "Everything else" add up to 100 % of what was recorded.
pub fn rows(b: &Breakdown, by: Resource) -> Vec<Row> {
    let all: f64 =
        b.apps.iter().map(|a| use_of(by, &a.figures).0).sum::<f64>() + use_of(by, &b.other).0;
    let share = |u: f64| if all > 0.0 { u / all } else { 0.0 };
    let ranked = b.ranked(by);
    let mut out = Vec::new();
    let mut listed = 0.0;
    for a in ranked.iter().take(APP_ROWS) {
        let (u, avg, peak) = use_of(by, &a.figures);
        let unknown = by == Resource::Disk && !a.disk_known;
        listed += u;
        out.push(Row {
            kind: Kind::App(a.name.clone()),
            label: a.name.clone(),
            share: (!unknown).then(|| share(u)),
            avg: if unknown { "unknown".into() } else { avg },
            peak: if unknown { "unknown".into() } else { peak },
            active: time::duration(a.active_secs),
        });
    }
    // Everything else: what the listed apps did not use (the unlisted apps and the small ones).
    let rest = (all - listed).max(0.0);
    let secs = b.recorded_secs.max(f64::EPSILON);
    let rest_avg = rest / secs;
    if rest > 0.0 || !out.is_empty() {
        out.push(Row {
            kind: Kind::Else,
            label: "Everything else".into(),
            share: Some(share(rest)),
            avg: match by {
                Resource::Cpu | Resource::Gpu => pct(rest_avg),
                Resource::Memory => size_plain(rest_avg as u64),
                Resource::Disk => rate(rest_avg),
            },
            peak: String::new(),
            active: String::new(),
        });
    }
    let missing = b.period_secs() - b.recorded_secs;
    if missing >= 1.0 {
        out.push(Row {
            kind: Kind::NotRecorded,
            label: "Not recorded".into(),
            share: None,
            avg: String::new(),
            peak: String::new(),
            active: format!(
                "{} of {}",
                time::duration(missing),
                time::duration(b.period_secs())
            ),
        });
    }
    out
}

/// "Between 14:10 and 14:25 chrome.exe used 46 % of the CPU on average; memory peaked at 91 %."
pub fn sentence(b: &Breakdown, by: Resource, when: &str, mem_total: Option<u64>) -> String {
    if b.recorded_secs <= 0.0 {
        return format!("{when}, nothing was recorded.");
    }
    let top: Option<&AppRow> = b
        .ranked(by)
        .into_iter()
        .find(|a| by != Resource::Disk || a.disk_known);
    let mut s = match top {
        Some(a) => {
            let f = &a.figures;
            let name = &a.name;
            match by {
                Resource::Cpu => format!(
                    "{when} {name} used {} of the CPU on average",
                    pct(f[CPU].avg)
                ),
                Resource::Gpu => format!(
                    "{when} {name} used {} of the GPU on average",
                    pct(f[GPU].avg)
                ),
                Resource::Memory => format!(
                    "{when} {name} used the most memory, {} on average",
                    size_plain(f[MEM].avg as u64)
                ),
                Resource::Disk => format!(
                    "{when} {name} moved the most data on disk, {} on average",
                    rate(f[DISK_READ].avg + f[DISK_WRITE].avg)
                ),
            }
        }
        None => format!(
            "{when} no app used the {} noticeably",
            match by {
                Resource::Cpu => "CPU",
                Resource::Gpu => "GPU",
                Resource::Memory => "memory",
                Resource::Disk => "disk",
            }
        ),
    };
    if let Some(m) = b.system[MEM] {
        match mem_total {
            Some(t) if t > 0 => s.push_str(&format!(
                "; memory peaked at {:.0} %",
                m.peak as f64 / t as f64 * 100.0
            )),
            _ => s.push_str(&format!("; memory peaked at {}", size_plain(m.peak as u64))),
        }
    }
    s.push('.');
    let missing = b.period_secs() - b.recorded_secs;
    if missing >= 1.0 {
        s.push_str(&format!(
            " Only {} of {} was recorded.",
            time::duration(b.recorded_secs),
            time::duration(b.period_secs())
        ));
    }
    s
}

/// The table as text, one row per line, columns separated by tabs (pastes into a spreadsheet).
pub fn copy_text(title: &str, sentence: &str, by: Resource, rows: &[Row]) -> String {
    let mut out = format!(
        "{title}\n{sentence}\n\nApp\t{} share\tAverage\tPeak\tActive\n",
        resource_name(by)
    );
    for r in rows {
        let share = r
            .share
            .map(|s| format!("{:.1} %", s * 100.0))
            .unwrap_or_default();
        out.push_str(&format!(
            "{}\t{share}\t{}\t{}\t{}\n",
            r.label, r.avg, r.peak, r.active
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::store::Stat;

    fn fig(avg: f64, peak: f64, secs: f64) -> Figure {
        Figure {
            avg,
            peak,
            total: avg * secs,
            share: 0.0,
        }
    }

    fn app(name: &str, cpu: f64, disk: f64, secs: f64, known: bool) -> AppRow {
        AppRow {
            name: name.into(),
            active_secs: secs,
            peak_procs: 1,
            disk_known: known,
            figures: [
                fig(cpu, cpu * 2.0, secs),
                fig(1e9, 2e9, secs),
                fig(0.0, 0.0, secs),
                fig(disk, disk, secs),
                fig(disk, disk, secs),
            ],
        }
    }

    fn sample() -> Breakdown {
        let secs = 600.0;
        let mut apps: Vec<AppRow> = vec![
            app("chrome.exe", 40.0, 0.0, secs, true),
            app("backup.exe", 1.0, 5e6, secs, true),
            app("MsMpEng.exe", 2.0, 9e6, secs, false),
        ];
        for i in 0..20 {
            apps.push(app(&format!("small{i}.exe"), 0.5, 0.0, secs, true));
        }
        Breakdown {
            from_ms: 0,
            to_ms: 900_000,
            recorded_secs: secs,
            apps,
            other: [
                fig(3.2, 3.2, secs),
                fig(2e9, 2e9, secs),
                fig(0.0, 0.0, secs),
                fig(0.0, 0.0, secs),
                fig(0.0, 0.0, secs),
            ],
            system: {
                let mut s = [None; crate::recording::record::TOTAL_FIGURES];
                s[0] = Some(Stat {
                    avg: 60.0,
                    peak: 95.0,
                });
                s[1] = Some(Stat {
                    avg: 12e9,
                    peak: 14.56e9,
                });
                s
            },
        }
    }

    #[test]
    fn the_table_adds_up_and_names_what_was_not_recorded() {
        let b = sample();
        let rows = rows(&b, Resource::Cpu);
        assert_eq!(rows[0].label, "chrome.exe");
        assert_eq!(rows.len(), APP_ROWS + 2);
        let total: f64 = rows.iter().filter_map(|r| r.share).sum();
        assert!((total - 1.0).abs() < 1e-9, "{total}");
        let rest = &rows[APP_ROWS];
        assert_eq!(rest.kind, Kind::Else);
        // 11 small apps not listed (0.5 % each) plus the 3.2 % below the thresholds.
        assert_eq!(rest.avg, "9 %");
        let missing = rows.last().unwrap();
        assert_eq!(missing.kind, Kind::NotRecorded);
        assert_eq!(missing.active, "5 min of 15 min");
    }

    #[test]
    fn unknown_disk_stays_unknown() {
        let rows = rows(&sample(), Resource::Disk);
        let defender = rows.iter().find(|r| r.label == "MsMpEng.exe").unwrap();
        assert_eq!((defender.avg.as_str(), defender.share), ("unknown", None));
        assert_eq!(
            rows[0].label, "MsMpEng.exe",
            "ranked by its (unknown-flagged) figures"
        );
        assert_eq!(rows[1].label, "backup.exe");
    }

    #[test]
    fn the_sentence_says_who_used_what_and_how_much_was_recorded() {
        let b = sample();
        let s = sentence(
            &b,
            Resource::Cpu,
            "Between 14:10 and 14:25",
            Some(16_000_000_000),
        );
        assert_eq!(
            s,
            "Between 14:10 and 14:25 chrome.exe used 40 % of the CPU on average; memory peaked at 91 %. Only 10 min of 15 min was recorded."
        );
        let s = sentence(&b, Resource::Disk, "At 14:32", None);
        assert!(
            s.starts_with("At 14:32 backup.exe moved the most data on disk, 9.5 MB/s on average"),
            "{s}"
        );
        let mut empty = sample();
        empty.recorded_secs = 0.0;
        assert_eq!(
            sentence(&empty, Resource::Cpu, "At 14:32", None),
            "At 14:32, nothing was recorded."
        );
    }

    #[test]
    fn the_copy_is_one_row_per_line() {
        let b = sample();
        let r = rows(&b, Resource::Cpu);
        let text = copy_text("Apps between 14:10 and 14:25", "S.", Resource::Cpu, &r);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[3], "App\tCPU share\tAverage\tPeak\tActive");
        assert!(lines[4].starts_with("chrome.exe\t"));
        assert_eq!(lines.len(), 4 + r.len());
    }
}
