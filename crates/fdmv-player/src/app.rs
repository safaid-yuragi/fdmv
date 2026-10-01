use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Color32, ColorImage, Key, Layout, Modifiers, RichText, Sense, TextureHandle,
    TextureOptions, Vec2, ViewportCommand,
};
use libfdmv::format::Rational;
use libfdmv::time::format_time;

use crate::engine::Player;
use crate::texts::Texts;
use crate::timeline::{Lane, chain_color, timeline};

/// ドラッグ中にシークを送る最小間隔。
const SEEK_INTERVAL: Duration = Duration::from_millis(80);

pub struct App {
    t: &'static Texts,
    player: Option<Player>,
    texture: Option<TextureHandle>,
    video_size: Vec2,
    error: Option<String>,
    show_info: bool,
    show_chains: bool,
    fullscreen: bool,
    master_volume: f32,
    muted: bool,
    last_seek: Instant,
    pending_seek: Option<f64>,
    /// ドラッグ中に表示する時刻。
    scrub: Option<f64>,
    /// 表示中のフレームの時刻。
    shown_pts: Option<f64>,
    /// `FDMV_DEBUG_SCREENSHOT` による自己テスト。
    debug: Option<DebugRun>,
}

/// 環境変数 `FDMV_DEBUG_SCREENSHOT=<出力.ppm>` を指定すると、起動後に全チェーンを有効にして
/// 2 秒の位置から 1.5 秒再生し、画面を保存して終了する（動作確認用）。
struct DebugRun {
    path: PathBuf,
    started: Option<Instant>,
    requested: bool,
}

impl App {
    pub fn new(t: &'static Texts, ctx: &egui::Context, path: Option<PathBuf>) -> Self {
        let mut app = App {
            t,
            player: None,
            texture: None,
            video_size: Vec2::ZERO,
            error: None,
            show_info: false,
            show_chains: true,
            fullscreen: false,
            master_volume: 1.0,
            muted: false,
            last_seek: Instant::now(),
            pending_seek: None,
            scrub: None,
            shown_pts: None,
            debug: std::env::var_os("FDMV_DEBUG_SCREENSHOT").map(|p| DebugRun {
                path: p.into(),
                started: None,
                requested: false,
            }),
        };
        if let Some(p) = path {
            app.open(ctx, &p);
        }
        app
    }

