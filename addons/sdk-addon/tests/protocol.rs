//! The manifest belongs to this add-on, its icon passes the host's checker, and the exe connects,
//! draws, answers a heartbeat and quits (with the SDK's `testing` helper, as Toolboks starts it).
//! `ADDON_TEST_EXE` tests a release exe instead of Cargo's test build.
use toolboks_addon_sdk::testing::{check_exe, check_manifest};

const ID: &str = "toolboks.sdk-addon";
const MANIFEST: &[u8] = include_bytes!("../sdk-addon.tbaddon");

#[test]
fn manifest_matches_the_executable() {
    let package = check_manifest(MANIFEST, ID, env!("CARGO_PKG_VERSION")).unwrap();
    assert_eq!(sdk_addon::ID, ID);
    assert_eq!(package.manifest.icon, "custom");
    // The first Toolboks that draws a custom icon.
    assert_eq!(package.manifest.min_toolboks_version, "0.21.0");
    assert!(package.manifest.data_note.is_some());
}

#[test]
fn icon_passes_the_hosts_checker() {
    let icon = include_bytes!("../icon.png");
    assert_eq!(toolboks_catalog::check_icon_header(icon), Ok(64));
}

#[test]
fn protocol_test_host_draws_pings_and_quits() {
    let exe =
        std::env::var("ADDON_TEST_EXE").unwrap_or_else(|_| env!("CARGO_BIN_EXE_sdk-addon").into());
    check_exe(std::path::Path::new(&exe), ID).unwrap();
}
