//! Repository automation; catalog types and validation belong to the host, not this repo.
use std::{env, fs, path::Path};
use toolboks_addon_proto::PROTOCOL_VERSION;
use toolboks_addon_sdk::{
    valid_check_names, valid_data_note, version, Manifest, Package, MAX_CHECKS,
    MAX_CHECK_NAME_CHARS, MAX_DATA_NOTE_CHARS,
};
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
const PINNED_HOST: &str = "0.16.0";

/// The first host release that speaks each protocol newer than 3. A catalog entry at one of these
/// protocols needs at least that host, so no older host offers it. When the pin moves to a host
/// with a new protocol, add its row (a test fails until then). Entries at protocol 3 predate this
/// rule and keep the minimum they were released with.
const PROTOCOL_HOSTS: [(u32, &str); 2] = [(4, "0.15.0"), (5, "0.16.0")];

fn first_host(protocol: u32) -> Option<&'static str> {
    PROTOCOL_HOSTS
        .iter()
        .find(|(p, _)| *p == protocol)
        .map(|(_, host)| *host)
}

/// The `min_toolboks_version` of an entry at `protocol` for a manifest that needs
/// `manifest_min`: the newer of the two, so the entry is offered only to hosts that can both
/// install the manifest and speak the protocol.
fn entry_min_host(manifest_min: &str, protocol: u32) -> String {
    match first_host(protocol) {
        Some(host) if version(host) > version(manifest_min) => host.into(),
        _ => manifest_min.into(),
    }
}

/// An entry whose protocol the pinned SDK does not speak, or whose minimum host is older than
/// the first host that speaks its protocol. Applies to every entry, old versions included.
fn protocol_problem(e: &Entry) -> Option<String> {
    let (id, v, protocol) = (&e.id, &e.version, e.protocol_version);
    if protocol > PROTOCOL_VERSION {
        return Some(format!(
            "{id} {v}: protocol {protocol} is newer than the pinned SDK's {PROTOCOL_VERSION}"
        ));
    }
    match first_host(protocol) {
        Some(host) if version(&e.min_toolboks_version) < version(host) => Some(format!(
            "{id} {v}: protocol {protocol} needs min_toolboks_version {host} or later (the first host that speaks it), not {}",
            e.min_toolboks_version
        )),
        _ => None,
    }
}

/// The manifest's health check names for the catalog entry, checked with the host's own rule
/// (the one the catalog applies to entries). No names in the manifest means none in the entry.
fn checks(m: &Manifest) -> Result<Vec<String>, String> {
    if valid_check_names(&m.checks) {
        Ok(m.checks.clone())
    } else {
        Err(format!(
            "{}: the health checks must be at most {MAX_CHECKS} different names of 1 to {MAX_CHECK_NAME_CHARS} characters of plain text, with no spaces at either end and no hidden or control characters",
            m.id
        ))
    }
}

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

