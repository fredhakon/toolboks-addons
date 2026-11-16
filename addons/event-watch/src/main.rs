#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // The latency check's administrator helper branches off here, before any UI exists.
    if let Err(error) = toolboks_addon_sdk::runtime::run_addon_with_admin(
        event_watch::EventWatch::default,
        event_watch::ID,
        event_watch::prepare_admin,
    ) {
        eprintln!("Event Watch stopped: {error}");
        std::process::exit(1);
    }
}
