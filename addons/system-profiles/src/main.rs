#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if let Err(error) = toolboks_addon_sdk::runtime::run_addon_with_admin(
        system_profiles::Gaming::default,
        system_profiles::ID,
        system_profiles::prepare_admin,
    ) {
        eprintln!("System Profiles stopped: {error}");
        std::process::exit(1);
    }
}
