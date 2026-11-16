// Adapted from the pinned host's sdk/examples/hello-addon/tests/stdio.rs (MIT).
// Uses the same headless protocol test host, with a bounded wait and child cleanup.
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;
use toolboks_addon_proto::{
    host_handshake, AddonMessage, FrameReader, FrameWriter, Hello, HostMessage, InputBatch, Theme,
};

struct Guard(Child);

#[test]
fn manifest_matches_the_executable() {
    let package =
        toolboks_addon_sdk::Package::parse(include_bytes!("../event-watch.tbaddon"), "0.15.0")
            .unwrap();
    assert_eq!(package.manifest.id, "toolboks.event-watch");
    assert_eq!(package.manifest.id, event_watch::ID);
    assert_eq!(package.manifest.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(
        package.manifest.native_provider.as_deref(),
        Some("toolboks.event-watch")
    );
    let services: serde_json::Value =
        serde_json::from_str(include_str!("../host-services.json")).unwrap();
    assert_eq!(services["pages"], serde_json::json!(event_watch::PAGES));
    assert_eq!(
        package.manifest.capabilities,
        [
            toolboks_addon_sdk::Capability::FilesystemRead,
            toolboks_addon_sdk::Capability::LocalPreferences,
            toolboks_addon_sdk::Capability::Administrator,
        ]
    );
}
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// GitHub's Windows runners run elevated. The add-on drawing runtime refuses to run elevated by
/// design (administrator changes go through the separate helper), so there the exe exits at once
/// and this test cannot start it. It still runs on every normal, unelevated PC.
fn running_elevated() -> bool {
    Command::new("whoami")
        .arg("/groups")
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).contains("S-1-16-12288"))
        .unwrap_or(false)
}

#[test]
fn protocol_test_host_draws_pings_and_quits() {
    if running_elevated() {
        eprintln!(
            "skipped: this process is elevated and the add-on runtime refuses to run elevated"
        );
        return;
    }
    // CI can exercise the exact release executable, rather than Cargo's test build.
    let exe = std::env::var("ADDON_TEST_EXE")
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_event-watch").into());
    let mut child = Guard(
        Command::new(exe)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stdout = child.0.stdout.take().unwrap();
    let stdin = child.0.stdin.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = FrameReader::new(stdout);
        let mut writer = FrameWriter::new(stdin);
        let hello = host_handshake(
            &mut reader,
            &mut writer,
            &Hello::new("toolboks.event-watch", "test"),
        )
        .unwrap();
        assert_eq!(hello.addon_id, "toolboks.event-watch");
        assert_eq!(
            hello.protocol_version,
            toolboks_addon_proto::PROTOCOL_VERSION
        );
        let preferences = serde_json::json!({"future_preference":"preserved"});
        writer
            .write(&HostMessage::LoadPreferences {
                json: preferences.to_string(),
            })
            .unwrap();
        writer
            .write(&HostMessage::Capabilities(vec![
                "filesystem_read".into(),
                "local_preferences".into(),
            ]))
            .unwrap();
        for page in 0..event_watch::PAGES.len() {
            writer
                .write(&HostMessage::Navigate {
                    page: page.to_string(),
                })
                .unwrap();
            writer
                .write(&HostMessage::Input(InputBatch {
                    seq: page as u64 + 1,
                    size: [1000.0, 650.0],
                    pixels_per_point: 1.0,
                    theme: Theme::Dark,
                    window_focused: true,
                    events: vec![],
                }))
                .unwrap();
            loop {
                if let AddonMessage::Frame(frame) = reader.read().unwrap() {
                    assert_eq!(frame.seq, page as u64 + 1);
                    assert!(!frame.meshes.is_empty());
                    break;
                }
            }
        }
        writer.write(&HostMessage::Suspend).unwrap();
        writer.write(&HostMessage::SavePreferences).unwrap();
        loop {
            if let AddonMessage::Preferences { json, .. } = reader.read().unwrap() {
                let saved: serde_json::Value = serde_json::from_str(&json).unwrap();
                assert_eq!(saved["future_preference"], "preserved");
                break;
            }
        }
        writer.write(&HostMessage::Resume).unwrap();
        writer.write(&HostMessage::Ping { nonce: 7 }).unwrap();
        loop {
            if let AddonMessage::Pong { nonce } = reader.read().unwrap() {
                assert_eq!(nonce, 7);
                break;
            }
        }
        writer.write(&HostMessage::Quit).unwrap();
        // Drain until closed so a GUI-subsystem executable has flushed its answers.
        while reader.read::<AddonMessage>().is_ok() {}
        tx.send(()).unwrap();
    });
    rx.recv_timeout(Duration::from_secs(20))
        .expect("protocol test host timed out or failed");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(std::time::Instant::now() < deadline, "add-on did not quit");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Event Watch runs in the background: suspended, with no page ever drawn, it reads the Windows
