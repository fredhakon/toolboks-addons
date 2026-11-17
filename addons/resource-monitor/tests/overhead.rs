#![cfg(windows)]
//! Overhead check of the Processes page (#70): drives the exe the way the host does (one frame
//! per repaint request, at most 60 per second, a ping every second) and reads the add-on's own
//! CPU time and memory with each page shown and while suspended (hidden).
//!
//! Not part of the gate: it takes a few minutes and its numbers depend on the PC. Run it on the
//! release exe, with nothing else heavy running:
//!
//! ```text
//! ADDON_TEST_EXE=<target>/release/resource-monitor.exe OVERHEAD_DATA=<fresh folder> \
//!   cargo test --release -p resource-monitor --test overhead -- --ignored --nocapture
//! ```
//!
//! `OVERHEAD_SECS` (default 60) is the measured time per phase, after a 10 s warm-up.
//! `OVERHEAD_DATA` is the data folder handed to the add-on (`TOOLBOKS_DATA_DIR`), so the history
//! recorder never writes into a real Toolboks folder.

use std::os::windows::io::AsRawHandle;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use toolboks_addon_proto::{
    host_handshake, AddonMessage, FrameReader, FrameWriter, Hello, HostMessage, InputBatch, Theme,
};
use windows::Win32::Foundation::{FILETIME, HANDLE};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::Threading::GetProcessTimes;

const WARM_UP: Duration = Duration::from_secs(10);
const FRAME_INTERVAL: Duration = Duration::from_nanos(16_666_667);
const PING_INTERVAL: Duration = Duration::from_secs(1);

struct Guard(Child);
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn handle(child: &Child) -> HANDLE {
    HANDLE(child.as_raw_handle())
}

/// Kernel plus user CPU time of the child.
fn cpu_time(child: &Child) -> Duration {
    let (mut c, mut e, mut k, mut u) = Default::default();
    // SAFETY: the handle belongs to a child this process owns; the four FILETIMEs are writable.
    unsafe { GetProcessTimes(handle(child), &mut c, &mut e, &mut k, &mut u) }
        .expect("GetProcessTimes");
    let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
    Duration::from_nanos((t(k) + t(u)) * 100)
}

