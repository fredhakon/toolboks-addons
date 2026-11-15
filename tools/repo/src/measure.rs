//! Measures each add-on's release exe against the budgets in `budgets.json` (host issue #97).
//!
//! It starts the exe the way the headless protocol test does (stdin and stdout as the two
//! pipes), so it measures the add-on itself: no named pipes, launch token or host window.
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};
use toolboks_addon_proto::{
    host_handshake, AddonMessage, FrameReader, FrameWriter, Hello, HostMessage, InputBatch, Theme,
};

/// How long an add-on may take for each step before the run is given up.
const STEP_TIMEOUT: Duration = Duration::from_secs(20);
/// How long the add-on sits untouched before its memory is read.
const SETTLE: Duration = Duration::from_secs(3);

struct Budgets {
    first_frame_ms: f64,
    idle_working_set_bytes: u64,
}

impl Budgets {
    fn load(path: &str) -> Self {
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(path).expect("budgets.json"))
                .expect("budgets.json is not JSON");
        Self {
            first_frame_ms: value["first_frame_ms"]
                .as_f64()
                .expect("first_frame_ms in budgets.json"),
            idle_working_set_bytes: value["idle_working_set_bytes"]
                .as_u64()
                .expect("idle_working_set_bytes in budgets.json"),
        }
    }
}

/// One start of an add-on.
struct Run {
    handshake_ms: f64,
    first_frame_ms: f64,
    working_set: u64,
    private_bytes: u64,
}

struct Guard(Child);
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(windows)]
fn memory(child: &Child) -> (u64, u64) {
    use std::os::windows::io::AsRawHandle;
    // PROCESS_MEMORY_COUNTERS_EX, as documented for GetProcessMemoryInfo.
    #[repr(C)]
    #[derive(Default)]
    struct Counters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
        private_usage: usize,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn K32GetProcessMemoryInfo(
            process: *mut std::ffi::c_void,
            counters: *mut Counters,
            size: u32,
        ) -> i32;
    }
    let mut counters = Counters {
        cb: std::mem::size_of::<Counters>() as u32,
        ..Default::default()
    };
    // SAFETY: the handle belongs to a child this process still owns, and the struct has the
    // layout and size the call is told about.
    let ok = unsafe { K32GetProcessMemoryInfo(child.as_raw_handle(), &mut counters, counters.cb) };
    assert!(ok != 0, "could not read the add-on's memory use");
    (
        counters.working_set_size as u64,
        counters.private_usage as u64,
    )
}

#[cfg(not(windows))]
fn memory(_child: &Child) -> (u64, u64) {
    panic!("measure runs on Windows only");
}

fn ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

fn run(exe: &Path, id: &str) -> Run {
    let started = Instant::now();
    let mut child = Guard(
        Command::new(exe)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| panic!("could not start {}: {error}", exe.display())),
    );
    let stdout = child.0.stdout.take().unwrap();
    let stdin = child.0.stdin.take().unwrap();
    let (timing, timings) = mpsc::channel();
    let (finish, finished) = mpsc::channel::<()>();
    let id = id.to_owned();
    std::thread::spawn(move || {
        let mut reader = FrameReader::new(stdout);
        let mut writer = FrameWriter::new(stdin);
        host_handshake(&mut reader, &mut writer, &Hello::new(&id, "measure")).unwrap();
        let handshake = ms(started);
        writer
            .write(&HostMessage::Input(InputBatch {
                seq: 1,
                size: [900.0, 600.0],
                pixels_per_point: 1.0,
                theme: Theme::Dark,
                window_focused: true,
                events: vec![],
            }))
            .unwrap();
        loop {
            if let AddonMessage::Frame(frame) = reader.read().unwrap() {
                if frame.seq == 1 {
                    break;
                }
            }
        }
        timing.send((handshake, ms(started))).unwrap();
        // Wait here, sending nothing, until the memory has been read.
        let _ = finished.recv();
        let _ = writer.write(&HostMessage::Quit);
        while reader.read::<AddonMessage>().is_ok() {}
    });
    let (handshake_ms, first_frame_ms) = timings
        .recv_timeout(STEP_TIMEOUT)
        .unwrap_or_else(|_| panic!("{} did not draw a first frame", exe.display()));
    std::thread::sleep(SETTLE);
    let (working_set, private_bytes) = memory(&child.0);
    let _ = finish.send(());
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.0.try_wait().unwrap().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    Run {
        handshake_ms,
        first_frame_ms,
        working_set,
        private_bytes,
    }
}

