#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::BTreeMap;
use toolboks_addon_sdk::{runtime::run_addon, NativeAddon};

#[derive(Default)]
struct Hello {
    styled: bool,
}

impl NativeAddon for Hello {
    fn ui(&mut self, ui: &mut egui::Ui, _: &mut BTreeMap<String, serde_json::Value>) -> bool {
        if !self.styled {
            toolboks_shared::ui::apply_style(ui.ctx());
            self.styled = true;
        }
        ui.heading("Hello");
        ui.label("The add-on is connected to Toolboks.");
        false
    }
}

fn main() {
    if let Err(error) = run_addon(Hello::default(), "toolboks.hello") {
        eprintln!("Hello stopped: {error}");
        std::process::exit(1);
    }
}
