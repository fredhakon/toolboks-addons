//! Repository automation; catalog types and validation belong to the host, not this repo.
use std::{collections::BTreeMap, env, fs, path::Path};
use toolboks_addon_proto::PROTOCOL_VERSION;
use toolboks_addon_sdk::{
    valid_check_names, valid_data_note, version, Manifest, Package, MAX_CHECKS,
    MAX_CHECK_NAME_CHARS, MAX_DATA_NOTE_CHARS,
};
use toolboks_catalog::{check_icon_header, Channel, Entry, Index, Kind, CUSTOM_ICON};
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
const PINNED_HOST: &str = "0.21.0";

/// The first host release that speaks each protocol newer than 3. A catalog entry at one of these
/// protocols needs at least that host, so no older host offers it. When the pin moves to a host
/// with a new protocol, add its row (a test fails until then). Entries at protocol 3 predate this
/// rule and keep the minimum they were released with.
const PROTOCOL_HOSTS: [(u32, &str); 2] = [(4, "0.15.0"), (5, "0.16.0")];

/// The first host release that draws an add-on's own picture (`"icon": "custom"` with `icon_url`,
/// `icon_sha256` and `icon_size`). Older hosts draw the default glyph instead, so an entry with a
/// custom icon needs at least this host. Applies to every entry, old versions included.
const CUSTOM_ICON_HOST: &str = "0.21.0";

fn first_host(protocol: u32) -> Option<&'static str> {
    PROTOCOL_HOSTS
        .iter()
        .find(|(p, _)| *p == protocol)
        .map(|(_, host)| *host)
}

/// The `min_toolboks_version` of an entry at `protocol` with `icon` for a manifest that needs
/// `manifest_min`: the newest of the three, so the entry is offered only to hosts that can
/// install the manifest, speak the protocol and draw the icon.
fn entry_min_host(manifest_min: &str, protocol: u32, icon: &str) -> String {
    let mut min = manifest_min;
    for host in [
        first_host(protocol),
        (icon == CUSTOM_ICON).then_some(CUSTOM_ICON_HOST),
    ]
    .into_iter()
    .flatten()
    {
        if version(host) > version(min) {
            min = host;
        }
    }
    min.into()
}

/// An entry whose protocol the pinned SDK does not speak, or whose minimum host is older than
/// the first host that speaks its protocol, or (for a custom icon) the first host that draws it.
/// Applies to every entry, old versions included.
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
        _ if e.icon == CUSTOM_ICON
            && version(&e.min_toolboks_version) < version(CUSTOM_ICON_HOST) =>
        {
            Some(format!(
                "{id} {v}: a custom icon needs min_toolboks_version {CUSTOM_ICON_HOST} or later (the first host that draws it), not {}",
                e.min_toolboks_version
            ))
        }
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
/// manifest: its data note (both set and equal, or both absent), its health check names, the
/// minimum host [`entry_min_host`] gives for the entry's protocol and icon, and for a custom icon
/// the address, SHA-256 and size of `addons/<name>/icon.png` (`icons`, by manifest id). Entries
/// for older versions are not compared with the manifest: theirs is the one at their release
/// tag, not the one here.
fn entry_problem(
    entries: &[Entry],
    manifests: &[Manifest],
    icons: &BTreeMap<String, Vec<u8>>,
) -> Option<String> {
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
        let min = entry_min_host(&m.min_toolboks_version, e.protocol_version, &m.icon);
        if e.min_toolboks_version != min {
            return Some(format!(
                "{id} {v}: the catalog entry's min_toolboks_version {} differs from {min} (the manifest's {} at protocol {})",
                e.min_toolboks_version, m.min_toolboks_version, e.protocol_version
            ));
        }
        if let Some(problem) = custom_icon_problem(e, m, icons.get(id).map(Vec::as_slice)) {
            return Some(problem);
        }
    }
    None
}

