use std::{fs, process::Command};
use toolboks_catalog::Index;

#[test]
fn catalog_update_uses_released_bytes_and_preserves_other_versions() {
    // Removed when the test ends, also when it fails or panics.
    let tmp = tempfile::Builder::new()
        .prefix("toolboks-addon-catalog-")
        .tempdir()
        .unwrap();
    let root = tmp.path().to_path_buf();
    fs::create_dir_all(root.join("addons/hello")).unwrap();
    fs::create_dir(root.join("catalog")).unwrap();
    let mut manifest: serde_json::Value =
        serde_json::from_slice(include_bytes!("../../../addons/hello/hello.tbaddon")).unwrap();
    manifest["manifest"]["version"] = "0.1.0".into();
    manifest["manifest"]["byline"] = "by test author".into();
    manifest["manifest"]["min_toolboks_version"] = "0.12.5".into();
    fs::write(
        root.join("addons/hello/hello.tbaddon"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    fs::write(
        root.join("catalog/index.json"),
        br#"{"format_version":1,"entries":[]}"#,
    )
    .unwrap();
    fs::write(root.join("hello.exe"), b"release fixture").unwrap();
    fs::write(root.join("addons/hello/host-services.json"), br#"{"pages":["Overview","Largest files","File types","Scan details","Optimization"],"background":true}"#).unwrap();
    let update = |version: &str| {
        Command::new(env!("CARGO_BIN_EXE_addon-repo"))
            .current_dir(&root)
            .args([
                "catalog-update",
                "hello",
                version,
                "new-owner/addons",
                "hello.exe",
            ])
            .output()
            .unwrap()
    };
    assert!(update("0.1.0").status.success());
    let first = fs::read(root.join("catalog/index.json")).unwrap();
    let index = Index::parse(&first).unwrap();
    let entry = &index.entries[0];
    assert_eq!(entry.size, 15);
    assert_eq!(
        entry.sha256,
        toolboks_catalog::sha256_hex(b"release fixture")
    );
    assert_eq!(
        entry.protocol_version,
        toolboks_addon_proto::PROTOCOL_VERSION
    );
    // The manifest needs 0.12.5, but the entry speaks protocol 5, which only 0.16.0 and later do.
    assert_eq!(entry.protocol_version, 5);
    assert_eq!(entry.min_toolboks_version, "0.16.0");
    assert_eq!(
        entry.download_url,
        "https://github.com/new-owner/addons/releases/download/hello-v0.1.0/hello.exe"
    );
    assert!(!entry.verified);
    assert_eq!(entry.byline.as_deref(), Some("by test author"));
    assert_eq!(
        entry.pages,
        [
            "Overview",
            "Largest files",
            "File types",
            "Scan details",
            "Optimization"
        ]
    );
    assert!(entry.background);
    assert!(!update("0.2.0").status.success());
    assert_eq!(fs::read(root.join("catalog/index.json")).unwrap(), first);
    // A subsequent version retains the already released version in the plain list.
    manifest["manifest"]["version"] = "0.2.0".into();
    fs::write(
        root.join("addons/hello/hello.tbaddon"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    assert!(update("0.2.0").status.success());
    let next = Index::parse(&fs::read(root.join("catalog/index.json")).unwrap()).unwrap();
    assert_eq!(next.entries.len(), 2);
    assert_eq!(next.entries[0], index.entries[0]);
    let value: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("catalog/index.json")).unwrap()).unwrap();
    assert_eq!(value.as_object().unwrap().len(), 2);
    assert_eq!(value["format_version"], 1);
    assert!(value.get("entries").is_some());
    assert_eq!(fs::read_dir(root.join("catalog")).unwrap().count(), 1);
}

/// `catalog-update` copies the manifest's data note into the entry, leaves it out when the
/// manifest has none, refuses a bad one without touching the list, and `validate` fails when the
/// entry for the current version carries a different note.
#[test]
fn data_note_is_copied_checked_and_validated() {
    const NOTE: &str = "Keeps your last scan of each drive on this PC (file and folder names).";
    let tmp = tempfile::Builder::new()
        .prefix("toolboks-addon-data-note-")
        .tempdir()
        .unwrap();
    let root = tmp.path().to_path_buf();
    fs::create_dir_all(root.join("addons/hello")).unwrap();
    fs::create_dir(root.join("catalog")).unwrap();
    fs::write(root.join("hello.exe"), b"release fixture").unwrap();
    let empty: &[u8] = br#"{"format_version":1,"entries":[]}"#;
    fs::write(root.join("catalog/index.json"), empty).unwrap();
    let mut manifest: serde_json::Value =
        serde_json::from_slice(include_bytes!("../../../addons/hello/hello.tbaddon")).unwrap();
    manifest["manifest"]["min_toolboks_version"] = "0.14.0".into();
    // `validate` checks catalog icons; Hello's own icon is not one the sidebar draws.
    manifest["manifest"]["icon"] = "chip".into();
    let write = |manifest: &serde_json::Value| {
        fs::write(
            root.join("addons/hello/hello.tbaddon"),
            serde_json::to_vec(manifest).unwrap(),
        )
        .unwrap()
    };
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_addon-repo"))
            .current_dir(&root)
            .args(args)
            .output()
            .unwrap()
    };
    let update = |version: &str| {
        run(&[
            "catalog-update",
            "hello",
            version,
            "owner/addons",
            "hello.exe",
        ])
    };
    let index = || Index::parse(&fs::read(root.join("catalog/index.json")).unwrap()).unwrap();

    // No note in the manifest: none in the entry, and none written to the file.
    manifest["manifest"]["version"] = "1.0.0".into();
    write(&manifest);
    assert!(update("1.0.0").status.success());
    assert_eq!(index().entries[0].data_note, None);
    assert!(
        !String::from_utf8(fs::read(root.join("catalog/index.json")).unwrap())
            .unwrap()
            .contains("data_note")
    );
    assert!(run(&["validate"]).status.success());

    // A note in the manifest is copied as it is.
    manifest["manifest"]["version"] = "1.1.0".into();
    manifest["manifest"]["data_note"] = NOTE.into();
    write(&manifest);
    assert!(update("1.1.0").status.success());
    let entries = index().entries;
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].data_note, None);
    assert_eq!(entries[1].data_note.as_deref(), Some(NOTE));
    assert!(run(&["validate"]).status.success());

    // A bad note is refused and the list stays as it was.
    let before = fs::read(root.join("catalog/index.json")).unwrap();
    manifest["manifest"]["version"] = "1.2.0".into();
    for bad in [
        "a".repeat(161),
        "Keeps \u{202e}snacs.".into(),
        "Keeps\u{200b}scans.".into(),
        "Keeps\nscans.".into(),
        " Keeps scans.".into(),
    ] {
        manifest["manifest"]["data_note"] = bad.clone().into();
        write(&manifest);
        assert!(!update("1.2.0").status.success(), "{bad:?}");
        assert_eq!(fs::read(root.join("catalog/index.json")).unwrap(), before);
    }

    // The entry of the current version must carry the manifest's note.
    manifest["manifest"]["version"] = "1.1.0".into();
    manifest["manifest"]["data_note"] = "Keeps something else.".into();
    write(&manifest);
    let out = run(&["validate"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("differs"));
    manifest["manifest"]
        .as_object_mut()
        .unwrap()
        .remove("data_note");
    write(&manifest);
    assert!(!run(&["validate"]).status.success());

    // Older versions are not compared: their manifest is the one at their release tag.
    manifest["manifest"]["version"] = "2.0.0".into();
    manifest["manifest"]["data_note"] = "A newer note.".into();
    write(&manifest);
    assert!(run(&["validate"]).status.success());
}

/// `catalog-update` copies the manifest's health check names and gives the entry the first host
/// that speaks its protocol; `validate` fails when the current version's entry differs from the
/// manifest, and leaves entries of older versions released at protocol 3 alone.
#[test]
fn checks_and_minimum_host_are_copied_and_validated() {
    let tmp = tempfile::Builder::new()
        .prefix("toolboks-addon-checks-")
        .tempdir()
        .unwrap();
    let root = tmp.path().to_path_buf();
    fs::create_dir_all(root.join("addons/hello")).unwrap();
    fs::create_dir(root.join("catalog")).unwrap();
    fs::write(root.join("hello.exe"), b"release fixture").unwrap();
    let mut manifest: serde_json::Value =
        serde_json::from_slice(include_bytes!("../../../addons/hello/hello.tbaddon")).unwrap();
    manifest["manifest"]["min_toolboks_version"] = "0.12.5".into();
    manifest["manifest"]["icon"] = "chip".into();
    // An entry released before the repin: protocol 3, the manifest's own minimum.
    manifest["manifest"]["version"] = "0.9.0".into();
    fs::write(
        root.join("addons/hello/hello.tbaddon"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_addon-repo"))
            .current_dir(&root)
            .args(args)
            .output()
            .unwrap()
    };
    fs::write(
        root.join("catalog/index.json"),
        br#"{"format_version":1,"entries":[]}"#,
    )
    .unwrap();
    assert!(run(&[
        "catalog-update",
        "hello",
        "0.9.0",
        "owner/addons",
        "hello.exe"
    ])
    .status
    .success());
    let mut old: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("catalog/index.json")).unwrap()).unwrap();
    old["entries"][0]["protocol_version"] = 3.into();
    old["entries"][0]["min_toolboks_version"] = "0.12.5".into();
    let old = serde_json::to_vec_pretty(&old).unwrap();
    fs::write(root.join("catalog/index.json"), &old).unwrap();
    // The current manifest still describes 0.9.0, and protocol 3 needs nothing newer.
    assert!(run(&["validate"]).status.success());

    let write = |manifest: &serde_json::Value| {
        fs::write(
            root.join("addons/hello/hello.tbaddon"),
            serde_json::to_vec(manifest).unwrap(),
        )
        .unwrap()
    };
    let update = || {
        run(&[
            "catalog-update",
            "hello",
            "1.0.0",
            "owner/addons",
            "hello.exe",
        ])
    };
    let read = || -> serde_json::Value {
        serde_json::from_slice(&fs::read(root.join("catalog/index.json")).unwrap()).unwrap()
    };

    // A bad list of checks is refused and the list stays as it was.
    manifest["manifest"]["version"] = "1.0.0".into();
    for bad in [
        serde_json::json!(["a".repeat(41)]),
        serde_json::json!(["1", "2", "3", "4", "5", "6", "7"]),
        serde_json::json!(["Recent crashes", "Recent crashes"]),
        serde_json::json!([" Recent crashes"]),
        serde_json::json!(["Recent\u{200b}crashes"]),
    ] {
        manifest["manifest"]["checks"] = bad.clone();
        write(&manifest);
        assert!(!update().status.success(), "{bad}");
        assert_eq!(fs::read(root.join("catalog/index.json")).unwrap(), old);
    }

    // Good checks are copied, the old entry is kept byte for byte.
    manifest["manifest"]["checks"] = serde_json::json!(["Recent crashes", "Disk health"]);
    write(&manifest);
    assert!(update().status.success());
    let list = read();
    assert_eq!(
        list["entries"][0],
        serde_json::from_slice::<serde_json::Value>(&old).unwrap()["entries"][0]
    );
    let new = &list["entries"][1];
    assert_eq!(
        new["checks"],
        serde_json::json!(["Recent crashes", "Disk health"])
    );
    assert_eq!(new["protocol_version"], 5);
    assert_eq!(new["min_toolboks_version"], "0.16.0");
    assert!(run(&["validate"]).status.success());

    // The current version's entry must carry the manifest's checks.
    manifest["manifest"]["checks"] = serde_json::json!(["Recent crashes"]);
    write(&manifest);
    let out = run(&["validate"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("checks"));
    manifest["manifest"]
        .as_object_mut()
        .unwrap()
        .remove("checks");
    write(&manifest);
    assert!(!run(&["validate"]).status.success());

    // A protocol 5 entry may not be offered to a host older than 0.16.0.
    manifest["manifest"]["checks"] = serde_json::json!(["Recent crashes", "Disk health"]);
    write(&manifest);
    let mut list = read();
    list["entries"][1]["min_toolboks_version"] = "0.15.0".into();
    fs::write(
        root.join("catalog/index.json"),
        serde_json::to_vec_pretty(&list).unwrap(),
    )
    .unwrap();
    let out = run(&["validate"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("0.16.0"));
}

/// A PNG signature and header for a `side` x `side` picture: what the host's checker reads.
fn png(side: u32) -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
    bytes.extend(13u32.to_be_bytes());
    bytes.extend(b"IHDR");
    bytes.extend(side.to_be_bytes());
    bytes.extend(side.to_be_bytes());
    bytes.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]);
    bytes
}

