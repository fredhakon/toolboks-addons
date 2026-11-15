#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if let Err(error) = toolboks_addon_sdk::runtime::run_addon(
        resource_monitor::Resources::default(),
        resource_monitor::ID,
    ) {
        eprintln!("Resource Monitor stopped: {error}");
        std::process::exit(1);
    }
}
