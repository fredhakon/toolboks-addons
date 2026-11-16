//! Repository automation; catalog types and validation belong to the host, not this repo.
use std::{env, fs, path::Path};
use toolboks_addon_sdk::{valid_data_note, Manifest, Package, MAX_DATA_NOTE_CHARS};
use toolboks_catalog::{Channel, Entry, Index, Kind};
mod measure;

fn host_services(name: &str) -> (Vec<String>, bool) {
    let path = format!("addons/{name}/host-services.json");
    match fs::read(&path) {
        Ok(bytes) => {
            let value: serde_json::Value =
                serde_json::from_slice(&bytes).expect("host services JSON");
            // Both keys are optional: no pages means a single-page add-on, no flag means it does
            // not keep running while hidden.
            let pages = match value.get("pages") {
                Some(pages) => serde_json::from_value(pages.clone()).expect("page names"),
                None => Vec::new(),
            };
            let background = match value.get("background") {
                Some(flag) => flag.as_bool().expect("background flag"),
                None => false,
            };
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
    Package::parse(&fs::read(path).unwrap(), PINNED_HOST).expect("host manifest validation")
}

/// The host release the workspace is pinned to (README, "When to move the pin"). Manifests are
/// checked as this host checks them, so an add-on may need this host but nothing newer.
const PINNED_HOST: &str = "0.14.0";

/// The manifest's data note for the catalog entry, checked with the host's own rule (the one the
/// catalog applies to entries). No note in the manifest means no note in the entry.
fn data_note(m: &Manifest) -> Result<Option<String>, String> {
    match &m.data_note {
        Some(note) if !valid_data_note(note) => Err(format!(
            "{}: the data note must be one line of plain text, 1 to {MAX_DATA_NOTE_CHARS} characters, with no spaces at either end and no hidden or control characters",
            m.id
        )),
        note => Ok(note.clone()),
    }
}

/// A catalog entry for the version that is current in `addons/<name>/<name>.tbaddon` must carry
/// exactly that manifest's data note (both set and equal, or both absent). Entries for older
/// versions are not checked: their manifest is the one at their release tag, not the one here.
/// `entries` is (id, version, note).
fn data_note_problem(
    entries: &[(&str, &str, Option<&str>)],
    manifests: &[Manifest],
) -> Option<String> {
    for (id, version, note) in entries {
        let Some(m) = manifests
            .iter()
            .find(|m| m.id == *id && m.version == *version)
        else {
            continue;
        };
        if *note != m.data_note.as_deref() {
            return Some(format!(
                "{id} {version}: the catalog entry's data_note {note:?} differs from the manifest's {:?}",
                m.data_note.as_deref()
            ));
        }
    }
    None
}

/// The sidebar icons the host can draw for an add-on. The names `grid`, `home`, `menu` and `gear`
/// belong to the Add-ons, Home, hamburger and Settings entries and are not for add-ons.
const ICONS: [&str; 9] = [
    "activity", "storage", "macro", "chip", "shield", "mic", "gauge", "sliders", "layers",
];

/// Every add-on in the catalog needs an icon the host can draw, and no two may share one: the
/// sidebar is scanned by icon first. `list` is (add-on id, icon).
fn icon_problem(list: &[(&str, &str)]) -> Option<String> {
    let mut seen: Vec<(&str, &str)> = Vec::new();
    for (id, icon) in list {
        if !ICONS.contains(icon) {
            return Some(format!(
                "{id}: icon `{icon}` is not one the host draws (use one of {ICONS:?})"
            ));
        }
        if let Some((other, _)) = seen.iter().find(|(o, i)| i == icon && o != id) {
            return Some(format!("{id} and {other} both use the icon `{icon}`"));
        }
        seen.push((id, icon));
    }
    None
}

fn check_icons(index: &Index) {
    let list: Vec<(&str, &str)> = index
        .entries
        .iter()
        .map(|e| (e.id.as_str(), e.icon.as_str()))
        .collect();
    if let Some(problem) = icon_problem(&list) {
        panic!("{problem}");
    }
}

fn validate() {
    let index =
        Index::parse(&fs::read("catalog/index.json").unwrap()).expect("host catalog validation");
    check_icons(&index);
    let mut manifests = Vec::new();
    for dir in fs::read_dir("addons").unwrap() {
        let dir = dir.unwrap();
        if dir.file_type().unwrap().is_dir() {
            manifests.push(package(dir.file_name().to_str().unwrap()).manifest);
        }
    }
    let entries: Vec<(&str, &str, Option<&str>)> = index
        .entries
        .iter()
        .map(|e| (e.id.as_str(), e.version.as_str(), e.data_note.as_deref()))
        .collect();
    if let Some(problem) = data_note_problem(&entries, &manifests) {
        panic!("{problem}");
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
    let data_note = data_note(&m).unwrap_or_else(|problem| panic!("{problem}"));
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
        data_note,
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
        ["catalog-update", name, version, repository, exe] => {
            update(name, version, repository, Path::new(exe))
        }
        ["measure", options @ ..] => measure::measure(options),
        _ => panic!(
            "usage: addon-repo validate | catalog-update <name> <version> <owner/repo> <exe> | measure [--report-only] [--runs <n>] [--json <file>] [<add-on>...]"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_must_be_known_and_unique() {
        assert_eq!(icon_problem(&[("a", "chip"), ("b", "mic")]), None);
        // The same add-on may list several versions with one icon.
        assert_eq!(icon_problem(&[("a", "chip"), ("a", "chip")]), None);
        assert!(icon_problem(&[("a", "chip"), ("b", "chip")])
            .unwrap()
            .contains("both use the icon"));
        assert!(icon_problem(&[("a", "grid")])
            .unwrap()
            .contains("not one the host draws"));
        assert!(icon_problem(&[("a", "gear")]).is_some());
    }

    const NOTE: &str = "Keeps your last scan of each drive on this PC (file and folder names).";

    /// The Hello manifest with the given version and data note. Deserializing does not validate,
    /// so a bad note reaches `data_note` the way a manifest built by hand would.
    fn manifest(version: &str, note: Option<&str>) -> Manifest {
        let mut v: serde_json::Value =
            serde_json::from_slice(include_bytes!("../../../addons/hello/hello.tbaddon")).unwrap();
        v["manifest"]["version"] = version.into();
        if let Some(note) = note {
            v["manifest"]["data_note"] = note.into();
        }
        serde_json::from_value(v["manifest"].clone()).unwrap()
    }

    #[test]
    fn data_note_is_copied_only_when_valid() {
        assert_eq!(
            data_note(&manifest("1.0.0", Some(NOTE))),
            Ok(Some(NOTE.to_string()))
        );
        assert_eq!(data_note(&manifest("1.0.0", None)), Ok(None));
        assert_eq!(
            data_note(&manifest("1.0.0", Some(&"a".repeat(160)))),
            Ok(Some("a".repeat(160)))
        );
        for bad in [
            "a".repeat(161),
            String::new(),
            " Keeps scans.".into(),
            "Keeps\nscans.".into(),
            "Keeps \u{202e}snacs.".into(),
            "Keeps\u{200b}scans.".into(),
            "Keeps\u{1b}[31mscans.".into(),
        ] {
            assert!(
                data_note(&manifest("1.0.0", Some(&bad))).is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn current_entry_note_must_match_the_manifest() {
        let id = "toolboks.hello";
        let current = [manifest("2.0.0", Some(NOTE))];
        // The current version with the same note, and older versions with anything.
        assert_eq!(
            data_note_problem(
                &[
                    (id, "1.0.0", None),
                    (id, "1.5.0", Some("An older note.")),
                    (id, "2.0.0", Some(NOTE))
                ],
                &current
            ),
            None
        );
        // Another add-on's entries are not compared with this manifest.
        assert_eq!(
            data_note_problem(&[("toolboks.other", "2.0.0", Some("Other."))], &current),
            None
        );
        // A different note, or none at all, for the current version fails.
        assert!(
            data_note_problem(&[(id, "2.0.0", Some("Keeps scans."))], &current)
                .unwrap()
                .contains("differs")
        );
        assert!(data_note_problem(&[(id, "2.0.0", None)], &current).is_some());
        // A note the current manifest does not have fails too.
        let without = [manifest("2.0.0", None)];
        assert!(data_note_problem(&[(id, "2.0.0", Some(NOTE))], &without).is_some());
        assert_eq!(data_note_problem(&[(id, "2.0.0", None)], &without), None);
    }
}
