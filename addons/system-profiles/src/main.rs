#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if let Err(error) = toolboks_addon_sdk::runtime::run_addon(
        system_profiles::Gaming::default(),
        system_profiles::ID,
    ) {
        eprintln!("System Profiles stopped: {error}");
        std::process::exit(1);
    }
}