    fn open(&mut self, ctx: &egui::Context, path: &Path) {
        // 先に古いプレイヤーを止める（スレッドとデバイスを解放）。
        self.player = None;
        self.texture = None;
        match Player::open(path, ctx.clone()) {
            Ok(p) => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let title = p
                    .dir
                    .meta("title")
                    .map(|t| format!("{t} — {name}"))
                    .unwrap_or(name);
                ctx.send_viewport_cmd(ViewportCommand::Title(format!("{title} — FDMV Player")));
                if let Some(v) = p.dir.video().and_then(|v| v.video()) {
                    self.video_size = Vec2::new(v.width as f32, v.height as f32);
                }
                p.set_master_volume(self.effective_volume());
                self.player = Some(p);
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }

    fn close(&mut self, ctx: &egui::Context) {
        self.player = None;
        self.texture = None;
        ctx.send_viewport_cmd(ViewportCommand::Title("FDMV Player".into()));
    }

    fn open_dialog(&mut self, ctx: &egui::Context) {
        let picked = rfd::FileDialog::new()
            .add_filter(self.t.fdmv_files, &["fdmv"])
            .pick_file();
        if let Some(p) = picked {
            self.open(ctx, &p);
        }
    }

    fn effective_volume(&self) -> f32 {
        if self.muted { 0.0 } else { self.master_volume }
    }

    fn apply_volume(&self) {
        if let Some(p) = &self.player {
            p.set_master_volume(self.effective_volume());
        }
    }

    fn seek_relative(&mut self, delta: f64) {
        if let Some(p) = &mut self.player {
            let t = p.position() + delta;
            p.seek(t);
        }
    }

    fn handle_input(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .filter(|p| !p.as_os_str().is_empty())
                .collect()
        });
        if let Some(p) = dropped.first() {
            self.open(ctx, p);
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::O)) {
            self.open_dialog(ctx);
        }
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let pressed = |k: Key| ctx.input_mut(|i| i.consume_key(Modifiers::NONE, k));
        if pressed(Key::Space)
            && let Some(p) = &mut self.player
        {
            p.toggle();
        }
        if pressed(Key::ArrowLeft) {
            self.seek_relative(-5.0);
        }
        if pressed(Key::ArrowRight) {
            self.seek_relative(5.0);
        }
        if pressed(Key::Home)
            && let Some(p) = &mut self.player
        {
            p.seek(0.0);
        }
        if pressed(Key::ArrowUp) {
            self.master_volume = (self.master_volume + 0.1).min(1.5);
            self.apply_volume();
        }
        if pressed(Key::ArrowDown) {
            self.master_volume = (self.master_volume - 0.1).max(0.0);
            self.apply_volume();
        }
        if pressed(Key::M) {
            self.muted = !self.muted;
            self.apply_volume();
        }
        if pressed(Key::F) {
            self.set_fullscreen(ctx, !self.fullscreen);
        }
        if self.fullscreen && pressed(Key::Escape) {
            self.set_fullscreen(ctx, false);
        }
    }

    fn set_fullscreen(&mut self, ctx: &egui::Context, on: bool) {
        self.fullscreen = on;
        ctx.send_viewport_cmd(ViewportCommand::Fullscreen(on));
    }

    fn update_texture(&mut self, ctx: &egui::Context) {
        let Some(p) = &self.player else { return };
        if let Some(e) = p.error() {
            self.error = Some(e);
        }
        let Some(frame) = p.take_frame() else { return };
        let image = ColorImage::from_rgba_unmultiplied([frame.width, frame.height], &frame.rgba);
        self.video_size = Vec2::new(frame.width as f32, frame.height as f32);
        self.shown_pts = Some(frame.pts);
        match &mut self.texture {
            Some(t) => t.set(image, TextureOptions::LINEAR),
            None => self.texture = Some(ctx.load_texture("video", image, TextureOptions::LINEAR)),
        }
    }

    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button(self.t.file, |ui| {
                if ui.button(self.t.open).clicked() {
                    ui.close();
                    self.open_dialog(&ctx);
                }
                if ui
                    .add_enabled(self.player.is_some(), egui::Button::new(self.t.close))
                    .clicked()
                {
                    ui.close();
                    self.close(&ctx);
                }
                ui.separator();
                if ui.button(self.t.quit).clicked() {
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
            });
            ui.menu_button(self.t.view, |ui| {
                ui.checkbox(&mut self.show_chains, self.t.chains_panel);
                ui.checkbox(&mut self.show_info, self.t.info);
                let mut fs = self.fullscreen;
                if ui.checkbox(&mut fs, self.t.fullscreen).changed() {
                    self.set_fullscreen(&ctx, fs);
                }
            });
        });
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        let t = self.t;
        let Some(p) = &mut self.player else {
            ui.add_space(4.0);
            ui.label(RichText::new(t.keys_hint).weak());
            ui.add_space(4.0);
            return;
        };
        ui.add_space(4.0);
        let lanes: Vec<Lane> = p
            .chains
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.is_default)
            .map(|(i, c)| Lane {
                color: chain_color(i),
                segments: &c.segments,
                active: c.enabled,
            })
            .collect();
        let shown = self.scrub.unwrap_or_else(|| p.position());
        let r = timeline(ui, shown, p.duration, &lanes);
        if let Some(target) = r.seek {
            self.scrub = Some(target);
            self.pending_seek = Some(target);
        }
        if let Some(target) = self.pending_seek
            && (r.released || self.last_seek.elapsed() >= SEEK_INTERVAL)
        {
            p.seek(target);
            self.pending_seek = None;
            self.last_seek = Instant::now();
        }
        if r.released || (r.seek.is_none() && self.pending_seek.is_none()) {
            self.scrub = None;
        }

        ui.horizontal(|ui| {
            let icon = if p.is_playing() { "⏸" } else { "▶" };
            if ui
                .add(
                    egui::Button::new(RichText::new(icon).size(18.0))
                        .min_size(Vec2::new(36.0, 28.0)),
                )
                .clicked()
            {
                p.toggle();
            }
            ui.label(
                RichText::new(format!(
                    "{} / {}",
                    format_time(shown),
                    format_time(p.duration)
                ))
                .monospace(),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let changed = ui
                    .add(egui::Slider::new(&mut self.master_volume, 0.0..=1.5).show_value(false))
                    .on_hover_text(t.master)
                    .changed();
                let speaker = if self.muted || self.master_volume == 0.0 {
                    "🔇"
                } else {
                    "🔊"
                };
                let mute = ui
                    .selectable_label(self.muted, speaker)
                    .on_hover_text(t.mute);
                if mute.clicked() {
                    self.muted = !self.muted;
                }
                if changed || mute.clicked() {
                    p.set_master_volume(if self.muted { 0.0 } else { self.master_volume });
                }
            });
        });
        ui.add_space(2.0);
    }

    fn chains_panel(&mut self, ui: &mut egui::Ui) {
        let t = self.t;
        ui.heading(t.chains);
        ui.separator();
        let Some(p) = &mut self.player else { return };
        if p.chains.is_empty() {
            ui.label(RichText::new(t.no_chains).weak());
            return;
        }
        egui::ScrollArea::vertical().show(ui, |ui| {
            for i in 0..p.chains.len() {
                let c = &p.chains[i];
                let mut enabled = c.enabled;
                let mut volume = c.volume;
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                    let color = if c.is_default {
                        ui.visuals().text_color()
                    } else {
                        chain_color(i)
                    };
                    ui.painter().circle_filled(rect.center(), 5.0, color);
                    let mut label = RichText::new(&c.name);
                    if c.is_default {
                        label = label.strong();
                    }
                    let resp = ui.checkbox(&mut enabled, label);
                    let mut tip = format!("{}: {}\n{}ch", t.segments, c.segments.len(), c.channels);
                    if let Some(l) = &c.language {
                        tip.push_str(&format!("\nlanguage: {l}"));
                    }
                    if let Some(d) = &c.description {
                        tip.push_str(&format!("\n{d}"));
                    }
                    resp.on_hover_text(tip);
                    if c.is_default {
                        ui.label(RichText::new(format!("[{}]", t.default_tag)).weak());
                    }
                });
                if let Some(d) = &c.description {
                    ui.label(RichText::new(d).small().weak());
                }
                ui.add_enabled_ui(enabled, |ui| {
                    ui.add(
                        egui::Slider::new(&mut volume, 0.0..=2.0)
                            .text(t.volume)
                            .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
                    );
                });
                if enabled != c.enabled {
                    p.set_chain_enabled(i, enabled);
                }
                if volume != p.chains[i].volume {
                    p.set_chain_volume(i, volume);
                }
                ui.add_space(6.0);
            }
        });
    }

    fn info_window(&mut self, ctx: &egui::Context) {
        let t = self.t;
        let mut open = self.show_info;
        egui::Window::new(t.info)
            .open(&mut open)
            .resizable(true)
            .show(ctx, |ui| {
                let Some(p) = &self.player else {
                    ui.label("—");
                    return;
                };
                egui::Grid::new("info")
                    .num_columns(2)
                    .striped(true)
                    .show(ui, |ui| {
                        ui.label("File");
                        ui.label(p.path.display().to_string());
                        ui.end_row();
                        ui.label(t.duration);
                        ui.label(format_time(p.duration));
                        ui.end_row();
                        if let Some(v) = p.dir.video()
                            && let Some(vp) = v.video()
                        {
                            ui.label(t.video);
                            ui.label(format!(
                                "AV1 {}x{}, {:.3} fps",
                                vp.width,
                                vp.height,
                                vp.frame_rate.as_f64()
                            ));
                            ui.end_row();
                        }
                        ui.label(t.audio_device);
                        ui.label(p.audio_device().unwrap_or(t.no_audio_device));
                        ui.end_row();
                        ui.label("Size");
                        ui.label(format!("{:.1} MiB", p.file_size as f64 / (1024.0 * 1024.0)));
                        ui.end_row();
                        for (k, v) in &p.dir.meta {
                            ui.label(format!("{}: {k}", t.metadata));
                            ui.label(v);
                            ui.end_row();
                        }
                        for c in &p.chains {
                            ui.label(format!("{}: {}", t.chains, c.name));
                            let segs: Vec<String> = c
                                .segments
                                .iter()
                                .map(|s| {
                                    format!(
                                        "{} – {}",
                                        format_time(Rational::OPUS.to_seconds(s.start)),
                                        format_time(Rational::OPUS.to_seconds(s.end()))
                                    )
                                })
                                .collect();
                            ui.label(segs.join("\n"));
                            ui.end_row();
                        }
                    });
            });
        self.show_info = open;
    }

    fn error_modal(&mut self, ctx: &egui::Context) {
        let Some(msg) = &self.error else { return };
        let t = self.t;
        let mut close = false;
        egui::Modal::new(egui::Id::new("error")).show(ctx, |ui| {
            ui.set_max_width(480.0);
            ui.heading(t.error);
            ui.label(msg);
            ui.add_space(8.0);
            if ui.button(t.ok).clicked() {
                close = true;
            }
        });
        if close {
            self.error = None;
        }
    }

    fn video_area(&mut self, ui: &mut egui::Ui) {
        let rect = ui.max_rect();
        ui.painter().rect_filled(rect, 0.0, Color32::BLACK);
        match (&self.texture, &self.player) {
            (Some(tex), Some(_)) if self.video_size.x > 0.0 => {
                let scale =
                    (rect.width() / self.video_size.x).min(rect.height() / self.video_size.y);
                let size = self.video_size * scale;
                let r = egui::Rect::from_center_size(rect.center(), size);
                ui.painter().image(
                    tex.id(),
                    r,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            (_, None) => {
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    self.t.drop_hint,
                    egui::FontId::proportional(16.0),
                    Color32::GRAY,
                );
            }
            _ => {}
        }
        // 映像のクリックで再生／一時停止、ダブルクリックで全画面
        let resp = ui.interact(rect, egui::Id::new("video"), Sense::click());
        if resp.double_clicked() {
            let fs = !self.fullscreen;
            self.set_fullscreen(ui.ctx(), fs);
        } else if resp.clicked()
            && let Some(p) = &mut self.player
        {
            p.toggle();
        }
    }
}

