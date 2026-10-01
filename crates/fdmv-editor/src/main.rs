// リリースビルドの Windows ではコンソールウィンドウを出さない。
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod history;
mod jobs;
mod panels;
mod timeline;

use std::path::PathBuf;

use eframe::egui;

fn main() -> eframe::Result {
    // 古い macOS では Finder から起動すると `-psn_...` 引数が付くので無視する。
    let path = std::env::args_os()
        .skip(1)
        .find(|a| !a.to_string_lossy().starts_with("-psn_"))
        .map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("FDMV Editor")
            .with_app_id("fdmv-editor")
            .with_inner_size([1360.0, 860.0])
            .with_min_inner_size([900.0, 560.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "fdmv-editor",
        options,
        Box::new(move |cc| {
            if !fdmv_gui::fonts::install(&cc.egui_ctx) {
                eprintln!(
                    "no CJK font found; Japanese text may not display (set FDMV_FONT to a font file)"
                );
            }
            Ok(Box::new(app::App::new(&cc.egui_ctx, path)))
        }),
    )
}
