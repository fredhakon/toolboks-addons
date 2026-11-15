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
    assert_eq!(entry.min_toolboks_version, "0.12.5");
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
