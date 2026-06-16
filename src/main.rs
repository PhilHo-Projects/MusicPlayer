#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use music_player::app::{MusicPlayerApp, initial_file_from_args};
use music_player::icon::app_icon;
use music_player::single_instance::{self, Startup};

fn main() -> eframe::Result<()> {
    let initial_file = initial_file_from_args();

    // If a window is already open, hand it our file and exit — double-clicking a
    // track reuses the running player instead of opening another window.
    let listener = match single_instance::acquire(initial_file.as_deref()) {
        Startup::Secondary => return Ok(()),
        Startup::Primary(listener) => listener,
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([960.0, 720.0])
            .with_min_inner_size([720.0, 520.0])
            .with_icon(app_icon()),
        ..Default::default()
    };

    eframe::run_native(
        "MusicPlayer",
        options,
        Box::new(move |cc| {
            let file_rx = single_instance::serve(listener, cc.egui_ctx.clone());
            Ok(Box::new(MusicPlayerApp::new(
                cc,
                initial_file,
                Some(file_rx),
            )))
        }),
    )
}
