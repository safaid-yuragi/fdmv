//! 録画アプリの画面と状態。
//!
//! - 左: 録画する画面・ウィンドウの選択とプレビュー
//! - 右: 録音するアプリ（チェーン）の選択、メインチェーンの指定、マイク
//! - 下: 設定と録画ボタン
//!
//! 時間のかかる処理（画面共有ダイアログ、アプリ一覧の取得、録画の開始・停止・変換）は
//! 別スレッドで行い、結果をチャンネルで受け取る。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use eframe::egui::{
    self, Align, Color32, ColorImage, Layout, RichText, TextureHandle, TextureOptions,
    ViewportCommand,
};
use fdmv_capture::audio::{self, AudioApp, MicDevice};
use fdmv_capture::codecs::{self, EncoderKind, LiveEncoder};
use fdmv_capture::finalize::{FinalizeReport, Stage};
use fdmv_capture::recorder::ChainStatus;
use fdmv_capture::video::{self, TargetInfo, VideoCapture, VideoTarget};
use fdmv_capture::{AudioSource, ChainSetup, RecordOptions, Recorder, VideoMode};
use libfdmv::ffmpeg::{Ffmpeg, Quality};

const ACCENT: Color32 = Color32::from_rgb(0x3d, 0x7e, 0xd6);
const REC: Color32 = Color32::from_rgb(0xd9, 0x3a, 0x3a);
/// マイクのチェーン名。
const MIC_CHAIN: &str = "マイク";
/// アプリ一覧を取り直す間隔。
const APPS_INTERVAL: Duration = Duration::from_secs(2);
/// プレビューを更新する間隔。
const PREVIEW_INTERVAL: Duration = Duration::from_millis(100);
const PREVIEW_MAX_WIDTH: u32 = 960;

/// 音声の一覧の 1 行。
struct AppRow {
    app: AudioApp,
    /// 録音する。
    selected: bool,
    chain_name: String,
    /// 直近の一覧にあったか（無ければ音を出していない）。
    present: bool,
    /// 録画中: このアプリのチェーンを録っているか。
    recording: bool,
}

enum Phase {
    Idle,
    Starting(Receiver<Result<Recorder>>),
    Recording(Box<Recorder>),
    /// 停止して FDMV に変換している。
    Finishing {
        rx: Receiver<Result<FinalizeReport>>,
        progress: Arc<Mutex<Option<(Stage, f64)>>>,
        cancel: Arc<AtomicBool>,
        output: PathBuf,
    },
    Done {
        output: PathBuf,
        report: FinalizeReport,
    },
}

struct Settings {
    out_dir: PathBuf,
    title: String,
    fps: u32,
    quality: Quality,
    mode: VideoMode,
    /// 録画中に使う映像エンコーダ（None = おすすめを自動で選ぶ）。
    encoder: Option<String>,
    max_height: Option<u32>,
    audio_bitrate: String,
}

pub struct App {
    ff: Result<Ffmpeg, String>,
    settings: Settings,
    /// 使える映像エンコーダ（おすすめ順）。調べ終わるまでは None。
    encoders: Option<Vec<LiveEncoder>>,
    encoders_job: Option<Receiver<Vec<LiveEncoder>>>,

    targets: Vec<TargetInfo>,
    target: usize,
    capture: Option<Box<dyn VideoCapture>>,
    capture_job: Option<Receiver<Result<Box<dyn VideoCapture>>>>,
    preview: Option<TextureHandle>,
    preview_generation: u64,
    preview_at: Instant,
    /// 取り込んでいる映像の大きさ。
    capture_size: Option<(u32, u32)>,

    apps: Vec<AppRow>,
    main_key: Option<String>,
    apps_job: Option<Receiver<Result<Vec<AudioApp>>>>,
    apps_at: Instant,
    apps_error: Option<String>,
    mics: Vec<MicDevice>,
    mic_on: bool,
    mic: usize,
    mic_recording: bool,

    phase: Phase,
    /// 録画中のファイルの保存先。
    output: PathBuf,
    /// チェーン名 → 表示中のレベル（ピークを緩やかに下げる）。
    levels: HashMap<String, f32>,
    chain_status: Vec<ChainStatus>,
    error: Option<String>,
    /// 閉じる前の確認を出している。
    confirm_close: bool,
    /// 変換が終わったら閉じる。
    close_when_done: bool,
    debug: Option<Debug>,
}

/// `FDMV_DEBUG_SCREENSHOT` による自己テスト（テストパターンを録って画面を保存し、変換して終了する）。
struct Debug {
    path: PathBuf,
    frames: u32,
    shot: bool,
}

