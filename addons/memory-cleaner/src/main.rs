#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if let Err(error) = toolboks_addon_sdk::runtime::run_addon_with_admin(
        memory_cleaner::Memory::new,
        memory_cleaner::ID,
        memory_cleaner::prepare_admin,
    ) {
        eprintln!("Memory Cleaner stopped: {error}");
        std::process::exit(1);
    }
}
