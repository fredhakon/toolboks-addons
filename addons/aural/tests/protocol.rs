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
        toolboks_addon_sdk::Package::parse(include_bytes!("../aural.tbaddon"), "0.12.5").unwrap();
    assert_eq!(package.manifest.id, "toolboks.aural");
    assert_eq!(package.manifest.id, aural::ID);
    assert_eq!(package.manifest.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(
        package.manifest.native_provider.as_deref(),
        Some("toolboks.aural")
    );
    let services: serde_json::Value =
        serde_json::from_str(include_str!("../host-services.json")).unwrap();
    assert_eq!(services["background"], true);
    assert_eq!(
        package.manifest.capabilities,
        [
            toolboks_addon_sdk::Capability::LiveAudio,
            toolboks_addon_sdk::Capability::LocalPreferences,
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
    let exe =
        std::env::var("ADDON_TEST_EXE").unwrap_or_else(|_| env!("CARGO_BIN_EXE_aural").into());
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
            &Hello::new("toolboks.aural", "test"),
        )
        .unwrap();
        assert_eq!(hello.addon_id, "toolboks.aural");
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
                "live_audio".into(),
                "local_preferences".into(),
            ]))
            .unwrap();
        for page in 0..1usize {
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
    rx.recv_timeout(Duration::from_secs(120))
        .expect("protocol test host timed out or failed");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(std::time::Instant::now() < deadline, "add-on did not quit");
        std::thread::sleep(Duration::from_millis(10));
    }
}