/// The current version's entry `e` against its manifest `m` and `png`, the add-on's
/// `icon.png`: a custom icon on either side needs it on both, and then the entry's address,
/// SHA-256 and size must be the ones `catalog-update` gives for that file. A named icon carries
/// none of the three fields.
fn custom_icon_problem(e: &Entry, m: &Manifest, png: Option<&[u8]>) -> Option<String> {
    let (id, v) = (&e.id, &e.version);
    if e.icon != m.icon && (e.icon == CUSTOM_ICON || m.icon == CUSTOM_ICON) {
        return Some(format!(
            "{id} {v}: the catalog entry's icon `{}` differs from the manifest's `{}`",
            e.icon, m.icon
        ));
    }
    if m.icon != CUSTOM_ICON {
        if e.icon_url.is_some() || e.icon_sha256.is_some() || e.icon_size.is_some() {
            return Some(format!(
                "{id} {v}: the catalog entry has icon_url, icon_sha256 or icon_size, but its icon is `{}`, not custom",
                e.icon
            ));
        }
        return None;
    }
    let Some(png) = png else {
        return Some(format!(
            "{id} {v}: the icon is custom, but the add-on has no icon.png"
        ));
    };
    let url = e
        .download_url
        .rsplit_once('/')
        .map(|(folder, _)| format!("{folder}/icon.png"));
    if e.icon_url != url {
        return Some(format!(
            "{id} {v}: the catalog entry's icon_url {:?} is not {url:?}, next to its download",
            e.icon_url
        ));
    }
    if e.icon_sha256.as_deref() != Some(toolboks_catalog::sha256_hex(png).as_str())
        || e.icon_size != Some(png.len() as u64)
    {
        return Some(format!(
            "{id} {v}: the catalog entry's icon_sha256 or icon_size does not match the add-on's icon.png"
        ));
    }
    None
}

/// The sidebar icons the host can draw for an add-on. The names `grid`, `home`, `menu` and `gear`
/// belong to the Add-ons, Home, hamburger and Settings entries and are not for add-ons.
const ICONS: [&str; 9] = [
    "activity", "storage", "macro", "chip", "shield", "mic", "gauge", "sliders", "layers",
];

/// Every add-on in the catalog needs an icon the host can draw, and no two may share one: the
/// sidebar is scanned by icon first. A named icon belongs to one add-on; add-ons with their own
/// picture (`custom`) share that word, but not the picture, told apart by its SHA-256.
/// `list` is (add-on id, icon, icon_sha256).
fn icon_problem(list: &[(&str, &str, Option<&str>)]) -> Option<String> {
    let mut seen: Vec<(&str, &str, Option<&str>)> = Vec::new();
    for &(id, icon, sha) in list {
        if icon == CUSTOM_ICON {
            let Some(sha) = sha else {
                return Some(format!("{id}: a custom icon needs its icon_sha256"));
            };
            if let Some((other, ..)) = seen
                .iter()
                .find(|(o, i, s)| *i == CUSTOM_ICON && *s == Some(sha) && *o != id)
            {
                return Some(format!(
                    "{id} and {other} both use the same custom icon (icon_sha256 {sha})"
                ));
            }
        } else {
            if !ICONS.contains(&icon) {
                return Some(format!(
                    "{id}: icon `{icon}` is not one the host draws (use one of {ICONS:?}, or `{CUSTOM_ICON}` with the add-on's own icon.png)"
                ));
            }
            if let Some((other, ..)) = seen.iter().find(|(o, i, _)| *i == icon && *o != id) {
                return Some(format!("{id} and {other} both use the icon `{icon}`"));
            }
        }
        seen.push((id, icon, sha));
    }
    None
}

fn check_icons(index: &Index) {
    let list: Vec<(&str, &str, Option<&str>)> = index
        .entries
        .iter()
        .map(|e| (e.id.as_str(), e.icon.as_str(), e.icon_sha256.as_deref()))
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
    let mut icons = BTreeMap::new();
    for dir in fs::read_dir("addons").unwrap() {
        let dir = dir.unwrap();
        if dir.file_type().unwrap().is_dir() {
            let name = dir.file_name().to_str().unwrap().to_string();
            let manifest = package(&name).manifest;
            // Every add-on with its own picture must ship a good one, released or not.
            if manifest.icon == CUSTOM_ICON {
                let png = icon_png(&name).unwrap_or_else(|problem| panic!("{problem}"));
                icons.insert(manifest.id.clone(), png);
            }
            manifests.push(manifest);
        }
    }
    if let Some(problem) = entry_problem(&index.entries, &manifests, &icons) {
        panic!("{problem}");
    }
}

