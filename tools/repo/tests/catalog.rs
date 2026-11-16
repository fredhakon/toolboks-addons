use std::{fs, process::Command};
use toolboks_catalog::Index;

#[test]
fn catalog_update_uses_released_bytes_and_preserves_other_versions() {
    let root = std::env::temp_dir().join(format!("toolboks-addon-catalog-{}", std::process::id()));
    // create_dir refuses to reuse another test's workspace.
    fs::create_dir(&root).unwrap();
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
    // The manifest needs 0.12.5, but the entry speaks protocol 4, which only 0.15.0 and later do.
    assert_eq!(entry.protocol_version, 4);
    assert_eq!(entry.min_toolboks_version, "0.15.0");
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
    fs::remove_dir_all(root).unwrap();
}

/// `catalog-update` copies the manifest's data note into the entry, leaves it out when the
/// manifest has none, refuses a bad one without touching the list, and `validate` fails when the
/// entry for the current version carries a different note.
#[test]
fn data_note_is_copied_checked_and_validated() {
    const NOTE: &str = "Keeps your last scan of each drive on this PC (file and folder names).";
    let root =
        std::env::temp_dir().join(format!("toolboks-addon-data-note-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
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
    fs::remove_dir_all(root).unwrap();
}

/// `catalog-update` copies the manifest's health check names and gives the entry the first host
/// that speaks its protocol; `validate` fails when the current version's entry differs from the
/// manifest, and leaves entries of older versions released at protocol 3 alone.
#[test]
fn checks_and_minimum_host_are_copied_and_validated() {
    let root = std::env::temp_dir().join(format!("toolboks-addon-checks-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
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
    assert_eq!(new["protocol_version"], 4);
    assert_eq!(new["min_toolboks_version"], "0.15.0");
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

    // A protocol 4 entry may not be offered to a host older than 0.15.0.
    manifest["manifest"]["checks"] = serde_json::json!(["Recent crashes", "Disk health"]);
    write(&manifest);
    let mut list = read();
    list["entries"][1]["min_toolboks_version"] = "0.14.0".into();
    fs::write(
        root.join("catalog/index.json"),
        serde_json::to_vec_pretty(&list).unwrap(),
    )
    .unwrap();
    let out = run(&["validate"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("0.15.0"));
    fs::remove_dir_all(root).unwrap();
}
