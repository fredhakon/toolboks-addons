//! A real recording run, for the history checks (#43) and the overhead budget (#46). Ignored:
//! it takes minutes and measures this PC. It never opens a window.
//!
//!   set ADDON_TEST_EXE=<release resource-monitor.exe>   (default: the test build's exe)
//!   set RM_RUN_DIR=E:\somewhere\fresh-folder            (the isolated TOOLBOKS_DATA_DIR)
//!   set RM_RUN_SECS=600                                  (how long; default 120)
//!   set RM_RUN_RECORDING=1                               (0: the same run with recording off)
//!   set RM_RUN_DRAW=1                                    (draw every page once first)
//!   set RM_RUN_LOAD=1                                    (a known load: see below)
//!   cargo test --release -p resource-monitor --test recording_run -- --ignored --nocapture
//!
//! It starts the exe over the add-on protocol as the host does, sends the preferences, then
//! `Suspend` (the window hidden or another add-on open: recording is the only work left) and
//! samples the add-on's CPU time, private memory and bytes written every 10 s.
//!
//! With `RM_RUN_LOAD=1` it also runs two known loads for 60 s, starting 60 s in, as copies of
//! this test exe with their own names: `cpuload.exe` keeps one thread busy and `diskload.exe`
//! writes 25 MB per second to a file in `RM_RUN_DIR`. Their own CPU time and bytes written,
//! read from Windows (the counters Task Manager shows), are printed with the period, so the
//! history's breakdown for that period can be compared.

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use toolboks_addon_proto::{
    host_handshake, AddonMessage, FrameReader, FrameWriter, Hello, HostMessage, InputBatch, Theme,
};

struct Guard(Child);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Counters {
    cpu: Duration,
    private: u64,
    written: u64,
    writes: u64,
}

#[cfg(windows)]
fn counters(child: &Child) -> Counters {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::{FILETIME, HANDLE};
    use windows::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows::Win32::System::Threading::{GetProcessIoCounters, GetProcessTimes, IO_COUNTERS};
    let h = HANDLE(child.as_raw_handle());
    let (mut a, mut b, mut k, mut u) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    let mut mem = PROCESS_MEMORY_COUNTERS_EX::default();
    let mut io = IO_COUNTERS::default();
    // SAFETY: `h` is the child's open process handle (std keeps it until the child is dropped);
    // every out pointer points at a local of the right type and size.
    unsafe {
        let _ = GetProcessTimes(h, &mut a, &mut b, &mut k, &mut u);
        let _ = GetProcessMemoryInfo(
            h,
            &mut mem as *mut PROCESS_MEMORY_COUNTERS_EX as *mut PROCESS_MEMORY_COUNTERS,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        );
        let _ = GetProcessIoCounters(h, &mut io);
    }
    let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
    Counters {
        cpu: Duration::from_nanos((t(k) + t(u)) * 100),
        private: mem.PrivateUsage as u64,
        written: io.WriteTransferCount,
        writes: io.WriteOperationCount,
    }
}

fn utc_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

const LOAD_SECS: u64 = 60;

