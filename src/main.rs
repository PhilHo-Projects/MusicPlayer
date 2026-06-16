#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use music_player::app::{MusicPlayerApp, initial_file_from_args};

fn main() -> eframe::Result<()> {
    let initial_file = initial_file_from_args();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([960.0, 720.0])
            .with_min_inner_size([720.0, 520.0]),
        ..Default::default()
    };

    eframe::run_native(
        "MusicPlayer",
        options,
        Box::new(move |cc| Ok(Box::new(MusicPlayerApp::new(cc, initial_file.clone())))),
    )
}
