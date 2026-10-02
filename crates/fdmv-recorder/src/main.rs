// リリースビルドの Windows ではコンソールウィンドウを出さない。
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;

use eframe::egui;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("FDMV Recorder")
            .with_app_id("fdmv-recorder")
            .with_inner_size([1080.0, 720.0])
            .with_min_inner_size([760.0, 520.0]),
        ..Default::default()
    };
    eframe::run_native(
        "fdmv-recorder",
        options,
        Box::new(|cc| {
            if !fdmv_gui::fonts::install(&cc.egui_ctx) {
                eprintln!(
                    "no CJK font found; Japanese text may not display (set FDMV_FONT to a font file)"
                );
            }
            Ok(Box::new(app::App::new(&cc.egui_ctx)))
        }),
    )
}
