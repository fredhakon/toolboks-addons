#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if let Err(error) = toolboks_addon_sdk::runtime::run_addon(
        macro_manager::MacroManager::default(),
        macro_manager::ID,
    ) {
        eprintln!("Macro Manager stopped: {error}");
        std::process::exit(1);
    }
}
