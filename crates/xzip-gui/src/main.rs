//! xZip Archiver - desktop app.
//!
//! Copyright (c) Clinton Turner.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod theme;

fn main() -> eframe::Result {
    let icon = {
        let img = image::load_from_memory(include_bytes!("../../../assets/icon.png"))
            .expect("embedded icon")
            .to_rgba8();
        let (w, h) = img.dimensions();
        egui::IconData {
            rgba: img.into_raw(),
            width: w,
            height: h,
        }
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("xZip Archiver")
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([820.0, 520.0])
            .with_icon(icon)
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "xZip Archiver",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
