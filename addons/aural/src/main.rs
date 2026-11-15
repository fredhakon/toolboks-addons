#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if let Err(error) = toolboks_addon_sdk::runtime::run_addon(aural::Aural::default(), aural::ID) {
        eprintln!("Audio Gainer stopped: {error}");
        std::process::exit(1);
    }
}
