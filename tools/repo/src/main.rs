//! Repository automation; catalog types and validation belong to the host, not this repo.
use std::{env, fs, path::Path};
use toolboks_addon_sdk::Package;
use toolboks_catalog::{Channel, Entry, Index, Kind};
mod measure;

fn host_services(name: &str) -> (Vec<String>, bool) {
    let path = format!("addons/{name}/host-services.json");
    match fs::read(&path) {
        Ok(bytes) => {
            let value: serde_json::Value =
                serde_json::from_slice(&bytes).expect("host services JSON");
            let pages = serde_json::from_value(value["pages"].clone()).expect("page names");
            let background = value["background"].as_bool().expect("background flag");
            (pages, background)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (vec![], false),
        Err(error) => panic!("{path}: {error}"),
    }
}

fn package(name: &str) -> Package {
    assert!(
        !name.is_empty()
            && name
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
    );
    let path = format!("addons/{name}/{name}.tbaddon");
    Package::parse(&fs::read(path).unwrap(), "0.12.5").expect("host manifest validation")
}

fn validate() {
    Index::parse(&fs::read("catalog/index.json").unwrap()).expect("host catalog validation");
    for dir in fs::read_dir("addons").unwrap() {
        let dir = dir.unwrap();
        if dir.file_type().unwrap().is_dir() {
            package(dir.file_name().to_str().unwrap());
        }
    }
}

fn update(name: &str, version: &str, repository: &str, exe: &Path) {
    assert_eq!(repository.split('/').count(), 2);
    assert!(repository
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || b"-_./".contains(&c)));
    let m = package(name).manifest;
    let (pages, background) = host_services(name);
    assert_eq!(m.version, version);
    let bytes = fs::read(exe).expect("release executable");
    let entry = Entry {
        id: m.id,
        title: m.title,
        short_description: m.description,
        long_description: m.long_description,
        publisher: m.publisher,
        byline: m.byline,
        category: m.category,
        icon: m.icon,
        accent: m.accent,
        screenshots: m.screenshots,
        capabilities: serde_json::from_value(serde_json::to_value(m.capabilities).unwrap())
            .unwrap(),
        pages,
        background,
        kind: Kind::Native,
        version: version.into(),
        min_toolboks_version: m.min_toolboks_version,
        protocol_version: toolboks_addon_proto::PROTOCOL_VERSION,
        download_url: format!(
            "https://github.com/{repository}/releases/download/{name}-v{version}/{name}.exe"
        ),
        size: bytes.len() as u64,
        sha256: toolboks_catalog::sha256_hex(&bytes),
        release_notes: format!("Release {name}-v{version}."),
        verified: false,
        channel: Channel::Stable,
    };
    entry.validate().expect("host catalog entry validation");
    let mut index = Index::parse(&fs::read("catalog/index.json").unwrap()).unwrap();
    // Retain other versions and add-ons; replace only this exact id/version when retried.
    index
        .entries
        .retain(|e| !(e.id == entry.id && e.version == entry.version));
    index.entries.push(entry);
    index.validate().unwrap();
    fs::write(
        "catalog/index.json",
        serde_json::to_vec_pretty(&index).unwrap(),
    )
    .unwrap();
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["validate"] => validate(),
        ["measure", name] => measure::run(name),
        ["catalog-update", name, version, repository, exe] => {
            update(name, version, repository, Path::new(exe))
        }
        _ => panic!(
            "usage: addon-repo validate | measure <name> | catalog-update <name> <version> <owner/repo> <exe>"
        ),
    }
}
