// Hide the console window on Windows release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod decode;
#[cfg(target_os = "macos")]
mod mac_tags;
mod meta;
mod workers;

use std::path::PathBuf;

fn main() -> eframe::Result {
    let start = std::env::args_os().nth(1).map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Photo Picker")
            .with_inner_size([1400.0, 900.0])
            .with_min_inner_size([640.0, 400.0])
            .with_drag_and_drop(true)
            .with_maximized(true)
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon-256.png"))
                    .expect("valid icon PNG"),
            ),
        ..Default::default()
    };
    eframe::run_native(
        "Photo Picker",
        options,
        Box::new(move |cc| Ok(Box::new(app::PickerApp::new(cc, start)))),
    )
}
