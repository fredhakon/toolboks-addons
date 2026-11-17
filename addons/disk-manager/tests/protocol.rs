// Adapted from the pinned host's sdk/examples/hello-addon/tests/stdio.rs (MIT).
// Uses the same headless protocol test host, with a bounded wait and child cleanup.
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;
use toolboks_addon_proto::{
    host_handshake, AddonMessage, Evidence, Finding, FindingStatus, FrameReader, FrameWriter,
    Hello, HostMessage, InputBatch, Theme,
};

struct Guard(Child, std::path::PathBuf);

#[test]
fn manifest_matches_the_executable() {
    let package =
        toolboks_addon_sdk::Package::parse(include_bytes!("../disk-manager.tbaddon"), "0.15.0")
            .unwrap();
    assert_eq!(package.manifest.id, "toolboks.disk-manager");
    assert_eq!(package.manifest.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(
        package.manifest.native_provider.as_deref(),
        Some("stratum.disk-manager")
    );
    let services: serde_json::Value =
        serde_json::from_str(include_str!("../host-services.json")).unwrap();
    assert_eq!(services["pages"], serde_json::json!(disk_manager::PAGES));
    assert_eq!(services["background"], true);
    assert_eq!(
        package.manifest.capabilities,
        [
            toolboks_addon_sdk::Capability::FilesystemRead,
            toolboks_addon_sdk::Capability::LocalPreferences,
            toolboks_addon_sdk::Capability::UserExports,
            toolboks_addon_sdk::Capability::DriveOptimize,
        ]
    );
}
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        remove_dir_with_retry(&self.1);
    }
}

/// Removes a test folder, retrying for up to 3 seconds because a child process (or the virus
/// scanner) may still hold a file for a moment. Errors at the end are ignored.
fn remove_dir_with_retry(dir: &std::path::Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while dir.exists() {
        if std::fs::remove_dir_all(dir).is_ok() || std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
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
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_disk-manager").into());
    let data = std::path::PathBuf::from(
        std::env::var_os("CARGO_TARGET_DIR")
            .unwrap_or_else(|| std::env::temp_dir().into_os_string()),
    )
    .join(format!("disk-manager-protocol-{}", std::process::id()));
    assert!(data.is_absolute());
    let mut child = Guard(
        Command::new(exe)
            .env("TOOLBOKS_DATA_DIR", &data)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
        data,
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
            &Hello::new("toolboks.disk-manager", "test"),
        )
        .unwrap();
        assert_eq!(hello.addon_id, "toolboks.disk-manager");
        assert_eq!(
            hello.protocol_version,
            toolboks_addon_proto::PROTOCOL_VERSION
        );
        let preferences = serde_json::json!({
            "settings": {"path":"C:\\saved", "exclude":"target", "min_bytes":1234, "older_days":9, "sort":"name"},
            "optimize_selected_drive":"Z:",
            "fragmentation_reports":{},
            "future_preference":"preserved"
        });
        writer
            .write(&HostMessage::LoadPreferences {
                json: preferences.to_string(),
            })
            .unwrap();
        writer
            .write(&HostMessage::Capabilities(vec![
                "filesystem_read".into(),
                "local_preferences".into(),
                "user_exports".into(),
                "drive_optimize".into(),
            ]))
            .unwrap();
        for page in 0..disk_manager::PAGES.len() {
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
                let mut expected = preferences["settings"].clone();
                expected["remember"] = serde_json::json!(true);
                assert_eq!(saved["settings"], expected);
                assert_eq!(saved["future_preference"], "preserved");
                assert!(saved.get("fragmentation_reports").is_some());
                assert!(saved.get("optimize_selected_drive").is_some());
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

/// The findings in `message`, if it carries them, checked against the agreed protocol: a
/// protocol 5 host gets `FindingsV5` (with `why` and `evidence`), a protocol 4 host the old
/// `Findings` layout without them, and never the other one.
fn findings_of(message: AddonMessage, protocol: u32) -> Option<Vec<Finding>> {
    match message {
        AddonMessage::FindingsV5(list) => {
            assert_eq!(protocol, 5, "FindingsV5 sent to a protocol {protocol} host");
            Some(list)
        }
        AddonMessage::Findings(list) => {
            assert_eq!(
                protocol, 4,
                "old Findings layout sent to a protocol {protocol} host"
            );
            Some(list.into_iter().map(Finding::from).collect())
        }
        _ => None,
    }
}

/// Suspended like Home runs it, the add-on reads this PC's fixed drives in the background and
/// reports drive health and free space. A protocol 5 host gets each free-space finding with the
/// bar of what is used and free, and `why` on every finding that needs attention; a protocol 4
/// host (Toolboks 0.15) gets the old `Findings` layout without them. Read-only.
#[test]
fn findings_follow_the_host_protocol() {
    if running_elevated() {
        eprintln!(
            "skipped: this process is elevated and the add-on runtime refuses to run elevated"
        );
        return;
    }
    for protocol in [5, 4] {
        let exe = std::env::var("ADDON_TEST_EXE")
            .unwrap_or_else(|_| env!("CARGO_BIN_EXE_disk-manager").into());
        let data = std::path::PathBuf::from(
            std::env::var_os("CARGO_TARGET_DIR")
                .unwrap_or_else(|| std::env::temp_dir().into_os_string()),
        )
        .join(format!(
            "disk-manager-findings-{protocol}-{}",
            std::process::id()
        ));
        let mut child = Guard(
            Command::new(exe)
                .env("TOOLBOKS_DATA_DIR", &data)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
            data,
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
                &Hello::with_version("toolboks.disk-manager", "test", protocol),
            )
            .unwrap();
            assert_eq!(hello.protocol_version, protocol);
            writer.write(&HostMessage::Suspend).unwrap();
            // Wait for the free-space findings (a quick read of every fixed drive).
            let list = loop {
                if let Some(list) = findings_of(reader.read().unwrap(), protocol) {
                    if list.iter().any(|f| f.id.starts_with("free-space-")) {
                        break list;
                    }
                }
            };
            for f in &list {
                assert!(Finding::valid_id(&f.id), "{}", f.id);
                let attention = f.status == FindingStatus::Attention;
                if protocol == 4 {
                    assert_eq!((f.why.as_ref(), f.evidence.as_ref()), (None, None));
                    continue;
                }
                assert_eq!(f.why.is_some(), attention, "{f:?}");
                if let Some(why) = &f.why {
                    assert!(why.chars().count() <= 200, "{why}");
                }
                if f.id.starts_with("free-space-") && f.status != FindingStatus::Unavailable {
                    let Some(Evidence::Bar {
                        fraction,
                        left,
                        right,
                    }) = &f.evidence
                    else {
                        panic!("free space without its bar: {f:?}");
                    };
                    assert!((0.0..=1.0).contains(fraction));
                    assert!(left.ends_with(" used") && right.ends_with(" free"), "{f:?}");
                }
            }
            writer.write(&HostMessage::Quit).unwrap();
            while let Ok(message) = reader.read::<AddonMessage>() {
                let _ = findings_of(message, protocol);
            }
            tx.send(()).unwrap();
        });
        rx.recv_timeout(Duration::from_secs(30))
            .expect("protocol test host timed out or failed");
    }
}