impl App {
    fn debug_tick(&mut self, ctx: &egui::Context) {
        let Some(d) = &mut self.debug else { return };
        let Some(p) = &mut self.player else { return };
        match d.started {
            None => {
                for i in 0..p.chains.len() {
                    if !p.chains[i].enabled {
                        p.set_chain_enabled(i, true);
                    }
                }
                p.seek(2.0);
                p.play();
                d.started = Some(Instant::now());
            }
            Some(t0) if !d.requested && t0.elapsed() >= Duration::from_millis(1500) => {
                ctx.send_viewport_cmd(ViewportCommand::Screenshot(Default::default()));
                d.requested = true;
            }
            _ => {}
        }
        ctx.request_repaint();
        let shot = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = shot {
            let mut ppm = format!("P6\n{} {}\n255\n", img.size[0], img.size[1]).into_bytes();
            for c in &img.pixels {
                ppm.extend_from_slice(&[c.r(), c.g(), c.b()]);
            }
            let res = std::fs::write(&d.path, ppm);
            println!(
                "debug: wall={:.3}s position={:.3}s frame_pts={:?} playing={} audio={:?} save={:?}",
                d.started.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0),
                p.position(),
                self.shown_pts,
                p.is_playing(),
                p.audio_device(),
                res
            );
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.handle_input(&ctx);
        if let Some(p) = &mut self.player {
            p.update();
        }
        self.update_texture(&ctx);
        self.debug_tick(&ctx);

        if !self.fullscreen {
            egui::Panel::top("menu").show(ui, |ui| self.menu_bar(ui));
        }
        egui::Panel::bottom("controls").show(ui, |ui| self.controls(ui));
        if self.show_chains && !self.fullscreen {
            egui::Panel::right("chains")
                .resizable(true)
                .default_size(240.0)
                .min_size(160.0)
                .show(ui, |ui| self.chains_panel(ui));
        }
        egui::CentralPanel::no_frame().show(ui, |ui| self.video_area(ui));

        self.info_window(&ctx);
        self.error_modal(&ctx);

        if let Some(p) = &self.player
            && p.is_playing()
        {
            // 次のフレームの表示時刻まで待つ。時刻表示のため最長でも 50 ms ごとに更新する。
            let wait = p.time_to_next_frame().unwrap_or(0.05).min(0.05);
            ctx.request_repaint_after(Duration::from_secs_f64(wait));
        }
    }
}