impl App {
    pub fn new(ctx: &egui::Context) -> Self {
        let ff = Ffmpeg::locate().map_err(|e| e.to_string());
        let out_dir = dirs::video_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        let (targets, mut error) = match video::targets() {
            Ok(t) => (t, None),
            Err(e) => (
                Vec::new(),
                Some(format!("録画できる画面の一覧を取得できません: {e:#}")),
            ),
        };
        let mics = audio::list_mics().unwrap_or_else(|e| {
            error.get_or_insert(format!("マイクの一覧を取得できません: {e:#}"));
            Vec::new()
        });
        let debug = std::env::var_os("FDMV_DEBUG_SCREENSHOT").map(|p| Debug {
            path: p.into(),
            frames: 0,
            shot: false,
        });
        // 自己テストではスクリーンショットと同じフォルダに保存する。
        let out_dir = match &debug {
            Some(d) => d
                .path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from(".")),
            None => out_dir,
        };
        let mut app = App {
            ff,
            settings: Settings {
                out_dir,
                title: String::new(),
                fps: 30,
                quality: Quality::High,
                mode: VideoMode::Direct,
                encoder: None,
                max_height: None,
                audio_bitrate: "128k".into(),
            },
            encoders: None,
            encoders_job: None,
            targets,
            target: 0,
            capture: None,
            capture_job: None,
            preview: None,
            preview_generation: 0,
            preview_at: Instant::now(),
            capture_size: None,
            apps: Vec::new(),
            main_key: None,
            apps_job: None,
            apps_at: Instant::now() - APPS_INTERVAL,
            apps_error: None,
            mics,
            mic_on: false,
            mic: 0,
            mic_recording: false,
            phase: Phase::Idle,
            output: PathBuf::new(),
            levels: HashMap::new(),
            chain_status: Vec::new(),
            error,
            confirm_close: false,
            close_when_done: false,
            debug,
        };
        // 使える GPU エンコーダを調べる（少し時間がかかるので別スレッドで）。
        if let Ok(ff) = app.ff.clone() {
            let (tx, rx) = channel();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(codecs::available(&ff));
                ctx.request_repaint();
            });
            app.encoders_job = Some(rx);
        }
        if app.debug.is_some()
            && let Some(i) = app
                .targets
                .iter()
                .position(|t| t.target == VideoTarget::TestPattern)
        {
            app.target = i;
            app.start_capture(ctx);
        }
        app
    }

    /// いま選ばれている（自動ならおすすめの）エンコーダ。
    fn selected_encoder(&self) -> Option<&LiveEncoder> {
        let list = self.encoders.as_ref()?;
        match &self.settings.encoder {
            None => list.first(),
            Some(n) => list.iter().find(|e| &e.name == n),
        }
    }

    fn poll_encoders(&mut self) {
        if let Some(rx) = &self.encoders_job {
            match rx.try_recv() {
                Ok(list) => {
                    self.encoders = Some(list);
                    self.encoders_job = None;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.encoders_job = None,
            }
        }
    }

    fn is_idle(&self) -> bool {
        matches!(self.phase, Phase::Idle | Phase::Done { .. })
    }

    // -----------------------------------------------------------------------
    // 映像

    fn start_capture(&mut self, ctx: &egui::Context) {
        let (Ok(ff), Some(t)) = (&self.ff, self.targets.get(self.target)) else {
            return;
        };
        // 前の取り込みを止めてから（同じ画面を 2 重に取り込まない）。
        self.capture = None;
        self.preview = None;
        self.capture_size = None;
        let (ff, target, fps) = (ff.clone(), t.target.clone(), self.settings.fps);
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(video::start(&ff, &target, fps));
            ctx.request_repaint();
        });
        self.capture_job = Some(rx);
        // ウィンドウを選んだら、そのアプリを録音対象・メインにする。
        if let Some(key) = t.app_key.clone()
            && let Some(row) = self.apps.iter_mut().find(|r| r.app.key == key)
        {
            row.selected = true;
            if self.main_key.is_none() {
                self.main_key = Some(key);
            }
        }
    }

    fn poll_capture(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.capture_job {
            match rx.try_recv() {
                Ok(Ok(c)) => {
                    self.capture = Some(c);
                    self.capture_job = None;
                }
                Ok(Err(e)) => {
                    self.error = Some(format!("{e:#}"));
                    self.capture_job = None;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.capture_job = None,
            }
        }
        let Some(cap) = &self.capture else { return };
        let slot = cap.slot();
        let generation = slot.generation();
        if generation == self.preview_generation || self.preview_at.elapsed() < PREVIEW_INTERVAL {
            return;
        }
        let (generation, Some(frame)) = slot.latest() else {
            return;
        };
        self.preview_generation = generation;
        self.preview_at = Instant::now();
        self.capture_size = Some((frame.width, frame.height));
        // 縮小（最近傍）して RGBA に。
        let step = frame.width.div_ceil(PREVIEW_MAX_WIDTH).max(1) as usize;
        let (w, h) = (frame.width as usize / step, frame.height as usize / step);
        if w == 0 || h == 0 {
            return;
        }
        let mut rgba = Vec::with_capacity(w * h * 4);
        let stride = frame.width as usize * 4;
        for y in 0..h {
            let row = &frame.bgra[y * step * stride..];
            for x in 0..w {
                let p = &row[x * step * 4..x * step * 4 + 4];
                rgba.extend_from_slice(&[p[2], p[1], p[0], 255]);
            }
        }
        let img = ColorImage::from_rgba_unmultiplied([w, h], &rgba);
        match &mut self.preview {
            Some(t) => t.set(img, TextureOptions::LINEAR),
            None => self.preview = Some(ctx.load_texture("preview", img, TextureOptions::LINEAR)),
        }
    }

    // -----------------------------------------------------------------------
    // 音声

    fn poll_apps(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.apps_job {
            match rx.try_recv() {
                Ok(Ok(list)) => {
                    self.apps_error = None;
                    self.merge_apps(list);
                    self.apps_job = None;
                }
                Ok(Err(e)) => {
                    self.apps_error = Some(format!("{e:#}"));
                    self.apps_job = None;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.apps_job = None,
            }
        }
        if self.apps_job.is_none() && self.apps_at.elapsed() >= APPS_INTERVAL {
            self.apps_at = Instant::now();
            let (tx, rx) = channel();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(audio::list_apps());
                ctx.request_repaint();
            });
            self.apps_job = Some(rx);
        }
    }

    /// 新しい一覧を反映する。一度出たアプリは、音が止まっても一覧に残す。
    fn merge_apps(&mut self, list: Vec<AudioApp>) {
        for r in &mut self.apps {
            r.present = false;
        }
        for app in list {
            match self.apps.iter_mut().find(|r| r.app.key == app.key) {
                Some(r) => {
                    r.present = true;
                    if !app.detail.is_empty() || !r.recording {
                        r.app = app;
                    }
                }
                None => self.apps.push(AppRow {
                    chain_name: app.name.clone(),
                    app,
                    selected: false,
                    present: true,
                    recording: false,
                }),
            }
        }
        if self.debug.is_some() && !self.apps.is_empty() && self.main_key.is_none() {
            for r in &mut self.apps {
                r.selected = true;
            }
            self.main_key = Some(self.apps[0].app.key.clone());
        }
    }

    /// 録音するチェーンの一覧（名前の重複は番号を付けて避ける）。
    fn chain_setups(&self) -> Vec<ChainSetup> {
        let mut setups: Vec<ChainSetup> = Vec::new();
        let mut used: Vec<String> = Vec::new();
        let mut unique = |name: &str| {
            let base = if name.trim().is_empty() {
                "音声".to_owned()
            } else {
                name.trim().to_owned()
            };
            let mut n = base.clone();
            let mut i = 2;
            while used.contains(&n) {
                n = format!("{base} ({i})");
                i += 1;
            }
            used.push(n.clone());
            n
        };
        // メインを先頭に。
        let mut rows: Vec<&AppRow> = self.apps.iter().filter(|r| r.selected).collect();
        rows.sort_by_key(|r| Some(&r.app.key) != self.main_key.as_ref());
        for r in rows {
            setups.push(ChainSetup {
                name: unique(&r.chain_name),
                main: Some(&r.app.key) == self.main_key.as_ref(),
                source: AudioSource::App(r.app.clone()),
            });
        }
        if self.mic_on
            && let Some(m) = self.mics.get(self.mic)
        {
            setups.push(ChainSetup {
                name: unique(MIC_CHAIN),
                main: false,
                source: AudioSource::Mic(m.clone()),
            });
        }
        setups
    }

    // -----------------------------------------------------------------------
    // 録画

    fn output_path(&self) -> PathBuf {
        let stamp = chrono::Local::now().format("%Y-%m-%d %H-%M-%S");
        let base = format!("録画 {stamp}");
        let mut p = self.settings.out_dir.join(format!("{base}.fdmv"));
        let mut i = 2;
        while p.exists() {
            p = self.settings.out_dir.join(format!("{base} ({i}).fdmv"));
            i += 1;
        }
        p
    }

    fn can_start(&self) -> Result<(), &'static str> {
        if self.ff.is_err() {
            return Err("ffmpeg が見つかりません");
        }
        if self.capture.is_none() {
            return Err("録画する画面を選んでください");
        }
        match &self.encoders {
            None => return Err("使えるエンコーダを調べています…"),
            Some(l) if l.is_empty() => return Err("使える映像エンコーダがありません"),
            Some(_) => {}
        }
        if self.chain_setups().is_empty() {
            return Err("録音するアプリかマイクを選んでください");
        }
        if !self
            .apps
            .iter()
            .any(|r| r.selected && Some(&r.app.key) == self.main_key.as_ref())
        {
            return Err("メインにするアプリを選んでください");
        }
        Ok(())
    }

    fn start_recording(&mut self, ctx: &egui::Context) {
        let (Ok(ff), Some(cap)) = (&self.ff, &self.capture) else {
            return;
        };
        if let Err(e) = std::fs::create_dir_all(&self.settings.out_dir) {
            self.error = Some(format!(
                "保存先 {} を作れません: {e}",
                self.settings.out_dir.display()
            ));
            return;
        }
        let setups = self.chain_setups();
        let s = &self.settings;
        let opts = RecordOptions {
            fps: s.fps,
            quality: s.quality,
            mode: s.mode,
            encoder: s.encoder.clone(),
            max_height: s.max_height,
            audio_bitrate: s.audio_bitrate.clone(),
            title: Some(s.title.clone()).filter(|t| !t.trim().is_empty()),
        };
        let (ff, slot, output) = (ff.clone(), cap.slot(), self.output_path());
        self.output = output.clone();
        for r in &mut self.apps {
            r.recording = r.selected;
        }
        self.mic_recording = self.mic_on;
        self.levels.clear();
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Recorder::start(&ff, slot, setups, opts, output));
            ctx.request_repaint();
        });
        self.phase = Phase::Starting(rx);
    }

    fn stop_recording(&mut self, ctx: &egui::Context) {
        let Phase::Recording(rec) = std::mem::replace(&mut self.phase, Phase::Idle) else {
            return;
        };
        let Ok(ff) = self.ff.clone() else { return };
        let progress: Arc<Mutex<Option<(Stage, f64)>>> = Arc::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let (p, c, ctx2) = (progress.clone(), cancel.clone(), ctx.clone());
        std::thread::spawn(move || {
            let result = rec.stop().and_then(|recording| {
                let r = recording.finalize(
                    &ff,
                    &mut |stage, f| {
                        *p.lock().unwrap() = Some((stage, f));
                        ctx2.request_repaint();
                    },
                    &c,
                );
                if r.is_err() && c.load(Ordering::Relaxed) {
                    recording.discard();
                }
                r
            });
            let _ = tx.send(result);
            ctx2.request_repaint();
        });
        self.phase = Phase::Finishing {
            rx,
            progress,
            cancel,
            output: std::mem::take(&mut self.output),
        };
        for r in &mut self.apps {
            r.recording = false;
        }
        self.mic_recording = false;
    }

    fn poll_phase(&mut self, ctx: &egui::Context) {
        match &mut self.phase {
            Phase::Starting(rx) => match rx.try_recv() {
                Ok(Ok(rec)) => self.phase = Phase::Recording(Box::new(rec)),
                Ok(Err(e)) => {
                    self.error = Some(format!("録画を開始できません: {e:#}"));
                    self.phase = Phase::Idle;
                    for r in &mut self.apps {
                        r.recording = false;
                    }
                    self.mic_recording = false;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.phase = Phase::Idle,
            },
            Phase::Recording(rec) => {
                self.chain_status = rec.chain_status();
                for c in &self.chain_status {
                    let l = self.levels.entry(c.name.clone()).or_insert(0.0);
                    *l = c.peak.max(*l * 0.85);
                }
                ctx.request_repaint_after(Duration::from_millis(50));
            }
            Phase::Finishing { rx, output, .. } => match rx.try_recv() {
                Ok(Ok(report)) => {
                    let output = std::mem::take(output);
                    self.phase = Phase::Done { output, report };
                    if self.close_when_done {
                        ctx.send_viewport_cmd(ViewportCommand::Close);
                    }
                }
                Ok(Err(e)) => {
                    self.error = Some(format!("録画の保存に失敗しました: {e:#}"));
                    self.phase = Phase::Idle;
                    self.close_when_done = false;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.phase = Phase::Idle,
            },
            Phase::Idle | Phase::Done { .. } => {}
        }
    }

    /// 録画中にアプリを足す。
    fn add_live_chain(&mut self, key: &str) {
        let Phase::Recording(rec) = &mut self.phase else {
            return;
        };
        let used: Vec<String> = self.chain_status.iter().map(|c| c.name.clone()).collect();
        let Some(row) = self.apps.iter_mut().find(|r| r.app.key == key) else {
            return;
        };
        let mut name = row.chain_name.trim().to_owned();
        if name.is_empty() {
            name = row.app.name.clone();
        }
        let base = name.clone();
        let mut i = 2;
        while used.contains(&name) {
            name = format!("{base} ({i})");
            i += 1;
        }
        let setup = ChainSetup {
            name,
            main: false,
            source: AudioSource::App(row.app.clone()),
        };
        match rec.add_chain(setup) {
            Ok(()) => {
                row.selected = true;
                row.recording = true;
            }
            Err(e) => {
                row.selected = false;
                self.error = Some(format!("{e:#}"));
            }
        }
    }

    fn add_live_mic(&mut self) {
        let Phase::Recording(rec) = &mut self.phase else {
            return;
        };
        let Some(m) = self.mics.get(self.mic) else {
            return;
        };
        let mut name = MIC_CHAIN.to_owned();
        let mut i = 2;
        while self.chain_status.iter().any(|c| c.name == name) {
            name = format!("{MIC_CHAIN} ({i})");
            i += 1;
        }
        match rec.add_chain(ChainSetup {
            name,
            main: false,
            source: AudioSource::Mic(m.clone()),
        }) {
            Ok(()) => self.mic_recording = true,
            Err(e) => {
                self.mic_on = false;
                self.error = Some(format!("{e:#}"));
            }
        }
    }

    // -----------------------------------------------------------------------
    // 画面

    fn video_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.heading("映像");
        ui.add_space(4.0);
        let busy = !self.is_idle();
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!busy && self.capture_job.is_none(), |ui| {
                let label = self
                    .targets
                    .get(self.target)
                    .map(|t| t.label.clone())
                    .unwrap_or_else(|| "（録画できる画面がありません）".into());
                let mut changed = false;
                if self.targets.len() > 1 {
                    egui::ComboBox::from_id_salt("target")
                        .width(320.0)
                        .selected_text(&label)
                        .show_ui(ui, |ui| {
                            for (i, t) in self.targets.iter().enumerate() {
                                changed |=
                                    ui.selectable_value(&mut self.target, i, &t.label).changed();
                            }
                        });
                } else {
                    ui.label(&label);
                }
                let portal = matches!(
                    self.targets.get(self.target).map(|t| &t.target),
                    Some(VideoTarget::Portal)
                );
                let text = match (portal, self.capture.is_some()) {
                    (true, false) => "選ぶ…",
                    (true, true) => "選び直す…",
                    (false, false) => "取り込む",
                    (false, true) => "取り込み直す",
                };
                if ui.button(text).clicked() || (changed && !portal) {
                    self.start_capture(ctx);
                }
                if ui
                    .button("更新")
                    .on_hover_text("画面・ウィンドウの一覧を取り直す")
                    .clicked()
                    && let Ok(t) = video::targets()
                {
                    self.targets = t;
                    self.target = self.target.min(self.targets.len().saturating_sub(1));
                }
            });
            if self.capture_job.is_some() {
                ui.spinner();
                ui.label("画面を選んでいます…");
            }
        });
        if let Some((w, h)) = self.capture_size {
            ui.label(RichText::new(format!("{w}×{h}")).small().weak());
        }
        if let Some(e) = self.capture.as_ref().and_then(|c| c.error()) {
            ui.colored_label(REC, e);
        }
        ui.add_space(4.0);
        let avail = ui.available_size() - egui::vec2(0.0, 4.0);
        let (rect, _) = ui.allocate_exact_size(avail, egui::Sense::hover());
        ui.painter().rect_filled(rect, 4.0, Color32::from_gray(16));
        match &self.preview {
            Some(tex) => {
                let size = tex.size_vec2();
                let scale = (rect.width() / size.x).min(rect.height() / size.y);
                let r = egui::Rect::from_center_size(rect.center(), size * scale);
                ui.painter().image(
                    tex.id(),
                    r,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            None => {
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    if self.capture.is_some() {
                        "映像を待っています…"
                    } else {
                        "録画する画面またはウィンドウを選んでください"
                    },
                    egui::FontId::proportional(16.0),
                    Color32::from_gray(150),
                );
            }
        }
        if let Phase::Recording(rec) = &self.phase {
            let t = rec.elapsed().as_secs_f64();
            let text = format!("● REC {}", format_time(t));
            let pos = rect.left_top() + egui::vec2(12.0, 10.0);
            ui.painter().text(
                pos,
                egui::Align2::LEFT_TOP,
                text,
                egui::FontId::proportional(18.0),
                REC,
            );
        }
    }

    fn audio_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("音声");
        ui.label(
            RichText::new("録音するアプリを選ぶと、アプリごとに別のチェーンになります。メインはデフォルトチェーン（普通に再生したときに鳴る音）になります。")
                .small()
                .weak(),
        );
        ui.add_space(4.0);
        let idle = self.is_idle();
        let recording = matches!(self.phase, Phase::Recording(_));
        let mut add_live: Option<String> = None;
        egui::ScrollArea::vertical()
            .id_salt("apps")
            .max_height(ui.available_height() - 150.0)
            .show(ui, |ui| {
                if self.apps.is_empty() {
                    ui.label(
                        RichText::new(if cfg!(target_os = "linux") {
                            "音を出しているアプリがありません。録りたいアプリで音を出すと、ここに表示されます。"
                        } else {
                            "アプリを探しています…"
                        })
                        .weak(),
                    );
                }
                egui::Grid::new("apps_grid")
                    .num_columns(3)
                    .spacing([8.0, 6.0])
                    .striped(true)
                    .show(ui, |ui| {
                        if !self.apps.is_empty() {
                            ui.label(RichText::new("録音").small().weak());
                            ui.label(RichText::new("メイン").small().weak());
                            ui.label(RichText::new("アプリ / チェーン名").small().weak());
                            ui.end_row();
                        }
                        for r in &mut self.apps {
                            // 録音する
                            let can_toggle = idle || (recording && !r.recording);
                            let mut sel = r.selected;
                            if ui
                                .add_enabled(can_toggle, egui::Checkbox::without_text(&mut sel))
                                .changed()
                            {
                                if recording {
                                    if sel {
                                        add_live = Some(r.app.key.clone());
                                    }
                                } else {
                                    r.selected = sel;
                                    if sel && self.main_key.is_none() {
                                        self.main_key = Some(r.app.key.clone());
                                    }
                                }
                            }
                            // メイン
                            let is_main = self.main_key.as_ref() == Some(&r.app.key);
                            if ui
                                .add_enabled(idle && r.selected, egui::RadioButton::new(is_main, ""))
                                .clicked()
                            {
                                self.main_key = Some(r.app.key.clone());
                            }
                            // 名前・補足
                            ui.vertical(|ui| {
                                ui.horizontal(|ui| {
                                    let dot = if r.app.playing && r.present {
                                        RichText::new("●").color(Color32::from_rgb(0x4c, 0xb0, 0x50))
                                    } else {
                                        RichText::new("●").color(Color32::from_gray(110))
                                    };
                                    ui.label(dot).on_hover_text(if r.app.playing && r.present {
                                        "音を出しています"
                                    } else {
                                        "音を出していません"
                                    });
                                    if r.selected && idle {
                                        ui.add(
                                            egui::TextEdit::singleline(&mut r.chain_name)
                                                .desired_width(180.0),
                                        )
                                        .on_hover_text("チェーン名");
                                    } else {
                                        ui.label(&r.chain_name);
                                    }
                                    if is_main && r.selected {
                                        ui.label(RichText::new("メイン").small().color(ACCENT));
                                    }
                                    if let Some(pid) = r.app.pid {
                                        ui.label(RichText::new(format!("PID {pid}")).small().weak());
                                    }
                                });
                                if !r.app.detail.is_empty() {
                                    ui.add(
                                        egui::Label::new(RichText::new(&r.app.detail).small().weak())
                                            .truncate(),
                                    );
                                }
                            });
                            ui.end_row();
                        }
                    });
                if let Some(e) = &self.apps_error {
                    ui.colored_label(REC, e);
                }
            });
        if let Some(key) = add_live {
            self.add_live_chain(&key);
        }

        ui.separator();
        ui.horizontal(|ui| {
            let mut on = self.mic_on;
            let enabled = idle || (recording && !self.mic_recording);
            if ui
                .add_enabled(
                    enabled,
                    egui::Checkbox::new(
                        &mut on,
                        format!("マイクも録音する（チェーン名「{MIC_CHAIN}」）"),
                    ),
                )
                .changed()
            {
                self.mic_on = on;
                if recording && on {
                    self.add_live_mic();
                }
            }
        });
        ui.add_enabled_ui(idle, |ui| {
            if let Some(m) = self.mics.get(self.mic) {
                egui::ComboBox::from_id_salt("mic")
                    .width(ui.available_width().min(360.0))
                    .selected_text(&m.name)
                    .show_ui(ui, |ui| {
                        for (i, m) in self.mics.iter().enumerate() {
                            ui.selectable_value(&mut self.mic, i, &m.name);
                        }
                    });
            }
        });

        if recording {
            ui.separator();
            ui.label(RichText::new("録音中のチェーン").strong());
            for c in &self.chain_status {
                let level = self.levels.get(&c.name).copied().unwrap_or(0.0);
                ui.horizontal(|ui| {
                    let name = if c.main {
                        format!("{}（メイン）", c.name)
                    } else {
                        c.name.clone()
                    };
                    ui.add_sized([150.0, 16.0], egui::Label::new(name).truncate());
                    level_meter(ui, level, c.receiving);
                });
                if let Some(e) = &c.error {
                    ui.colored_label(REC, e);
                }
            }
        }
    }

    fn settings_panel(&mut self, ui: &mut egui::Ui) {
        let idle = self.is_idle();
        ui.add_enabled_ui(idle, |ui| {
            egui::Grid::new("settings")
                .num_columns(4)
                .spacing([10.0, 6.0])
                .show(ui, |ui| {
                    ui.label("保存先");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::Label::new(self.settings.out_dir.display().to_string())
                                .truncate(),
                        );
                        if ui.button("変更…").clicked()
                            && let Some(d) = rfd::FileDialog::new()
                                .set_directory(&self.settings.out_dir)
                                .pick_folder()
                        {
                            self.settings.out_dir = d;
                        }
                    });
                    ui.label("タイトル");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.title)
                            .hint_text("（任意）")
                            .desired_width(200.0),
                    );
                    ui.end_row();

                    ui.label("フレームレート");
                    egui::ComboBox::from_id_salt("fps")
                        .selected_text(format!("{} fps", self.settings.fps))
                        .show_ui(ui, |ui| {
                            for f in [15, 24, 30, 60] {
                                ui.selectable_value(&mut self.settings.fps, f, format!("{f} fps"));
                            }
                        });
                    ui.label("画質");
                    egui::ComboBox::from_id_salt("quality")
                        .selected_text(quality_label(self.settings.quality))
                        .show_ui(ui, |ui| {
                            for q in Quality::ALL {
                                ui.selectable_value(
                                    &mut self.settings.quality,
                                    q,
                                    quality_label(q),
                                );
                            }
                        });
                    ui.end_row();

                    ui.label("エンコーダ");
                    match &self.encoders {
                        None => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("使えるエンコーダを調べています…");
                            });
                        }
                        Some(list) => {
                            let auto = list
                                .first()
                                .map(|e| format!("自動: {}", e.label()))
                                .unwrap_or_else(|| "（使えるエンコーダがありません）".into());
                            let selected = match &self.settings.encoder {
                                None => auto.clone(),
                                Some(n) => list
                                    .iter()
                                    .find(|e| &e.name == n)
                                    .map(|e| e.label())
                                    .unwrap_or_else(|| n.clone()),
                            };
                            egui::ComboBox::from_id_salt("encoder")
                                .selected_text(selected)
                                .width(300.0)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut self.settings.encoder, None, auto);
                                    for e in list {
                                        ui.selectable_value(
                                            &mut self.settings.encoder,
                                            Some(e.name.clone()),
                                            e.label(),
                                        )
                                        .on_hover_text(encoder_note(e));
                                    }
                                });
                        }
                    }
                    ui.label("圧縮");
                    if self.selected_encoder().is_some_and(|e| e.needs_conversion()) {
                        ui.label("停止後に AV1 へ変換")
                            .on_hover_text("GPU の H.264 / HEVC で録った映像は、停止後に AV1 へ変換します（録画の長さの数倍かかることがあります）");
                    } else {
                        egui::ComboBox::from_id_salt("mode")
                            .selected_text(mode_label(self.settings.mode))
                            .show_ui(ui, |ui| {
                                for m in [VideoMode::Direct, VideoMode::Reencode] {
                                    ui.selectable_value(&mut self.settings.mode, m, mode_label(m))
                                        .on_hover_text(mode_note(m));
                                }
                            })
                            .response
                            .on_hover_text(mode_note(self.settings.mode));
                    }
                    ui.end_row();


                    ui.label("解像度");
                    egui::ComboBox::from_id_salt("height")
                        .selected_text(height_label(self.settings.max_height))
                        .show_ui(ui, |ui| {
                            for h in [None, Some(1440), Some(1080), Some(720)] {
                                ui.selectable_value(
                                    &mut self.settings.max_height,
                                    h,
                                    height_label(h),
                                );
                            }
                        });
                    ui.label("音声");
                    egui::ComboBox::from_id_salt("abr")
                        .selected_text(&self.settings.audio_bitrate)
                        .show_ui(ui, |ui| {
                            for b in ["96k", "128k", "160k", "192k", "256k"] {
                                ui.selectable_value(
                                    &mut self.settings.audio_bitrate,
                                    b.to_owned(),
                                    b,
                                );
                            }
                        });
                    ui.end_row();
                });
        });
    }

    fn control_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            match &self.phase {
                Phase::Idle | Phase::Done { .. } => {
                    let ready = self.can_start();
                    let button = egui::Button::new(
                        RichText::new("●  録画開始").size(18.0).color(Color32::WHITE),
                    )
                    .fill(REC)
                    .min_size(egui::vec2(160.0, 40.0));
                    let resp = ui.add_enabled(ready.is_ok(), button);
                    if let Err(why) = ready {
                        ui.label(RichText::new(why).weak());
                    } else if resp.clicked() {
                        self.start_recording(ctx);
                    }
                }
                Phase::Starting(_) => {
                    ui.add_enabled(
                        false,
                        egui::Button::new(RichText::new("開始しています…").size(18.0))
                            .min_size(egui::vec2(160.0, 40.0)),
                    );
                    ui.spinner();
                }
                Phase::Recording(rec) => {
                    let elapsed = rec.elapsed().as_secs_f64();
                    let dropped = rec.video_stats().dropped();
                    let error = rec.video_stats().error();
                    let button = egui::Button::new(
                        RichText::new("■  停止して保存").size(18.0).color(Color32::WHITE),
                    )
                    .fill(Color32::from_gray(70))
                    .min_size(egui::vec2(160.0, 40.0));
                    if ui.add(button).clicked() {
                        self.stop_recording(ctx);
                        return;
                    }
                    ui.label(RichText::new(format_time(elapsed)).size(20.0).monospace().color(REC));
                    if let Some(e) = error {
                        ui.colored_label(REC, e);
                    } else if dropped > 0 {
                        ui.colored_label(
                            Color32::from_rgb(0xe0, 0xa0, 0x30),
                            format!("コマ落ち {dropped} フレーム"),
                        )
                        .on_hover_text(
                            "エンコードが追いつかず、フレームを間引きました（映像と音声はずれません）。\n多い場合はフレームレートか解像度を下げるか、GPU のエンコーダを選んでください",
                        );
                    }
                }
                Phase::Finishing {
                    progress, cancel, ..
                } => {
                    let p = *progress.lock().unwrap();
                    let (text, frac) = match p {
                        None => ("録画を止めています…".to_owned(), 0.0),
                        Some((Stage::Audio, f)) => ("音声をまとめています…".to_owned(), f * 0.2),
                        Some((Stage::Video, f)) => (
                            format!("映像を AV1 に変換しています… {:.0}%", f * 100.0),
                            0.2 + f * 0.7,
                        ),
                        Some((Stage::Pack, f)) => ("FDMV に書き出しています…".to_owned(), 0.9 + f * 0.1),
                    };
                    ui.label(text);
                    ui.add(
                        egui::ProgressBar::new(frac as f32)
                            .desired_width(260.0)
                            .animate(true),
                    );
                    if ui.button("中止").on_hover_text("録画を破棄する").clicked() {
                        cancel.store(true, Ordering::Relaxed);
                    }
                }
            }
            if let Phase::Done { output, report } = &self.phase {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("フォルダを開く").clicked() {
                        open_path(output.parent().unwrap_or(Path::new(".")));
                    }
                    if ui.button("再生").clicked() {
                        open_in_player(output);
                    }
                    let mut text = format!(
                        "保存しました: {}（{}）",
                        output.file_name().unwrap_or_default().to_string_lossy(),
                        format_size(report.pack.file_size)
                    );
                    if !report.empty_chains.is_empty() {
                        text.push_str(&format!(
                            "\n音が無かったため入れなかったチェーン: {}",
                            report.empty_chains.join("、")
                        ));
                    }
                    ui.add(egui::Label::new(RichText::new(text).color(ACCENT)).truncate());
                });
            }
        });
    }

    fn error_modal(&mut self, ctx: &egui::Context) {
        let Some(msg) = self.error.clone() else {
            return;
        };
        let mut close = false;
        egui::Modal::new(egui::Id::new("error")).show(ctx, |ui| {
            ui.set_max_width(520.0);
            ui.heading("エラー");
            egui::ScrollArea::vertical()
                .max_height(300.0)
                .show(ui, |ui| {
                    ui.label(&msg);
                });
            ui.add_space(8.0);
            if ui.button("OK").clicked() {
                close = true;
            }
        });
        if close {
            self.error = None;
        }
    }

    fn close_modal(&mut self, ctx: &egui::Context) {
        if !self.confirm_close {
            return;
        }
        let mut choice = None;
        egui::Modal::new(egui::Id::new("close")).show(ctx, |ui| {
            ui.set_max_width(400.0);
            ui.heading("録画中です");
            ui.label("録画を止めて保存してから終了しますか？");
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("保存して終了").clicked() {
                    choice = Some(1);
                }
                if ui.button("破棄して終了").clicked() {
                    choice = Some(2);
                }
                if ui.button("キャンセル").clicked() {
                    choice = Some(0);
                }
            });
        });
        match choice {
            Some(1) => {
                self.confirm_close = false;
                self.close_when_done = true;
                if matches!(self.phase, Phase::Recording(_)) {
                    self.stop_recording(ctx);
                }
            }
            Some(2) => {
                self.confirm_close = false;
                match std::mem::replace(&mut self.phase, Phase::Idle) {
                    Phase::Recording(mut rec) => rec.abort(),
                    Phase::Finishing { cancel, .. } => cancel.store(true, Ordering::Relaxed),
                    _ => {}
                }
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
            Some(0) => self.confirm_close = false,
            _ => {}
        }
    }

    fn debug_tick(&mut self, ctx: &egui::Context) {
        let Some(d) = &mut self.debug else { return };
        d.frames += 1;
        ctx.request_repaint_after(Duration::from_millis(30));
        let frames = d.frames;
        let shot = d.shot;
        if self.is_idle()
            && !matches!(self.phase, Phase::Done { .. })
            && self.can_start().is_ok()
            && frames > 20
        {
            self.start_recording(ctx);
            return;
        }
        if let Phase::Recording(rec) = &self.phase {
            let t = rec.elapsed().as_secs_f64();
            if t > 2.5 && !shot {
                ctx.send_viewport_cmd(ViewportCommand::Screenshot(Default::default()));
                self.debug.as_mut().unwrap().shot = true;
            } else if t > 4.0 && shot {
                self.stop_recording(ctx);
            }
        }
        let image = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = image {
            let path = self.debug.as_ref().unwrap().path.clone();
            let mut ppm = format!("P6\n{} {}\n255\n", img.size[0], img.size[1]).into_bytes();
            for c in &img.pixels {
                ppm.extend_from_slice(&[c.r(), c.g(), c.b()]);
            }
            let res = std::fs::write(&path, ppm);
            println!("debug: screenshot {res:?}");
        }
        if let Phase::Done { output, report } = &self.phase {
            println!(
                "debug: saved {} ({} bytes) empty={:?}",
                output.display(),
                report.pack.file_size,
                report.empty_chains
            );
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
        if self.error.is_some() {
            println!("debug: error {:?}", self.error);
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if ctx.input(|i| i.viewport().close_requested())
            && !matches!(self.phase, Phase::Idle | Phase::Done { .. })
            && !self.close_when_done
        {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.confirm_close = true;
        }
        self.poll_encoders();
        self.poll_capture(&ctx);
        self.poll_apps(&ctx);
        self.poll_phase(&ctx);

        egui::Panel::bottom("controls").show(ui, |ui| {
            ui.add_space(6.0);
            self.settings_panel(ui);
            ui.separator();
            self.control_bar(ui, &ctx);
            ui.add_space(6.0);
        });
        egui::Panel::right("audio")
            .resizable(true)
            .default_size(420.0)
            .min_size(320.0)
            .show(ui, |ui| {
                ui.add_space(6.0);
                self.audio_panel(ui);
            });
        egui::CentralPanel::default().show(ui, |ui| self.video_panel(ui, &ctx));

        if let Err(e) = &self.ff {
            let msg = format!("ffmpeg が見つかりません: {e}");
            if self.error.is_none() && !msg.is_empty() && self.debug.is_none() {
                egui::Window::new("ffmpeg")
                    .collapsible(false)
                    .show(&ctx, |ui| ui.label(msg));
            }
        }
        self.error_modal(&ctx);
        self.close_modal(&ctx);
        self.debug_tick(&ctx);
        ctx.request_repaint_after(PREVIEW_INTERVAL);
    }
}

fn level_meter(ui: &mut egui::Ui, level: f32, receiving: bool) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width().min(200.0), 10.0),
        egui::Sense::hover(),
    );
    let p = ui.painter();
    p.rect_filled(rect, 2.0, Color32::from_gray(40));
    // -60 dB – 0 dB を 0–1 に。
    let db = 20.0 * level.max(1e-6).log10();
    let f = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
    let mut r = rect;
    r.set_width(rect.width() * f);
    let color = if db > -3.0 {
        Color32::from_rgb(0xe0, 0x50, 0x40)
    } else if receiving {
        Color32::from_rgb(0x4c, 0xb0, 0x50)
    } else {
        Color32::from_gray(100)
    };
    p.rect_filled(r, 2.0, color);
}