/// The known loads, run in a copy of this exe (`LOAD_MODE=cpu` or `disk`).
#[test]
#[ignore]
fn load_child() {
    let until = Instant::now() + Duration::from_secs(LOAD_SECS);
    match std::env::var("LOAD_MODE").as_deref() {
        Ok("cpu") => {
            let mut x: u64 = 1;
            while Instant::now() < until {
                for _ in 0..100_000 {
                    x = x
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                }
                std::hint::black_box(x);
            }
        }
        Ok("disk") => {
            let path = std::env::var("LOAD_FILE").unwrap();
            let mut f = std::fs::File::create(&path).unwrap();
            let chunk = vec![7u8; 1 << 20];
            let start = Instant::now();
            let mut mb = 0u64;
            while Instant::now() < until {
                // 25 MB per second, evenly.
                let due = (start.elapsed().as_secs_f64() * 25.0) as u64;
                while mb < due {
                    f.write_all(&chunk).unwrap();
                    mb += 1;
                    if mb.is_multiple_of(256) {
                        // Keep the file small: start over every 256 MB.
                        f = std::fs::File::create(&path).unwrap();
                    }
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            f.sync_all().unwrap();
            drop(f);
            let _ = std::fs::remove_file(&path);
        }
        _ => {}
    }
}

fn start_load(dir: &std::path::Path, name: &str, mode: &str) -> Guard {
    let exe = dir.join(format!("{name}.exe"));
    std::fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
    Guard(
        Command::new(&exe)
            .args(["--exact", "load_child", "--ignored", "--test-threads=1"])
            .env("LOAD_MODE", mode)
            .env("LOAD_FILE", dir.join("diskload.bin"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

#[test]
#[ignore]
fn record_and_measure() {
    let dir = std::path::PathBuf::from(std::env::var("RM_RUN_DIR").expect("RM_RUN_DIR"));
    std::fs::create_dir_all(&dir).unwrap();
    let secs = env_u64("RM_RUN_SECS", 120);
    let recording = env_u64("RM_RUN_RECORDING", 1) == 1;
    let draw = env_u64("RM_RUN_DRAW", 0) == 1;
    let load = env_u64("RM_RUN_LOAD", 0) == 1;
    let exe = std::env::var("ADDON_TEST_EXE")
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_resource-monitor").into());
    let mut child = Guard(
        Command::new(&exe)
            .env("TOOLBOKS_DATA_DIR", &dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut reader = FrameReader::new(child.0.stdout.take().unwrap());
    let mut writer = FrameWriter::new(child.0.stdin.take().unwrap());
    host_handshake(
        &mut reader,
        &mut writer,
        &Hello::new("toolboks.resource-monitor", "test"),
    )
    .unwrap();
    let prefs = serde_json::json!({ "history_recording": recording });
    writer
        .write(&HostMessage::LoadPreferences {
            json: prefs.to_string(),
        })
        .unwrap();
    writer
        .write(&HostMessage::Capabilities(vec![
            "host_metrics".into(),
            "local_preferences".into(),
        ]))
        .unwrap();
    // Answers are read on a thread so the add-on never blocks on a full pipe.
    let (frames, got) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        while let Ok(m) = reader.read::<AddonMessage>() {
            if let AddonMessage::Frame(f) = m {
                let _ = frames.send(f.seq);
            }
        }
    });
    if draw {
        for page in 0..resource_monitor::PAGES.len() {
            writer
                .write(&HostMessage::Navigate {
                    page: page.to_string(),
                })
                .unwrap();
            for k in 0..3u64 {
                let seq = page as u64 * 10 + k + 1;
                writer
                    .write(&HostMessage::Input(InputBatch {
                        seq,
                        size: [1000.0, 650.0],
                        pixels_per_point: 1.0,
                        theme: Theme::Dark,
                        window_focused: true,
                        events: vec![],
                    }))
                    .unwrap();
                while got.recv_timeout(Duration::from_secs(30)).unwrap() != seq {}
                std::thread::sleep(Duration::from_millis(300));
            }
        }
    }
    writer.write(&HostMessage::Suspend).unwrap();
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1) as f64;
    let start = Instant::now();
    let base = counters(&child.0);
    println!("recording={recording} draw={draw} load={load} secs={secs} cores={cores} exe={exe}");
    println!(
        "{:>6} {:>10} {:>12} {:>12} {:>8}",
        "t (s)", "cpu (s)", "private MB", "written KB", "writes"
    );
    let mut loads: Vec<(&str, Guard, i64)> = Vec::new();
    let mut load_period = None;
    let mut peak_private = base.private;
    let mut min_private = base.private;
    let mut tick = 0u64;
    while start.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_secs(1));
        let t = start.elapsed().as_secs();
        if load && t >= 60 && loads.is_empty() && load_period.is_none() {
            let at = utc_ms();
            loads.push(("cpuload", start_load(&dir, "cpuload", "cpu"), at));
            loads.push(("diskload", start_load(&dir, "diskload", "disk"), at));
        }
        if !loads.is_empty()
            && loads
                .iter_mut()
                .all(|(_, g, _)| g.0.try_wait().ok().flatten().is_some())
        {
            let from = loads[0].2;
            let to = utc_ms();
            println!(
                "known load ran from {from} to {to} (UTC ms, {} s)",
                (to - from) / 1000
            );
            for (name, g, _) in &loads {
                let c = counters(&g.0);
                let secs = (to - from) as f64 / 1000.0;
                println!(
                    "  {name}.exe: CPU {:.1} % of the whole PC ({:.1} s CPU), wrote {:.1} MB/s on average",
                    c.cpu.as_secs_f64() / secs / cores * 100.0,
                    c.cpu.as_secs_f64(),
                    c.written as f64 / secs / 1048576.0
                );
            }
            load_period = Some((from, to));
            loads.clear();
        }
        if t / 10 > tick {
            tick = t / 10;
            let c = counters(&child.0);
            peak_private = peak_private.max(c.private);
            min_private = min_private.min(c.private);
            println!(
                "{t:>6} {:>10.3} {:>12.1} {:>12.1} {:>8}",
                (c.cpu - base.cpu).as_secs_f64(),
                c.private as f64 / 1048576.0,
                (c.written - base.written) as f64 / 1024.0,
                c.writes - base.writes
            );
        }
    }
    let end = counters(&child.0);
    let wall = start.elapsed().as_secs_f64();
    let cpu = (end.cpu - base.cpu).as_secs_f64();
    println!(
        "SUMMARY recording={recording} draw={draw}: {wall:.0} s, CPU {cpu:.2} s = {:.3} % of one core = {:.3} % of the whole PC; private memory {:.1} to {:.1} MB; written {:.1} KB in {} writes ({:.1} writes per minute)",
        cpu / wall * 100.0,
        cpu / wall / cores * 100.0,
        min_private as f64 / 1048576.0,
        peak_private as f64 / 1048576.0,
        (end.written - base.written) as f64 / 1024.0,
        end.writes - base.writes,
        (end.writes - base.writes) as f64 / wall * 60.0
    );
    let _ = writer.write(&HostMessage::Quit);
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.0.try_wait().unwrap().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    if let Some((from, to)) = load_period {
        println!("REPORT: set RM_REPORT_DIR to {}\\Toolboks\\addon-data\\toolboks.resource-monitor\\history, RM_REPORT_FROM={from}, RM_REPORT_TO={to}", dir.display());
    }
}