/// event log once and reports `recent-crashes`, then stays quiet (no further findings, no
/// repeated reads) until its next check, 30 minutes later.
#[test]
fn background_check_reports_a_finding_while_suspended_then_goes_quiet() {
    use toolboks_addon_proto::FindingStatus;
    if running_elevated() {
        eprintln!(
            "skipped: this process is elevated and the add-on runtime refuses to run elevated"
        );
        return;
    }
    let services: serde_json::Value =
        serde_json::from_str(include_str!("../host-services.json")).unwrap();
    assert_eq!(services["background"], serde_json::json!(true));

    let exe = std::env::var("ADDON_TEST_EXE")
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_event-watch").into());
    let mut child = Guard(
        Command::new(exe)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stdout = child.0.stdout.take().unwrap();
    let stdin = child.0.stdin.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = FrameReader::new(stdout);
        let mut writer = FrameWriter::new(stdin);
        host_handshake(
            &mut reader,
            &mut writer,
            &Hello::new("toolboks.event-watch", "test"),
        )
        .unwrap();
        writer.write(&HostMessage::Suspend).unwrap();
        // Findings arrive from background ticks while suspended: first "not checked", then the
        // real reading once the read has finished.
        let mut sent = Vec::new();
        let finding = loop {
            if let AddonMessage::Findings(list) = reader.read().unwrap() {
                assert_eq!(list.len(), 1);
                assert_eq!(list[0].id, "recent-crashes");
                sent.push(list[0].status);
                if list[0].status != FindingStatus::NotChecked {
                    break list[0].clone();
                }
            }
        };
        assert!(!finding.title.is_empty() && !finding.observation.is_empty());
        // Quiet now: a ping is answered and no second finding message comes before the pong.
        writer.write(&HostMessage::Ping { nonce: 9 }).unwrap();
        loop {
            match reader.read().unwrap() {
                AddonMessage::Pong { nonce } => {
                    assert_eq!(nonce, 9);
                    break;
                }
                AddonMessage::Findings(_) => panic!("a second finding with no new check"),
                _ => {}
            }
        }
        // `CheckNow` reads again at once. The reading carries its time in seconds and the add-on
        // only sends a changed finding, so let a second pass first.
        std::thread::sleep(Duration::from_millis(1100));
        writer.write(&HostMessage::CheckNow).unwrap();
        let again = loop {
            if let AddonMessage::Findings(list) = reader.read().unwrap() {
                break list;
            }
        };
        assert_eq!(again[0].id, "recent-crashes");
        assert!(again[0].measured_at >= finding.measured_at);
        writer.write(&HostMessage::Quit).unwrap();
        while reader.read::<AddonMessage>().is_ok() {}
        tx.send(()).unwrap();
    });
    rx.recv_timeout(Duration::from_secs(60))
        .expect("protocol test host timed out or failed");
}

/// Not part of the gate: holds a suspended Event Watch (check done) still, for an outside
/// process to measure its CPU and wake-ups. Needs `ADDON_TEST_EXE`, `MEASURE_READY_FILE` (written
/// with the process id once the first check has finished) and optionally `MEASURE_SECS` (default
/// 60) and `MEASURE_PING=0` to send nothing at all (default: one ping a second, as the host's
/// heartbeat does). Prints how many messages the add-on sent while held.
#[test]
#[ignore]
fn hold_suspended_for_measuring() {
    let exe = std::env::var("ADDON_TEST_EXE").expect("ADDON_TEST_EXE");
    let ready = std::env::var("MEASURE_READY_FILE").expect("MEASURE_READY_FILE");
    let secs: u64 = std::env::var("MEASURE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let ping = std::env::var("MEASURE_PING").map_or(true, |v| v != "0");
    let mut child = Guard(
        Command::new(exe)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let pid = child.0.id();
    let stdout = child.0.stdout.take().unwrap();
    let stdin = child.0.stdin.take().unwrap();
    let mut reader = FrameReader::new(stdout);
    let mut writer = FrameWriter::new(stdin);
    host_handshake(
        &mut reader,
        &mut writer,
        &Hello::new("toolboks.event-watch", "measure"),
    )
    .unwrap();
    writer.write(&HostMessage::Suspend).unwrap();
    let started = std::time::Instant::now();
    loop {
        if let AddonMessage::Findings(list) = reader.read().unwrap() {
            if list[0].status != toolboks_addon_proto::FindingStatus::NotChecked {
                println!(
                    "first check done after {:?}: {:?} - {}",
                    started.elapsed(),
                    list[0].status,
                    list[0].title
                );
                break;
            }
        }
    }
    // Let the add-on settle (a few more seconds so the read's threads are gone).
    std::thread::sleep(Duration::from_secs(5));
    std::fs::write(&ready, pid.to_string()).unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        while let Ok(message) = reader.read::<AddonMessage>() {
            if tx.send(message).is_err() {
                break;
            }
        }
    });
    let end = std::time::Instant::now() + Duration::from_secs(secs);
    let (mut pongs, mut other) = (0, 0);
    let mut nonce = 0;
    while std::time::Instant::now() < end {
        if ping {
            nonce += 1;
            writer.write(&HostMessage::Ping { nonce }).unwrap();
        }
        std::thread::sleep(Duration::from_secs(1));
        while let Ok(message) = rx.try_recv() {
            match message {
                AddonMessage::Pong { .. } => pongs += 1,
                _ => other += 1,
            }
        }
    }
    println!("held {secs} s, ping={ping}: {pongs} pongs, {other} other messages from the add-on");
    writer.write(&HostMessage::Quit).unwrap();
}