/// An add-on with its own picture: `catalog-update` refuses it without a good `icon.png`, then
/// fills the three icon fields from that file and raises the minimum host to the first that draws
/// it; `validate` fails when the file and the entry of the current version no longer agree, or
/// when the entry allows an older host. A named icon still gets none of the fields.
#[test]
fn custom_icon_is_checked_copied_and_validated() {
    let tmp = tempfile::Builder::new()
        .prefix("toolboks-addon-custom-icon-")
        .tempdir()
        .unwrap();
    let root = tmp.path().to_path_buf();
    fs::create_dir_all(root.join("addons/hello")).unwrap();
    fs::create_dir(root.join("catalog")).unwrap();
    fs::write(root.join("hello.exe"), b"release fixture").unwrap();
    let empty: &[u8] = br#"{"format_version":1,"entries":[]}"#;
    fs::write(root.join("catalog/index.json"), empty).unwrap();
    let mut manifest: serde_json::Value =
        serde_json::from_slice(include_bytes!("../../../addons/hello/hello.tbaddon")).unwrap();
    manifest["manifest"]["min_toolboks_version"] = "0.12.5".into();
    manifest["manifest"]["icon"] = "custom".into();
    manifest["manifest"]["version"] = "1.0.0".into();
    let write = |manifest: &serde_json::Value| {
        fs::write(
            root.join("addons/hello/hello.tbaddon"),
            serde_json::to_vec(manifest).unwrap(),
        )
        .unwrap()
    };
    write(&manifest);
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_addon-repo"))
            .current_dir(&root)
            .args(args)
            .output()
            .unwrap()
    };
    let update = |version: &str| {
        run(&[
            "catalog-update",
            "hello",
            version,
            "owner/addons",
            "hello.exe",
        ])
    };
    let list = || fs::read(root.join("catalog/index.json")).unwrap();

    // No icon.png, or one the host would refuse: nothing is written.
    let out = update("1.0.0");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("icon.png"));
    for bad in [png(200), png(8), b"not a picture".to_vec()] {
        fs::write(root.join("addons/hello/icon.png"), &bad).unwrap();
        assert!(!update("1.0.0").status.success());
        assert_eq!(list(), empty);
    }
    // validate also refuses an add-on with a custom icon and a bad icon.png.
    assert!(!run(&["validate"]).status.success());

    let icon = png(64);
    fs::write(root.join("addons/hello/icon.png"), &icon).unwrap();
    assert!(update("1.0.0").status.success());
    let entry = &Index::parse(&list()).unwrap().entries[0];
    assert_eq!(entry.icon, "custom");
    assert_eq!(
        entry.icon_url.as_deref(),
        Some("https://github.com/owner/addons/releases/download/hello-v1.0.0/icon.png")
    );
    assert_eq!(
        entry.icon_sha256.as_deref(),
        Some(toolboks_catalog::sha256_hex(&icon).as_str())
    );
    assert_eq!(entry.icon_size, Some(icon.len() as u64));
    assert_eq!(entry.min_toolboks_version, "0.21.0");
    assert!(run(&["validate"]).status.success());

    // Another picture in the repository than the one in the list for the current version.
    fs::write(root.join("addons/hello/icon.png"), png(32)).unwrap();
    let out = run(&["validate"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("icon_sha256"));
    fs::remove_file(root.join("addons/hello/icon.png")).unwrap();
    assert!(!run(&["validate"]).status.success());
    fs::write(root.join("addons/hello/icon.png"), &icon).unwrap();

    // A custom icon offered to a host older than 0.21.0 fails, whatever the version.
    let mut value: serde_json::Value = serde_json::from_slice(&list()).unwrap();
    value["entries"][0]["min_toolboks_version"] = "0.16.0".into();
    fs::write(
        root.join("catalog/index.json"),
        serde_json::to_vec_pretty(&value).unwrap(),
    )
    .unwrap();
    let out = run(&["validate"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("0.21.0"));
    manifest["manifest"]["version"] = "2.0.0".into();
    write(&manifest);
    assert!(!run(&["validate"]).status.success());

    // A named icon gets none of the three fields, even with an icon.png beside it.
    fs::write(root.join("catalog/index.json"), empty).unwrap();
    manifest["manifest"]["icon"] = "chip".into();
    write(&manifest);
    assert!(update("2.0.0").status.success());
    let text = String::from_utf8(list()).unwrap();
    assert!(
        !text.contains("icon_url") && !text.contains("icon_sha256") && !text.contains("icon_size")
    );
    assert!(text.contains("\"min_toolboks_version\": \"0.16.0\""));
    assert!(run(&["validate"]).status.success());
}