/// (working set, private bytes) of the child.
fn memory(child: &Child) -> (u64, u64) {
    let mut m = PROCESS_MEMORY_COUNTERS_EX {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    // SAFETY: as above; the struct is the size the call is told about.
    unsafe { GetProcessMemoryInfo(handle(child), &mut m as *mut _ as *mut _, m.cb) }
        .expect("GetProcessMemoryInfo");
    (m.WorkingSetSize as u64, m.PrivateUsage as u64)
}

enum Seen {
    Frame(Option<u32>),
    Repaint(u32),
    Other,
}

enum Step {
    Page(&'static str),
    Suspend,
}

struct Phase {
    name: &'static str,
    step: Step,
}

struct Result {
    name: &'static str,
    cpu_machine: f64,
    cpu_core: f64,
    frames_per_sec: f64,
    working_set: u64,
    private: u64,
}

fn mb(b: u64) -> String {
    format!("{:.1} MB", b as f64 / 1_048_576.0)
}

#[test]
#[ignore]
fn processes_page_overhead() {
    let exe = std::env::var("ADDON_TEST_EXE").expect("set ADDON_TEST_EXE to the release exe");
    let data = std::env::var("OVERHEAD_DATA").expect("set OVERHEAD_DATA to a fresh folder");
    let secs: u64 = std::env::var("OVERHEAD_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let measured = Duration::from_secs(secs);
    let logical = std::thread::available_parallelism().map_or(1, |n| n.get());

    let mut child = Guard(
        Command::new(&exe)
            .env("TOOLBOKS_DATA_DIR", &data)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("could not start {exe}: {e}")),
    );
    let mut reader = FrameReader::new(child.0.stdout.take().unwrap());
    let mut writer = FrameWriter::new(child.0.stdin.take().unwrap());
    host_handshake(
        &mut reader,
        &mut writer,
        &Hello::new("toolboks.resource-monitor", "overhead"),
    )
    .unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        while let Ok(msg) = reader.read::<AddonMessage>() {
            let seen = match msg {
                AddonMessage::Frame(f) => Seen::Frame(f.repaint_after_ms),
                AddonMessage::RequestRepaint { after_ms } => Seen::Repaint(after_ms),
                _ => Seen::Other,
            };
            if tx.send(seen).is_err() {
                return;
            }
        }
    });
    writer
        .write(&HostMessage::LoadPreferences { json: "{}".into() })
        .unwrap();
    writer
        .write(&HostMessage::Capabilities(vec!["host_metrics".into()]))
        .unwrap();

    let phases = [
        Phase {
            name: "Overview shown",
            step: Step::Page("Overview"),
        },
        Phase {
            name: "Processes shown",
            step: Step::Page("Processes"),
        },
        Phase {
            name: "Hidden (suspended) after Processes",
            step: Step::Suspend,
        },
    ];
    let mut seq = 0u64;
    let mut suspended = false;
    let mut in_flight = false;
    let mut last_frame_sent: Option<Instant> = None;
    let mut next_repaint: Option<Instant>;
    let mut next_ping = Instant::now() + PING_INTERVAL;
    let mut nonce = 0u64;
    let mut results = Vec::new();

    for phase in &phases {
        match phase.step {
            Step::Page(name) => {
                let index = resource_monitor::PAGES
                    .iter()
                    .position(|p| *p == name)
                    .expect("page name");
                if suspended {
                    writer.write(&HostMessage::Resume).unwrap();
                    suspended = false;
                }
                writer
                    .write(&HostMessage::Navigate {
                        page: index.to_string(),
                    })
                    .unwrap();
                next_repaint = Some(Instant::now());
            }
            Step::Suspend => {
                writer.write(&HostMessage::Suspend).unwrap();
                suspended = true;
                next_repaint = None;
            }
        }
        let start = Instant::now();
        let measure_from = start + WARM_UP;
        let end = measure_from + measured;
        let mut cpu_start = None;
        let mut frames = 0u64;
        loop {
            let now = Instant::now();
            if cpu_start.is_none() && now >= measure_from {
                cpu_start = Some((cpu_time(&child.0), now));
                frames = 0;
            }
            if now >= end {
                break;
            }
            if now >= next_ping {
                nonce += 1;
                writer.write(&HostMessage::Ping { nonce }).unwrap();
                next_ping = now + PING_INTERVAL;
            }
            if !suspended && !in_flight && next_repaint.is_some_and(|t| now >= t) {
                seq += 1;
                writer
                    .write(&HostMessage::Input(InputBatch {
                        seq,
                        size: [1100.0, 700.0],
                        pixels_per_point: 1.0,
                        theme: Theme::Dark,
                        window_focused: true,
                        events: vec![],
                    }))
                    .unwrap();
                in_flight = true;
                last_frame_sent = Some(now);
                next_repaint = None;
            }
            let mut wake = end.min(next_ping);
            if cpu_start.is_none() {
                wake = wake.min(measure_from);
            }
            if let (false, false, Some(t)) = (suspended, in_flight, next_repaint) {
                wake = wake.min(t);
            }
            let schedule = |after_ms: u32, next: &mut Option<Instant>| {
                // Like the host's RepaintLimiter: at most one repaint per frame interval.
                let mut at = Instant::now() + Duration::from_millis(after_ms as u64);
                if let Some(last) = last_frame_sent {
                    at = at.max(last + FRAME_INTERVAL);
                }
                *next = Some(next.map_or(at, |n| n.min(at)));
            };
            match rx.recv_timeout(wake.saturating_duration_since(Instant::now())) {
                Ok(Seen::Frame(after)) => {
                    in_flight = false;
                    frames += 1;
                    if let Some(ms) = after {
                        schedule(ms, &mut next_repaint);
                    }
                }
                Ok(Seen::Repaint(ms)) => schedule(ms, &mut next_repaint),
                Ok(Seen::Other) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => panic!("the add-on exited"),
            }
        }
        let (cpu0, t0) = cpu_start.expect("measured");
        let cpu = cpu_time(&child.0).saturating_sub(cpu0).as_secs_f64();
        let wall = t0.elapsed().as_secs_f64();
        let (working_set, private) = memory(&child.0);
        results.push(Result {
            name: phase.name,
            cpu_core: cpu / wall * 100.0,
            cpu_machine: cpu / wall * 100.0 / logical as f64,
            frames_per_sec: frames as f64 / wall,
            working_set,
            private,
        });
    }
    let _ = writer.write(&HostMessage::Quit);

    println!(
        "\n{exe}\n{secs} s per phase after {} s warm-up, {logical} logical CPUs\n",
        WARM_UP.as_secs()
    );
    println!(
        "| Phase | CPU (of machine) | CPU (of one core) | Frames/s | Working set | Private bytes |"
    );
    println!("|---|---|---|---|---|---|");
    for r in &results {
        println!(
            "| {} | {:.2}% | {:.1}% | {:.1} | {} | {} |",
            r.name,
            r.cpu_machine,
            r.cpu_core,
            r.frames_per_sec,
            mb(r.working_set),
            mb(r.private)
        );
    }
}
