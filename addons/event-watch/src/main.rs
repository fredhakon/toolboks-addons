#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if let Err(error) =
        toolboks_addon_sdk::runtime::run_addon(event_watch::EventWatch::default(), event_watch::ID)
    {
        eprintln!("Event Watch stopped: {error}");
        std::process::exit(1);
    }
}