/// Every entry must pass [`protocol_problem`]. The entry for the version that is current in
/// `addons/<name>/<name>.tbaddon` must also carry exactly what `catalog-update` copies from that
/// manifest: its data note (both set and equal, or both absent), its health check names, and the
/// minimum host [`entry_min_host`] gives for the entry's protocol. Entries for older versions are
/// not compared with the manifest: theirs is the one at their release tag, not the one here.
fn entry_problem(entries: &[Entry], manifests: &[Manifest]) -> Option<String> {
    for e in entries {
        if let Some(problem) = protocol_problem(e) {
            return Some(problem);
        }
        let (id, v) = (&e.id, &e.version);
        let Some(m) = manifests.iter().find(|m| m.id == *id && m.version == *v) else {
            continue;
        };
        if e.data_note != m.data_note {
            return Some(format!(
                "{id} {v}: the catalog entry's data_note {:?} differs from the manifest's {:?}",
                e.data_note, m.data_note
            ));
        }
        if e.checks != m.checks {
            return Some(format!(
                "{id} {v}: the catalog entry's checks {:?} differ from the manifest's {:?}",
                e.checks, m.checks
            ));
        }
        let min = entry_min_host(&m.min_toolboks_version, e.protocol_version);
        if e.min_toolboks_version != min {
            return Some(format!(
                "{id} {v}: the catalog entry's min_toolboks_version {} differs from {min} (the manifest's {} at protocol {})",
                e.min_toolboks_version, m.min_toolboks_version, e.protocol_version
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
    if let Some(problem) = entry_problem(&index.entries, &manifests) {
        panic!("{problem}");
    }
}

fn update(name: &str, version: &str, repository: &str, exe: &Path) {
    assert_eq!(repository.split('/').count(), 2);
    assert!(repository
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || b"-_./".contains(&c)));
    let m = package(name).manifest;
    assert_eq!(m.version, version);
    let bytes = fs::read(exe).expect("release executable");
    let entry = new_entry(name, m, host_services(name), repository, &bytes)
        .unwrap_or_else(|problem| panic!("{problem}"));
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

/// The catalog entry for a release of add-on `name` (its folder) with manifest `m`, whose
/// executable is `bytes`. It speaks the pinned protocol and copies the manifest's data note and
/// health checks; its minimum host covers both the manifest and that protocol. Checked as the
/// host checks entries.
fn new_entry(
    name: &str,
    m: Manifest,
    (pages, background): (Vec<String>, bool),
    repository: &str,
    bytes: &[u8],
) -> Result<Entry, String> {
    let data_note = data_note(&m)?;
    let checks = checks(&m)?;
    let min_toolboks_version = entry_min_host(&m.min_toolboks_version, PROTOCOL_VERSION);
    let v = m.version;
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
        checks,
        kind: Kind::Native,
        min_toolboks_version,
        protocol_version: PROTOCOL_VERSION,
        download_url: format!(
            "https://github.com/{repository}/releases/download/{name}-v{v}/{name}.exe"
        ),
        size: bytes.len() as u64,
        sha256: toolboks_catalog::sha256_hex(bytes),
        release_notes: format!("Release {name}-v{v}."),
        version: v,
        verified: false,
        channel: Channel::Stable,
    };
    entry
        .validate()
        .map_err(|field| format!("host catalog entry validation: bad {field}"))?;
    Ok(entry)
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

    /// The entry `catalog-update` makes for the manifest.
    fn entry(m: &Manifest) -> Entry {
        new_entry("hello", m.clone(), (vec![], false), "owner/addons", b"exe").unwrap()
    }

    #[test]
    fn new_entries_speak_the_pinned_protocol_and_need_the_host_that_speaks_it() {
        assert_eq!(PROTOCOL_VERSION, 5);
        let mut m = manifest("2.0.0", None);
        m.min_toolboks_version = "0.12.5".into();
        let e = entry(&m);
        assert_eq!(e.protocol_version, 5);
        assert_eq!(e.min_toolboks_version, "0.16.0");
        assert_eq!(protocol_problem(&e), None);
        // A manifest that needs a newer host keeps its own minimum.
        assert_eq!(entry_min_host("0.16.2", 5), "0.16.2");
        assert_eq!(entry_min_host("0.15.0", 5), "0.16.0");
        // Entries released at protocol 4 keep needing the first host that speaks 4.
        assert_eq!(entry_min_host("0.12.5", 4), "0.15.0");
        assert_eq!(entry_min_host("0.16.2", 4), "0.16.2");
        // Protocol 3 adds nothing: released entries keep the minimum they were released with.
        assert_eq!(entry_min_host("0.12.0", 3), "0.12.0");
    }

    #[test]
    fn every_pinned_protocol_has_its_first_host() {
        for protocol in 4..=PROTOCOL_VERSION {
            let host = first_host(protocol).expect("add a PROTOCOL_HOSTS row");
            assert!(version(host).is_some() && version(host) <= version(PINNED_HOST));
        }
        assert_eq!(first_host(3), None);
    }

    #[test]
    fn protocol_problems_apply_to_every_entry() {
        let mut e = entry(&manifest("2.0.0", None));
        e.min_toolboks_version = "0.15.0".into();
        assert!(protocol_problem(&e)
            .unwrap()
            .contains("needs min_toolboks_version 0.16.0"));
        // A protocol 4 entry (released with 0.15.0) still passes, and still may not go lower.
        e.protocol_version = 4;
        assert_eq!(protocol_problem(&e), None);
        e.min_toolboks_version = "0.14.0".into();
        assert!(protocol_problem(&e)
            .unwrap()
            .contains("needs min_toolboks_version 0.15.0"));
        // An entry released at protocol 3 with an older minimum is fine as it is.
        e.protocol_version = 3;
        e.min_toolboks_version = "0.12.0".into();
        assert_eq!(protocol_problem(&e), None);
        e.protocol_version = PROTOCOL_VERSION + 1;
        assert!(protocol_problem(&e)
            .unwrap()
            .contains("newer than the pinned SDK"));
        // `validate` applies it to entries of versions the manifests no longer describe.
        e.protocol_version = 4;
        assert!(entry_problem(&[e], &[manifest("9.0.0", None)]).is_some());
    }

    #[test]
    fn checks_are_copied_only_when_valid() {
        let mut m = manifest("1.0.0", None);
        assert_eq!(checks(&m), Ok(vec![]));
        m.checks = vec!["Recent crashes".into(), "a".repeat(40)];
        assert_eq!(checks(&m), Ok(m.checks.clone()));
        assert_eq!(entry(&m).checks, m.checks);
        for bad in [
            vec!["a".repeat(41)],
            (0..7).map(|i| format!("Check {i}")).collect(),
            vec!["Disk health".into(), "Disk health".into()],
            vec![String::new()],
            vec![" Disk health".into()],
            vec!["Disk\nhealth".into()],
            vec!["Disk \u{202e}health".into()],
        ] {
            m.checks = bad.clone();
            assert!(checks(&m).is_err(), "{bad:?}");
            assert!(new_entry("hello", m.clone(), (vec![], false), "o/r", b"exe").is_err());
        }
    }

    #[test]
    fn current_entry_must_match_the_manifest() {
        let mut current = manifest("2.0.0", Some(NOTE));
        current.checks = vec!["Recent crashes".into()];
        let made = entry(&current);
        let with = |change: &dyn Fn(&mut Entry)| {
            let mut e = made.clone();
            change(&mut e);
            e
        };
        let manifests = [current.clone()];
        // The entry catalog-update makes, and older versions with anything.
        let old = with(&|e| {
            e.version = "1.0.0".into();
            e.data_note = Some("An older note.".into());
            e.checks = vec![];
            e.protocol_version = 3;
            e.min_toolboks_version = "0.12.5".into();
        });
        assert_eq!(entry_problem(&[old, made.clone()], &manifests), None);
        // Another add-on's entries are not compared with this manifest.
        let other = with(&|e| {
            e.id = "toolboks.other".into();
            e.data_note = None;
        });
        assert_eq!(entry_problem(&[other], &manifests), None);
        // A different note, or none at all, for the current version fails.
        let problem = |e: Entry| entry_problem(&[e], &manifests);
        assert!(
            problem(with(&|e| e.data_note = Some("Keeps scans.".into())))
                .unwrap()
                .contains("data_note")
        );
        assert!(problem(with(&|e| e.data_note = None)).is_some());
        // So do different or missing checks.
        assert!(problem(with(&|e| e.checks = vec![]))
            .unwrap()
            .contains("checks"));
        assert!(problem(with(&|e| e.checks.push("Disk health".into()))).is_some());
        // And a minimum host other than the one catalog-update gives.
        assert!(problem(with(&|e| e.min_toolboks_version = "0.17.0".into()))
            .unwrap()
            .contains("min_toolboks_version"));
        // A note or checks the current manifest does not have fail too.
        let mut bare = manifest("2.0.0", None);
        bare.min_toolboks_version = current.min_toolboks_version.clone();
        assert!(entry_problem(std::slice::from_ref(&made), std::slice::from_ref(&bare)).is_some());
        assert_eq!(entry_problem(&[entry(&bare)], &[bare]), None);
    }

    /// Reading and writing the real list with the pinned catalog crate gives back the same bytes,
    /// so `catalog-update` never rewrites an existing entry when it adds one.
    #[test]
    fn existing_entries_are_written_back_unchanged() {
        let bytes = include_bytes!("../../../catalog/index.json");
        let index = Index::parse(bytes).unwrap();
        let again = serde_json::to_vec_pretty(&index).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&again).trim_end(),
            String::from_utf8_lossy(bytes).trim_end()
        );
    }
}
