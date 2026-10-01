// リリースビルドの Windows ではコンソールウィンドウを出さない。
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod engine;
mod fonts;
mod texts;
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
            .with_title("FDMV Player")
            .with_app_id("fdmv-player")
            .with_inner_size([1024.0, 680.0])
            .with_min_inner_size([480.0, 320.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "fdmv-player",
        options,
        Box::new(move |cc| {
            let texts = if fonts::install(&cc.egui_ctx) {
                &texts::JA
            } else {
                eprintln!(
                    "no CJK font found; using English UI (set FDMV_FONT to a font file to override)"
                );
                &texts::EN
            };
            Ok(Box::new(app::App::new(texts, &cc.egui_ctx, path)))
        }),
    )
}
