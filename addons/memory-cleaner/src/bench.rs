//! Test-only entry points for the memory benchmark (issue #32, `scripts/bench-memory-driven.ps1`).
//! Compiled only for `cargo test`, never part of the add-on exe, and every test here is
//! `#[ignore]`d so the normal gate does not run them.
//!
//! The benchmark presses "Trim idle apps" for the person in front of the PC. This file does the
//! same without a window and without touching any process the benchmark did not start:
//!
//! * `bench_child_workload` is one benchmark-owned process that holds some memory, stays idle,
//!   and re-reads all of it on request (timed), which measures what a trim costs on next use.
//! * `bench_trim_action` runs the add-on's own manual plan and runner (`actions::plan`,
//!   `actions::run` with `WindowsOps`: lower memory priority, then trim) over those children only.
//!   It is handed the child list, never enumerates other processes, and prints one `BENCH-TRIM`
//!   line with the same numbers the Last run card shows.
//!
//! What differs from the button: the candidate list is built from the children instead of the
//! process snapshot. Idle time is supplied by the script (seconds since the children were last
//! touched) because the children have no window and no input.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::actions::{
    self, Candidate, Cooldowns, Identity, Policy, ProcessOps, UndoLog, WindowsOps,
};
use crate::engine::RunSummary;

const MB: usize = 1024 * 1024;
const PAGE: usize = 4096;
/// A benchmark child that nobody stopped exits by itself after this long.
const MAX_LIFETIME: Duration = Duration::from_secs(3 * 3600);

fn env_dir() -> PathBuf {
    PathBuf::from(std::env::var("TB_BENCH_DIR").expect("TB_BENCH_DIR"))
}

fn env_num(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn image_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .expect("own image name")
}

/// Reads one byte per page. Returns a checksum so the loop is not optimised away.
fn touch_all(buf: &[u8]) -> u64 {
    let mut sum = 0u64;
    let mut at = 0;
    while at < buf.len() {
        // SAFETY: `at` is inside `buf`.
        sum += u64::from(unsafe { std::ptr::read_volatile(buf.as_ptr().add(at)) });
        at += PAGE;
    }
    sum
}

fn write_file(path: &Path, text: &str) {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, text).expect("write temp file");
    fs::rename(&tmp, path).expect("rename temp file");
}

#[test]
#[ignore = "benchmark helper: started by scripts/bench-memory-driven.ps1"]
fn bench_child_workload() {
    let dir = env_dir();
    let mb = env_num("TB_BENCH_MB", 300) as usize;
    let pid = std::process::id();
    let mut buf = vec![0u8; mb * MB];
    // Write once so every page is private and resident, like an app's heap.
    for at in (0..buf.len()).step_by(PAGE) {
        buf[at] = 1;
    }
    let me = actions::identity_of(pid, &image_name()).expect("own identity");
    write_file(
        &dir.join(format!("child-{pid}.ready")),
        &format!("{pid} {mb}"),
    );

    let started = Instant::now();
    let mut done_seq = 0u64;
    while started.elapsed() < MAX_LIFETIME && !dir.join("stop").exists() {
        let want = fs::read_to_string(dir.join("touch.req"))
            .ok()
            .and_then(|t| t.trim().parse::<u64>().ok())
            .unwrap_or(0);
        if want > done_seq {
            // Back to normal priority first, so a trim arm does not leave a low priority behind
            // for the next run. Then time the re-read of everything.
            let _ = WindowsOps.set_memory_priority(&me, actions::NORMAL_PRIORITY);
            let t = Instant::now();
            let sum = touch_all(&buf);
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            write_file(
                &dir.join(format!("touched-{pid}.{want}")),
                &format!("{ms:.2} {sum}"),
            );
            done_seq = want;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
#[ignore = "benchmark helper: started by scripts/bench-memory-driven.ps1"]
fn bench_trim_action() {
    let dir = env_dir();
    let idle = Duration::from_secs(env_num("TB_BENCH_IDLE_SECS", 120));
    let image = image_name();

    // The children, from the files they wrote. Nothing else is looked at.
    let mut candidates = Vec::new();
    for entry in fs::read_dir(&dir).expect("read bench dir").flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !(name.starts_with("child-") && name.ends_with(".ready")) {
            continue;
        }
        let text = fs::read_to_string(entry.path()).expect("read ready file");
        let mut parts = text
            .split_whitespace()
            .map(|p| p.parse::<u64>().unwrap_or(0));
        let (pid, mb) = (parts.next().unwrap_or(0) as u32, parts.next().unwrap_or(0));
        if let Some(identity) = actions::identity_of(pid, &image) {
            candidates.push(Candidate {
                identity,
                working_set: mb * MB as u64,
                idle_for: Some(idle),
                plays_audio: false,
                is_fullscreen: false,
                parent_name: None,
            });
        }
    }
    let ids: Vec<Identity> = candidates.iter().map(|c| c.identity.clone()).collect();

    let policy = Policy::manual(std::process::id(), actions::foreground_pid());
    let now = Instant::now();
    let mut undo = UndoLog::new();
    let mut cooldowns = Cooldowns::new();
    let plan = actions::plan(&candidates, &policy, &cooldowns, now);
    // Safety net: the plan can only contain the children.
    assert!(plan.iter().all(|a| ids.contains(&a.identity)));
    let results = actions::run(&plan, &WindowsOps, &mut undo, &mut cooldowns, now);

    let summary = RunSummary::from_results(&results);
    let ws_before: u64 = results.iter().filter_map(|r| r.before).sum();
    let ws_after: u64 = results.iter().filter_map(|r| r.after).sum();
    println!(
        "BENCH-TRIM children={} lowered={} trimmed={} failed={} ws_before_mb={} ws_after_mb={} text={}",
        ids.len(),
        summary.lowered,
        summary.trimmed,
        summary.failed,
        ws_before / MB as u64,
        ws_after / MB as u64,
        summary.text()
    );
}
