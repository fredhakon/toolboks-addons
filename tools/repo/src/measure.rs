//! Reproducible local release-exe measurements. No catalog writes or signatures.
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};
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

pub(super) fn run(name: &str) {
    let manifest = super::package(name).manifest;
    let build = Command::new("cargo")
        .args([
            "build",
            "--locked",
            "--release",
            "-p",
            name,
            "--message-format=json",
        ])
        .output()
        .expect("cargo build");
    eprint!("{}", String::from_utf8_lossy(&build.stderr));
    assert!(build.status.success(), "release build failed");
    let exe = String::from_utf8_lossy(&build.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find_map(|value| {
            (value["reason"] == "compiler-artifact" && value["target"]["name"] == name)
                .then(|| value["executable"].as_str().map(PathBuf::from))
                .flatten()
        })
        .expect("release executable artifact");
    let samples: Vec<_> = (0..3).map(|_| sample(&exe, &manifest.id)).collect();
    println!("{}", serde_json::to_string_pretty(&serde_json::json!({
        "addon": name,
        "protocol": toolboks_addon_proto::PROTOCOL_VERSION,
        "exe": exe,
        "exe_bytes": std::fs::metadata(&exe).unwrap().len(),
        "method": "Three sequential local launches over stdio, without clearing OS caches; spawn to first 1000x650 frame; suspended idle after 5 s. Working set and private commit are process-only; not the named-pipe installer or host UI.",
        "samples": samples,
    })).unwrap());
}

fn sample(exe: &Path, id: &str) -> serde_json::Value {
    let started = Instant::now();
    let mut child = Guard(
        Command::new(exe)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stdout = child.0.stdout.take().unwrap();
    let stdin = child.0.stdin.take().unwrap();
    let id = id.to_owned();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (quit_tx, quit_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = FrameReader::new(stdout);
        let mut writer = FrameWriter::new(stdin);
        host_handshake(&mut reader, &mut writer, &Hello::new(&id, "measure")).unwrap();
        writer
            .write(&HostMessage::Input(InputBatch {
                seq: 1,
                size: [1000.0, 650.0],
                pixels_per_point: 1.0,
                theme: Theme::Dark,
                window_focused: true,
                events: vec![],
            }))
            .unwrap();
        loop {
            if let AddonMessage::Frame(frame) = reader.read().unwrap() {
                assert_eq!(frame.seq, 1);
                assert!(!frame.meshes.is_empty());
                break;
            }
        }
        let first_frame_ms = started.elapsed().as_secs_f64() * 1000.0;
        writer.write(&HostMessage::Suspend).unwrap();
        ready_tx.send(first_frame_ms).unwrap();
        quit_rx.recv_timeout(Duration::from_secs(20)).unwrap();
        writer.write(&HostMessage::Quit).unwrap();
        while reader.read::<AddonMessage>().is_ok() {}
        let _ = done_tx.send(());
    });
    let first_frame_ms = ready_rx
        .recv_timeout(Duration::from_secs(20))
        .expect("first frame timed out");
    std::thread::sleep(Duration::from_secs(5));
    let memory = memory(child.0.id());
    quit_tx.send(()).unwrap();
    done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("quit timed out");
    serde_json::json!({"first_frame_ms": first_frame_ms, "idle_memory": memory})
}

#[cfg(windows)]
fn memory(pid: u32) -> serde_json::Value {
    use windows::Win32::{
        Foundation::CloseHandle,
        System::{
            ProcessStatus::{
                GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
            },
            Threading::{OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ},
        },
    };
    // SAFETY: the process is ours, the handle is closed, and the EX structure has the
    // documented counters prefix and remains writable for its full advertised size.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid).unwrap();
        let mut counters = PROCESS_MEMORY_COUNTERS_EX::default();
        let result = GetProcessMemoryInfo(
            handle,
            (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX).cast::<PROCESS_MEMORY_COUNTERS>(),
            std::mem::size_of_val(&counters) as u32,
        );
        let _ = CloseHandle(handle);
        result.unwrap();
        serde_json::json!({"working_set_bytes": counters.WorkingSetSize, "private_bytes": counters.PrivateUsage})
    }
}

#[cfg(not(windows))]
fn memory(_: u32) -> serde_json::Value {
    serde_json::Value::Null
}
