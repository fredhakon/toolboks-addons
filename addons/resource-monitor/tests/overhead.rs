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
//! `OVERHEAD_SECS` (default 60) is the measured time per phase, after a 10 s warm-up. Each
//! phase also prints the CPU use of every named thread of the add-on. CPU is counted in
//! processor cycles (`QueryProcessCycleTime`, `QueryThreadCycleTime`), not in CPU time: Windows
//! charges CPU time in whole 15.6 ms clock ticks, so a thread that wakes for a few milliseconds
//! a second reads as 0 % in one run and 1.6 % in the next. Cycles are converted to "% of one
//! core" with the cycle rate measured by spinning one thread for 300 ms at the start. History
//! recording runs, as by default.
//! `OVERHEAD_DATA` is the data folder handed to the add-on (`TOOLBOKS_DATA_DIR`), so the history
//! recorder never writes into a real Toolboks folder.

use std::collections::{BTreeMap, HashMap};
use std::os::windows::io::AsRawHandle;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use toolboks_addon_proto::{
    host_handshake, AddonMessage, FrameReader, FrameWriter, Hello, HostMessage, InputBatch, Theme,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::Threading::{
    GetCurrentThread, GetThreadDescription, OpenThread, THREAD_QUERY_LIMITED_INFORMATION,
};

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

#[link(name = "kernel32")]
extern "system" {
    fn QueryProcessCycleTime(process: HANDLE, cycles: *mut u64) -> i32;
    fn QueryThreadCycleTime(thread: HANDLE, cycles: *mut u64) -> i32;
}

/// Processor cycles the child has used, all its threads (also ended ones) together.
fn cycles(child: &Child) -> u64 {
    let mut c = 0u64;
    // SAFETY: the handle belongs to a child this process owns; `c` is writable.
    assert!(unsafe { QueryProcessCycleTime(handle(child), &mut c) } != 0);
    c
}

/// Cycles per second of one busy core, measured by spinning this thread for 300 ms.
fn cycle_rate() -> f64 {
    let read = || {
        let mut c = 0u64;
        // SAFETY: the pseudo handle of the current thread is always valid.
        unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut c) };
        c
    };
    let (c0, t0) = (read(), Instant::now());
    while t0.elapsed() < Duration::from_millis(300) {
        std::hint::spin_loop();
    }
    (read() - c0) as f64 / t0.elapsed().as_secs_f64()
}

/// Thread id -> (name, cycles used) for every thread of process `pid`. Unnamed threads are
/// "(unnamed)".
fn threads(pid: u32) -> HashMap<u32, (String, u64)> {
    let mut out = HashMap::new();
    // SAFETY: a snapshot handle is closed below; THREADENTRY32 has its size set.
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) else {
            return out;
        };
        let mut e = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let mut more = Thread32First(snap, &mut e).is_ok();
        while more {
            if e.th32OwnerProcessID == pid {
                if let Ok(h) = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, false, e.th32ThreadID) {
                    let name = GetThreadDescription(h)
                        .ok()
                        .and_then(|p| p.to_string().ok())
                        .filter(|n| !n.is_empty())
                        .unwrap_or_else(|| "(unnamed)".into());
                    let mut c = 0u64;
                    if QueryThreadCycleTime(h, &mut c) != 0 {
                        out.insert(e.th32ThreadID, (name, c));
                    }
                    let _ = CloseHandle(h);
                }
            }
            more = Thread32Next(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    out
}

/// Cycles per thread name between two snapshots (threads that started in between count from
/// zero; threads that ended in between are missing and show up in the process total only).
fn per_name(
    before: &HashMap<u32, (String, u64)>,
    after: &HashMap<u32, (String, u64)>,
) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    for (id, (name, c)) in after {
        let was = before.get(id).filter(|(n, _)| n == name).map_or(0, |b| b.1);
        *out.entry(name.clone()).or_insert(0) += c.saturating_sub(was);
    }
    out
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
    threads: BTreeMap<String, u64>,
    wall: f64,
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
    let rate = cycle_rate();

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
            name: "History shown",
            step: Step::Page("History"),
        },
        Phase {
            name: "Hidden (suspended)",
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
                cpu_start = Some((cycles(&child.0), now, threads(child.0.id())));
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
        let (cpu0, t0, threads0) = cpu_start.expect("measured");
        let cpu = cycles(&child.0).saturating_sub(cpu0) as f64 / rate;
        let by_thread = per_name(&threads0, &threads(child.0.id()));
        let wall = t0.elapsed().as_secs_f64();
        let (working_set, private) = memory(&child.0);
        results.push(Result {
            name: phase.name,
            cpu_core: cpu / wall * 100.0,
            cpu_machine: cpu / wall * 100.0 / logical as f64,
            frames_per_sec: frames as f64 / wall,
            working_set,
            private,
            threads: by_thread,
            wall,
        });
    }
    let _ = writer.write(&HostMessage::Quit);

    println!(
        "\n{exe}\n{secs} s per phase after {} s warm-up, {logical} logical CPUs, {:.2} GHz cycle rate\n",
        WARM_UP.as_secs(),
        rate / 1e9
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
    println!("\nCPU per thread, % of one core:\n");
    let names: std::collections::BTreeSet<&String> =
        results.iter().flat_map(|r| r.threads.keys()).collect();
    let head: Vec<&str> = results.iter().map(|r| r.name).collect();
    println!("| Thread | {} |", head.join(" | "));
    println!("|---|{}", "---|".repeat(results.len()));
    for name in names {
        let cells: Vec<String> = results
            .iter()
            .map(|r| {
                let c = r.threads.get(name).copied().unwrap_or_default();
                format!("{:.2}%", c as f64 / rate / r.wall * 100.0)
            })
            .collect();
        println!("| {name} | {} |", cells.join(" | "));
    }
}