fn median<T: Copy + PartialOrd>(mut values: Vec<T>) -> T {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    values[values.len() / 2]
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_048_576.0)
}

fn addons(names: &[String]) -> Vec<String> {
    if !names.is_empty() {
        return names.to_vec();
    }
    let mut all: Vec<String> = fs::read_dir("addons")
        .unwrap()
        .map(|dir| dir.unwrap())
        .filter(|dir| dir.file_type().unwrap().is_dir())
        .map(|dir| dir.file_name().to_str().unwrap().to_owned())
        .collect();
    all.sort();
    all
}

/// `measure [--report-only] [--runs <n>] [--json <file>] [<add-on>...]`
pub fn measure(args: &[&str]) {
    let mut report_only = false;
    let mut runs = 5usize;
    let mut json: Option<PathBuf> = None;
    let mut names = Vec::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match *arg {
            "--report-only" => report_only = true,
            "--runs" => {
                runs = rest
                    .next()
                    .and_then(|value| value.parse().ok())
                    .filter(|runs| (1..=50).contains(runs))
                    .expect("--runs takes a number from 1 to 50")
            }
            "--json" => json = Some(PathBuf::from(rest.next().expect("--json takes a file"))),
            name => names.push(name.to_owned()),
        }
    }
    let budgets = Budgets::load("budgets.json");
    let mut rows = Vec::new();
    let mut missed = Vec::new();
    println!(
        "| Add-on | Exe size | Handshake | First frame | Idle memory | Private bytes | Budgets |"
    );
    println!("|---|---|---|---|---|---|---|");
    for name in addons(&names) {
        let id = crate::package(&name).manifest.id;
        let exe = PathBuf::from(format!("target/release/{name}.exe"));
        let size = fs::metadata(&exe)
            .unwrap_or_else(|_| {
                panic!(
                    "{} is missing; run `cargo build --locked --release -p {name}` first",
                    exe.display()
                )
            })
            .len();
        let results: Vec<Run> = (0..runs).map(|_| run(&exe, &id)).collect();
        let handshake = median(results.iter().map(|run| run.handshake_ms).collect());
        let first_frame = median(results.iter().map(|run| run.first_frame_ms).collect());
        let working_set = median(results.iter().map(|run| run.working_set).collect());
        let private = median(results.iter().map(|run| run.private_bytes).collect());
        let mut over = Vec::new();
        if first_frame > budgets.first_frame_ms {
            over.push(format!("first frame over {:.0} ms", budgets.first_frame_ms));
        }
        if working_set > budgets.idle_working_set_bytes {
            over.push(format!(
                "idle memory over {}",
                megabytes(budgets.idle_working_set_bytes)
            ));
        }
        println!(
            "| {name} | {} | {handshake:.0} ms | {first_frame:.0} ms | {} | {} | {} |",
            megabytes(size),
            megabytes(working_set),
            megabytes(private),
            if over.is_empty() {
                "met".to_owned()
            } else {
                over.join(", ")
            }
        );
        rows.push(serde_json::json!({
            "addon": name,
            "id": id,
            "exe_bytes": size,
            "handshake_ms": handshake,
            "first_frame_ms": first_frame,
            "idle_working_set_bytes": working_set,
            "idle_private_bytes": private,
            "runs": runs,
            "over_budget": over,
        }));
        if !over.is_empty() {
            missed.push(name);
        }
    }
    println!();
    println!(
        "Median of {runs} starts each. Budgets: first frame within {:.0} ms, idle memory under {}.",
        budgets.first_frame_ms,
        megabytes(budgets.idle_working_set_bytes)
    );
    if let Some(path) = json {
        let report = serde_json::json!({
            "budgets": {
                "first_frame_ms": budgets.first_frame_ms,
                "idle_working_set_bytes": budgets.idle_working_set_bytes,
            },
            "addons": rows,
        });
        fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
    if !missed.is_empty() && !report_only {
        eprintln!("Over budget: {}", missed.join(", "));
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_takes_the_middle_value() {
        assert_eq!(median(vec![5.0, 1.0, 3.0]), 3.0);
        assert_eq!(median(vec![9u64]), 9);
    }

    #[test]
    fn the_budgets_file_holds_the_numbers_from_issue_97() {
        let budgets = Budgets::load(concat!(env!("CARGO_MANIFEST_DIR"), "/../../budgets.json"));
        assert_eq!(budgets.first_frame_ms, 1000.0);
        assert_eq!(budgets.idle_working_set_bytes, 25 * 1024 * 1024);
    }
}