/// `addons/<name>/icon.png`, the picture of an add-on whose icon is `custom`, checked with the
/// host's rule for custom icons (a square PNG of 16 to 128 pixels, at most 32 KiB).
fn icon_png(name: &str) -> Result<Vec<u8>, String> {
    let path = format!("addons/{name}/icon.png");
    let png = fs::read(&path).map_err(|e| {
        format!(
            "{path}: {e}. An add-on whose icon is \"{CUSTOM_ICON}\" needs its own icon.png there."
        )
    })?;
    check_custom_icon(name, &png)?;
    Ok(png)
}

fn check_custom_icon(name: &str, png: &[u8]) -> Result<(), String> {
    check_icon_header(png)
        .map(drop)
        .map_err(|problem| format!("addons/{name}/icon.png: {problem}"))
}

fn update(name: &str, version: &str, repository: &str, exe: &Path) {
    assert_eq!(repository.split('/').count(), 2);
    assert!(repository
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || b"-_./".contains(&c)));
    let m = package(name).manifest;
    assert_eq!(m.version, version);
    let bytes = fs::read(exe).expect("release executable");
    let png = if m.icon == CUSTOM_ICON {
        Some(icon_png(name).unwrap_or_else(|problem| panic!("{problem}")))
    } else {
        None
    };
    let entry = new_entry(
        name,
        m,
        host_services(name),
        repository,
        &bytes,
        png.as_deref(),
    )
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
/// executable is `bytes` and, for a custom icon, whose `icon.png` is `png` (uploaded next to the
/// executable). It speaks the pinned protocol and copies the manifest's data note and health
/// checks; its minimum host covers the manifest, that protocol and the icon. Checked as the host
/// checks entries.
fn new_entry(
    name: &str,
    m: Manifest,
    (pages, background): (Vec<String>, bool),
    repository: &str,
    bytes: &[u8],
    png: Option<&[u8]>,
) -> Result<Entry, String> {
    let data_note = data_note(&m)?;
    let checks = checks(&m)?;
    let min_toolboks_version = entry_min_host(&m.min_toolboks_version, PROTOCOL_VERSION, &m.icon);
    let v = m.version;
    let (icon_url, icon_sha256, icon_size) = if m.icon == CUSTOM_ICON {
        let png = png.ok_or_else(|| {
            format!("addons/{name}/icon.png is missing; an add-on whose icon is \"{CUSTOM_ICON}\" needs it")
        })?;
        check_custom_icon(name, png)?;
        (
            Some(format!(
                "https://github.com/{repository}/releases/download/{name}-v{v}/icon.png"
            )),
            Some(toolboks_catalog::sha256_hex(png)),
            Some(png.len() as u64),
        )
    } else {
        (None, None, None)
    };
    let entry = Entry {
        id: m.id,
        title: m.title,
        short_description: m.description,
        long_description: m.long_description,
        publisher: m.publisher,
        byline: m.byline,
        category: m.category,
        icon: m.icon,
        icon_url,
        icon_sha256,
        icon_size,
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
        assert_eq!(
            icon_problem(&[("a", "chip", None), ("b", "mic", None)]),
            None
        );
        // The same add-on may list several versions with one icon.
        assert_eq!(
            icon_problem(&[("a", "chip", None), ("a", "chip", None)]),
            None
        );
        assert!(icon_problem(&[("a", "chip", None), ("b", "chip", None)])
            .unwrap()
            .contains("both use the icon"));
        assert!(icon_problem(&[("a", "grid", None)])
            .unwrap()
            .contains("not one the host draws"));
        assert!(icon_problem(&[("a", "gear", None)]).is_some());
    }

    #[test]
    fn custom_icons_are_told_apart_by_their_picture() {
        let (one, two) = (Some("11"), Some("22"));
        // Two add-ons may both have their own picture, when the pictures differ.
        assert_eq!(
            icon_problem(&[("a", "custom", one), ("b", "custom", two)]),
            None
        );
        // One add-on may change its picture between versions.
        assert_eq!(
            icon_problem(&[("a", "custom", one), ("a", "custom", two)]),
            None
        );
        // A named icon next to a custom one is fine.
        assert_eq!(
            icon_problem(&[("a", "custom", one), ("b", "chip", None)]),
            None
        );
        assert!(icon_problem(&[("a", "custom", one), ("b", "custom", one)])
            .unwrap()
            .contains("same custom icon"));
        assert!(icon_problem(&[("a", "custom", None)]).is_some());
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
        new_entry(
            "hello",
            m.clone(),
            (vec![], false),
            "owner/addons",
            b"exe",
            None,
        )
        .unwrap()
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

    fn no_icons() -> BTreeMap<String, Vec<u8>> {
        BTreeMap::new()
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
        assert_eq!(entry_min_host("0.16.2", 5, "chip"), "0.16.2");
        assert_eq!(entry_min_host("0.15.0", 5, "chip"), "0.16.0");
        // Entries released at protocol 4 keep needing the first host that speaks 4.
        assert_eq!(entry_min_host("0.12.5", 4, "chip"), "0.15.0");
        assert_eq!(entry_min_host("0.16.2", 4, "chip"), "0.16.2");
        // Protocol 3 adds nothing: released entries keep the minimum they were released with.
        assert_eq!(entry_min_host("0.12.0", 3, "chip"), "0.12.0");
        // A custom icon needs the first host that draws it.
        assert_eq!(entry_min_host("0.12.0", 5, "custom"), "0.21.0");
        assert_eq!(entry_min_host("0.22.1", 5, "custom"), "0.22.1");
    }

    #[test]
    fn every_pinned_protocol_has_its_first_host() {
        for protocol in 4..=PROTOCOL_VERSION {
            let host = first_host(protocol).expect("add a PROTOCOL_HOSTS row");
            assert!(version(host).is_some() && version(host) <= version(PINNED_HOST));
        }
        assert_eq!(first_host(3), None);
        // The pinned host draws custom icons, and the SDK accepts them in a manifest.
        assert!(version(CUSTOM_ICON_HOST).is_some());
        assert!(version(CUSTOM_ICON_HOST) <= version(PINNED_HOST));
    }

    #[test]
    fn custom_icon_entries_carry_the_picture_and_need_the_host_that_draws_it() {
        let mut m = manifest("1.0.0", None);
        m.icon = "custom".into();
        m.min_toolboks_version = "0.12.5".into();
        let icon = png(64);
        let e = new_entry(
            "hello",
            m.clone(),
            (vec![], false),
            "owner/addons",
            b"exe",
            Some(icon.as_slice()),
        )
        .unwrap();
        assert_eq!(e.min_toolboks_version, "0.21.0");
        assert_eq!(
            e.icon_url.as_deref(),
            Some("https://github.com/owner/addons/releases/download/hello-v1.0.0/icon.png")
        );
        assert_eq!(e.icon_sha256, Some(toolboks_catalog::sha256_hex(&icon)));
        assert_eq!(e.icon_size, Some(icon.len() as u64));
        assert_eq!(protocol_problem(&e), None);
        // No icon.png, or one the host's checker refuses: no entry.
        let refused = |png: Option<&[u8]>| {
            new_entry("hello", m.clone(), (vec![], false), "o/r", b"exe", png).unwrap_err()
        };
        assert!(refused(None).contains("icon.png is missing"));
        assert!(refused(Some(png(200).as_slice())).contains("must be 16 to 128"));
        assert!(refused(Some(&b"GIF89a"[..])).contains("not a PNG"));
        // A named icon carries none of the three fields, even when a picture is given.
        m.icon = "chip".into();
        let named = new_entry(
            "hello",
            m.clone(),
            (vec![], false),
            "o/r",
            b"exe",
            Some(icon.as_slice()),
        )
        .unwrap();
        assert_eq!(
            (named.icon_url, named.icon_sha256, named.icon_size),
            (None, None, None)
        );
        assert_eq!(named.min_toolboks_version, "0.16.0");
        // Any entry with a custom icon that allows an older host fails, old versions too.
        let mut old = e.clone();
        old.min_toolboks_version = "0.20.0".into();
        assert!(protocol_problem(&old)
            .unwrap()
            .contains("custom icon needs min_toolboks_version 0.21.0"));
        old.version = "0.5.0".into();
        assert!(entry_problem(&[old], &[], &no_icons()).is_some());
    }

    #[test]
    fn the_current_custom_icon_entry_must_match_the_icon_file() {
        let mut m = manifest("1.0.0", None);
        m.icon = "custom".into();
        let icon = png(64);
        let made = new_entry(
            "hello",
            m.clone(),
            (vec![], false),
            "owner/addons",
            b"exe",
            Some(icon.as_slice()),
        )
        .unwrap();
        let icons = BTreeMap::from([(m.id.clone(), icon.clone())]);
        let manifests = [m.clone()];
        assert_eq!(
            entry_problem(std::slice::from_ref(&made), &manifests, &icons),
            None
        );
        let with = |change: &dyn Fn(&mut Entry)| {
            let mut e = made.clone();
            change(&mut e);
            entry_problem(&[e], &manifests, &icons)
        };
        // Another picture in the repository than the one in the list.
        let other = BTreeMap::from([(m.id.clone(), png(32))]);
        assert!(
            entry_problem(std::slice::from_ref(&made), &manifests, &other)
                .unwrap()
                .contains("icon_sha256 or icon_size")
        );
        // No picture at all.
        assert!(
            entry_problem(std::slice::from_ref(&made), &manifests, &no_icons())
                .unwrap()
                .contains("no icon.png")
        );
        assert!(with(&|e| e.icon_size = Some(1)).is_some());
        assert!(
            with(&|e| e.icon_url = Some("https://github.com/o/r/x.png".into()))
                .unwrap()
                .contains("icon_url")
        );
        // A named icon in the list for a custom manifest, and the other way round.
        assert!(with(&|e| e.icon = "chip".into())
            .unwrap()
            .contains("differs from the manifest"));
        let mut named = m.clone();
        named.icon = "chip".into();
        assert!(entry_problem(std::slice::from_ref(&made), &[named.clone()], &icons).is_some());
        // A named icon may not carry the custom fields.
        let mut bare = entry(&named);
        assert_eq!(
            entry_problem(std::slice::from_ref(&bare), &[named.clone()], &icons),
            None
        );
        bare.icon_size = Some(10);
        assert!(entry_problem(&[bare], &[named], &icons)
            .unwrap()
            .contains("not custom"));
        // Older versions are not compared with today's picture.
        let mut old = made.clone();
        old.version = "0.9.0".into();
        old.icon_sha256 = Some(toolboks_catalog::sha256_hex(b"older"));
        assert_eq!(entry_problem(&[old], &manifests, &icons), None);
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
        assert!(entry_problem(&[e], &[manifest("9.0.0", None)], &no_icons()).is_some());
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
            assert!(new_entry("hello", m.clone(), (vec![], false), "o/r", b"exe", None).is_err());
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
        assert_eq!(
            entry_problem(&[old, made.clone()], &manifests, &no_icons()),
            None
        );
        // Another add-on's entries are not compared with this manifest.
        let other = with(&|e| {
            e.id = "toolboks.other".into();
            e.data_note = None;
        });
        assert_eq!(entry_problem(&[other], &manifests, &no_icons()), None);
        // A different note, or none at all, for the current version fails.
        let problem = |e: Entry| entry_problem(&[e], &manifests, &no_icons());
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
        assert!(entry_problem(
            std::slice::from_ref(&made),
            std::slice::from_ref(&bare),
            &no_icons()
        )
        .is_some());
        assert_eq!(entry_problem(&[entry(&bare)], &[bare], &no_icons()), None);
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
