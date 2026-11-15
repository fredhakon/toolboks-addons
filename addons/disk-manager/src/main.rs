#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if let Err(error) = toolboks_addon_sdk::runtime::run_addon_with_admin(
        disk_manager::DiskManager::default,
        disk_manager::ID,
        disk_manager::prepare_admin,
    ) {
        eprintln!("Disk Manager stopped: {error}");
        std::process::exit(1);
    }
}