fn quality_label(q: Quality) -> &'static str {
    match q {
        Quality::Best => "最高画質",
        Quality::High => "高画質",
        Quality::Standard => "標準",
        Quality::Small => "小容量",
    }
}

fn encoder_note(e: &LiveEncoder) -> &'static str {
    match e.kind {
        EncoderKind::HardwareAv1 => {
            "GPU で AV1 にします。CPU をほとんど使わず、停止後すぐに保存されます"
        }
        EncoderKind::HardwareIntermediate => {
            "GPU で録るので録画中は軽く、ゲームなどの動きを妨げません。停止後に CPU で AV1 へ変換します"
        }
        EncoderKind::SoftwareAv1 => {
            "CPU で AV1 にします。高解像度・高フレームレートではとても重くなります"
        }
    }
}

fn mode_label(m: VideoMode) -> &'static str {
    match m {
        VideoMode::Direct => "録画しながら圧縮",
        VideoMode::Reencode => "停止後に圧縮（小さい）",
    }
}

fn mode_note(m: VideoMode) -> &'static str {
    match m {
        VideoMode::Direct => "録画中に最終的な形式で圧縮します。停止後すぐに保存されます",
        VideoMode::Reencode => {
            "録画中は軽い設定で一時保存し、停止後にじっくり圧縮します。ファイルは小さくなりますが、保存に時間がかかります"
        }
    }
}

fn height_label(h: Option<u32>) -> String {
    match h {
        None => "元のまま".into(),
        Some(h) => format!("{h}p まで"),
    }
}

fn format_time(t: f64) -> String {
    let s = t.max(0.0) as u64;
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

fn format_size(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1e9 {
        format!("{:.2} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.1} MB", b / 1e6)
    } else {
        format!("{:.0} KB", b / 1e3)
    }
}

/// 同じフォルダの fdmv-player で開く（無ければ OS に任せる）。
fn open_in_player(path: &Path) {
    let exe = if cfg!(windows) {
        "fdmv-player.exe"
    } else {
        "fdmv-player"
    };
    if let Some(player) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join(exe)))
        .filter(|p| p.exists())
        && std::process::Command::new(player).arg(path).spawn().is_ok()
    {
        return;
    }
    open_path(path);
}

fn open_path(path: &Path) {
    let program = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(program).arg(path).spawn();
}
