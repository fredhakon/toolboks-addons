#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if let Err(error) =
        toolboks_addon_sdk::runtime::run_addon(sdk_addon::SdkAddon::default(), sdk_addon::ID)
    {
        eprintln!("SDK Add-on stopped: {error}");
        std::process::exit(1);
    }
}
