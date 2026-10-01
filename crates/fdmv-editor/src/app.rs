use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use eframe::egui::{
    self, Color32, ColorImage, Key, KeyboardShortcut, Modifiers, TextureHandle, TextureOptions,
    Vec2, ViewportCommand,
};
use fdmv_edit::preview::{TimelineAudio, TimelineVideo};
use fdmv_edit::proxy::probe_source;
use fdmv_edit::{Id, Project, ProxyStore};
use fdmv_gui::{AudioOutput, VideoOutput};
use libfdmv::ffmpeg::Ffmpeg;
use libfdmv::time::format_time;

use crate::history::History;
use crate::jobs::{ExportJob, ProxyJobs, ProxyState};
use crate::timeline::{self, Action, Edit, Row, Selection, TimelineCtx, TimelineState};

pub const PROJECT_EXT: &str = "fdmvproj";

struct Preview {
    audio: AudioOutput<TimelineAudio>,
    video: VideoOutput<TimelineVideo>,
}

/// 未保存の変更があるときに確認してから行う操作。
#[derive(Clone)]
enum Pending {
    New,
    Open(Option<PathBuf>),
    Quit,
}

pub struct App {
    pub(crate) ff: Option<Ffmpeg>,
    pub(crate) proxies: ProxyStore,
    pub(crate) jobs: Option<ProxyJobs>,
    pub(crate) project: Project,
    pub(crate) project_path: Option<PathBuf>,
    pub(crate) dirty: bool,
    history: History,
    pub(crate) selection: Selection,
    pub(crate) playhead: f64,
    pub(crate) in_point: Option<f64>,
    pub(crate) out_point: Option<f64>,
    pub(crate) muted: HashSet<Id>,
    pub(crate) timeline: TimelineState,
    preview: Option<Preview>,
    texture: Option<TextureHandle>,
    frame_size: Vec2,
    proxy_generation: u64,
    pub(crate) master_volume: f32,
    pub(crate) export: Option<ExportJob>,
    pub(crate) show_export: bool,
    pub(crate) export_path: String,
    pub(crate) error: Option<String>,
    pending: Option<Pending>,
    allow_close: bool,
    fit_requested: bool,
    pub(crate) debug_screenshot: Option<(PathBuf, u32)>,
}

impl App {
    pub fn new(ctx: &egui::Context, path: Option<PathBuf>) -> Self {
        let mut error = None;
        let ff = match Ffmpeg::locate() {
            Ok(f) => Some(f),
            Err(e) => {
                error = Some(format!(
                    "ffmpeg が見つかりません。素材の読み込みと書き出しにはffmpegが必要です。\n{e}"
                ));
                None
            }
        };
        let proxies = match ProxyStore::new(ProxyStore::default_dir()) {
            Ok(p) => p,
            Err(e) => {
                error = Some(format!("{e:#}"));
                ProxyStore::new(std::env::temp_dir().join("fdmv-editor-proxies")).expect("temp dir")
            }
        };
        let jobs = ff
            .as_ref()
            .map(|ff| ProxyJobs::new(ff.clone(), proxies.clone(), ctx.clone()));
        let mut app = App {
            ff,
            proxies,
            jobs,
            project: Project::new(),
            project_path: None,
            dirty: false,
            history: History::default(),
            selection: Selection::None,
            playhead: 0.0,
            in_point: None,
            out_point: None,
            muted: HashSet::new(),
            timeline: TimelineState::default(),
            preview: None,
            texture: None,
            frame_size: Vec2::new(16.0, 9.0),
            proxy_generation: 0,
            master_volume: 1.0,
            export: None,
            show_export: false,
            export_path: String::new(),
            error,
            pending: None,
            allow_close: false,
            fit_requested: false,
            debug_screenshot: std::env::var_os("FDMV_DEBUG_SCREENSHOT").map(|p| (p.into(), 0)),
        };
        app.start_preview(ctx);
        if let Some(p) = path {
            if p.extension().is_some_and(|e| e == PROJECT_EXT) {
                app.open_project(ctx, &p);
            } else {
                app.import_files(&[p], true);
            }
        }
        app
    }

    // ------------------------------------------------------------------
    // プレビュー

    fn start_preview(&mut self, ctx: &egui::Context) {
        self.preview = None;
        let audio = TimelineAudio::new(self.proxies.clone(), &self.project, &self.muted);
        let video = TimelineVideo::new(self.proxies.clone(), &self.project);
        match (
            AudioOutput::start(audio),
            VideoOutput::start(video, ctx.clone()),
        ) {
            (Ok(audio), Ok(video)) => {
                audio.set_volume(self.master_volume);
                self.preview = Some(Preview { audio, video });
                self.refresh_preview();
            }
            (Err(e), _) | (_, Err(e)) => {
                self.error = Some(format!("プレビューを開始できません: {e:#}"))
            }
        }
    }

    /// 編集内容をプレビューに反映し、再生位置からやり直す。
    fn refresh_preview(&mut self) {
        let Some(pv) = &self.preview else { return };
        let (p1, p2, muted) = (
            self.project.clone(),
            self.project.clone(),
            self.muted.clone(),
        );
        pv.audio.with(move |a| {
            a.set_project(&p1, &muted);
            Ok(())
        });
        pv.video.with(move |v| {
            v.set_project(&p2);
            Ok(())
        });
        self.playhead = self.playhead.clamp(0.0, self.project.duration());
        pv.audio.seek((self.playhead * 48_000.0).round() as i64);
        pv.video.seek(self.playhead);
    }

    pub(crate) fn is_playing(&self) -> bool {
        self.preview.as_ref().is_some_and(|p| p.audio.is_playing())
    }

    pub(crate) fn toggle_play(&mut self) {
        let Some(pv) = &self.preview else { return };
        if pv.audio.is_playing() {
            pv.audio.set_playing(false);
            self.playhead = self.project.snap(self.playhead);
            self.seek(self.playhead);
        } else {
            if self.playhead >= self.project.duration() - 1e-6 {
                self.seek(0.0);
            }
            if let Some(pv) = &self.preview {
                pv.audio.set_playing(true);
            }
        }
    }

    pub(crate) fn seek(&mut self, t: f64) {
        self.playhead = t.clamp(0.0, self.project.duration());
        if let Some(pv) = &self.preview {
            pv.audio.seek((self.playhead * 48_000.0).round() as i64);
            pv.video.seek(self.playhead);
        }
    }

    pub(crate) fn set_master_volume(&mut self, v: f32) {
        self.master_volume = v;
        if let Some(pv) = &self.preview {
            pv.audio.set_volume(v);
        }
    }

    fn update_preview(&mut self, ctx: &egui::Context) {
        let Some(pv) = &self.preview else { return };
        if let Some(e) = pv.video.take_error().or_else(|| pv.audio.take_error()) {
            self.error = Some(e);
        }
        if pv.audio.is_playing() {
            self.playhead = (pv.audio.position() as f64 / 48_000.0).min(self.project.duration());
            if pv.audio.finished() {
                pv.audio.set_playing(false);
            }
            let wait = pv
                .video
                .next_pts()
                .map(|p| (p - self.playhead).max(0.0))
                .unwrap_or(0.03)
                .min(0.03);
            ctx.request_repaint_after(Duration::from_secs_f64(wait));
        }
        if let Some(f) = pv.video.take_frame(self.playhead) {
            let img = ColorImage::from_rgba_unmultiplied([f.width, f.height], &f.rgba);
            self.frame_size = Vec2::new(f.width as f32, f.height as f32);
            match &mut self.texture {
                Some(t) => t.set(img, TextureOptions::LINEAR),
                None => {
                    self.texture = Some(ctx.load_texture("preview", img, TextureOptions::LINEAR))
                }
            }
        }
        // プロキシができたらプレビューを更新する
        if let Some(j) = &self.jobs {
            let g = j.generation();
            if g != self.proxy_generation {
                self.proxy_generation = g;
                self.refresh_preview();
            }
        }
    }

    pub(crate) fn preview_texture(&self) -> Option<(&TextureHandle, Vec2)> {
        self.texture.as_ref().map(|t| (t, self.frame_size))
    }

    pub(crate) fn audio_device(&self) -> Option<&str> {
        self.preview.as_ref().and_then(|p| p.audio.device_name())
    }

    // ------------------------------------------------------------------
    // 編集

    /// 元に戻すを記録してからプロジェクトを変更する。
    pub(crate) fn edit(&mut self, key: Option<&str>, f: impl FnOnce(&mut Project)) {
        self.history.record(&self.project, key);
        f(&mut self.project);
        self.dirty = true;
        self.validate_selection();
        self.refresh_preview();
    }

    /// 元に戻すの記録をせずに変更する（ドラッグ中の更新）。プレビューはドラッグ終了時に更新する。
    fn edit_live(&mut self, f: impl FnOnce(&mut Project)) {
        f(&mut self.project);
        self.dirty = true;
    }

    pub(crate) fn undo(&mut self) {
        if let Some(p) = self.history.undo(&self.project) {
            self.project = p;
            self.dirty = true;
            self.validate_selection();
            self.refresh_preview();
        }
    }

    pub(crate) fn redo(&mut self) {
        if let Some(p) = self.history.redo(&self.project) {
            self.project = p;
            self.dirty = true;
            self.validate_selection();
            self.refresh_preview();
        }
    }

    pub(crate) fn can_undo(&self) -> bool {
        self.history.can_undo()
    }

    pub(crate) fn can_redo(&self) -> bool {
        self.history.can_redo()
    }

    fn validate_selection(&mut self) {
        let p = &self.project;
        let ok = match self.selection {
            Selection::None => true,
            Selection::Video(id) => p.video.iter().any(|c| c.id == id),
            Selection::Audio { chain, clip } => p.audio_clip(chain, clip).is_some(),
            Selection::Chain(id) => p.chain(id).is_some(),
            Selection::Source(id) => p.source(id).is_some(),
        };
        if !ok {
            self.selection = Selection::None;
        }
    }

    pub(crate) fn split(&mut self) {
        let t = self.project.snap(self.playhead);
        let sel = self.selection;
        self.split_at(sel, t);
    }

    fn split_at(&mut self, sel: Selection, t: f64) {
        match sel {
            Selection::Audio { chain, clip }
                if self
                    .project
                    .audio_clip(chain, clip)
                    .is_some_and(|c| t > c.start && t < c.end()) =>
            {
                self.edit(None, |p| {
                    p.split_audio(chain, clip, t);
                });
            }
            _ => {
                if self.project.video_clip_at(t).is_some() {
                    self.edit(None, |p| {
                        p.split_video(t);
                    });
                }
            }
        }
    }

    pub(crate) fn delete_selected(&mut self) {
        match self.selection {
            Selection::Video(id) => self.edit(None, |p| {
                p.remove_video(id);
            }),
            Selection::Audio { chain, clip } => self.edit(None, |p| {
                p.remove_audio(chain, clip);
            }),
            Selection::Chain(id) => self.edit(None, |p| p.remove_chain(id)),
            Selection::Source(id) => self.edit(None, |p| p.remove_source(id)),
            Selection::None => {}
        }
    }

    pub(crate) fn range(&self) -> Option<(f64, f64)> {
        let (a, b) = (self.in_point?, self.out_point?);
        let (a, b) = (a.min(b), a.max(b));
        (b - a > 1e-6).then_some((a, b))
    }

    pub(crate) fn delete_range(&mut self) {
        let Some((a, b)) = self.range() else { return };
        self.edit(None, |p| p.delete_range(a, b));
        self.in_point = None;
        self.out_point = None;
        self.seek(a);
    }

    pub(crate) fn add_chain(&mut self) {
        let name = self.project.unique_chain_name("チェーン");
        let mut id = 0;
        self.edit(None, |p| id = p.add_chain(&name));
        self.selection = Selection::Chain(id);
    }

    pub(crate) fn toggle_mute(&mut self, id: Id) {
        if !self.muted.remove(&id) {
            self.muted.insert(id);
        }
        self.refresh_preview();
    }

    /// 素材を取り込む。`to_video` なら映像トラックの末尾にも並べる。
    pub(crate) fn import_files(&mut self, paths: &[PathBuf], to_video: bool) -> Vec<Id> {
        let Some(ff) = self.ff.clone() else {
            self.error = Some("ffmpeg が無いため素材を読み込めません".into());
            return Vec::new();
        };
        let mut ids = Vec::new();
        let mut errors = Vec::new();
        let mut sources = Vec::new();
        for path in paths {
            // 既に取り込み済みならそれを使う
            let canon = path.canonicalize().unwrap_or_else(|_| path.clone());
            if let Some(s) = self.project.sources.iter().find(|s| s.path == canon) {
                ids.push(s.id);
                continue;
            }
            match probe_source(&ff, path) {
                Ok(s) => sources.push(s),
                Err(e) => errors.push(format!("{e:#}")),
            }
        }
        if !sources.is_empty() || (to_video && !ids.is_empty()) {
            let existing = ids.clone();
            let mut new_ids = Vec::new();
            let was_empty = self.project.video.is_empty();
            self.edit(None, |p| {
                for s in sources {
                    new_ids.push(p.add_source(s));
                }
                if to_video {
                    for id in existing.iter().chain(new_ids.iter()) {
                        if p.source(*id).is_some_and(|s| s.video.is_some()) {
                            let _ = p.insert_video(*id, None);
                        }
                    }
                }
            });
            ids.extend(new_ids);
            if was_empty && to_video {
                self.fit_requested = true;
            }
        }
        self.ensure_proxies();
        if !errors.is_empty() {
            self.error = Some(errors.join("\n"));
        }
        ids
    }

    pub(crate) fn ensure_proxies(&self) {
        if let Some(j) = &self.jobs {
            for s in &self.project.sources {
                if s.path.exists() {
                    j.ensure(&self.proxies, s);
                }
            }
        }
    }

    pub(crate) fn proxy_state(&self, id: Id) -> Option<ProxyState> {
        let s = self.project.source(id)?;
        if self.proxies.is_ready(s) {
            return Some(ProxyState::Ready);
        }
        self.jobs.as_ref().and_then(|j| j.state(id))
    }

    pub(crate) fn pick_files(&self, title: &str, video: bool) -> Vec<PathBuf> {
        let exts: &[&str] = if video {
            &[
                "mp4", "mkv", "mov", "webm", "avi", "m4v", "ts", "mts", "flv", "wmv", "fdmv",
            ]
        } else {
            &[
                "wav", "flac", "mp3", "m4a", "aac", "ogg", "opus", "wma", "aiff", "mp4", "mkv",
                "mov", "webm",
            ]
        };
        rfd::FileDialog::new()
            .set_title(title)
            .add_filter(if video { "動画" } else { "音声・動画" }, exts)
            .pick_files()
            .unwrap_or_default()
    }

    fn add_audio_file(&mut self, chain: Id, at: f64) {
        let files = self.pick_files("チェーンに追加する音声", false);
        let ids = self.import_files(&files, false);
        let mut t = at;
        for id in ids {
            let Some(d) = self
                .project
                .source(id)
                .filter(|s| s.has_audio)
                .map(|s| s.duration)
            else {
                continue;
            };
            self.edit(None, |p| {
                let _ = p.add_audio(chain, id, t);
            });
            t += d;
        }
    }

    pub(crate) fn add_video_files(&mut self) {
        let files = self.pick_files("映像トラックに追加する動画", true);
        self.import_files(&files, true);
    }

    pub(crate) fn add_source_files(&mut self) {
        let files = self.pick_files("素材を追加", true);
        self.import_files(&files, false);
    }

    fn apply_actions(&mut self, actions: Vec<Action>) {
        for a in actions {
            match a {
                Action::Seek(t) => {
                    if self.is_playing() {
                        self.seek(t);
                    } else {
                        self.playhead = t.clamp(0.0, self.project.duration());
                        self.seek(self.playhead);
                    }
                }
                Action::Select(s) => self.selection = s,
                Action::Edit { edit, begin } => {
                    if begin {
                        self.history.record(&self.project, None);
                    }
                    self.edit_live(|p| match edit {
                        Edit::MoveAudio { chain, clip, start } => p.move_audio(chain, clip, start),
                        Edit::TrimAudio {
                            chain,
                            clip,
                            start,
                            end,
                        } => p.trim_audio(chain, clip, start, end),
                        Edit::TrimVideo {
                            clip,
                            src_in,
                            src_out,
                        } => p.trim_video(clip, src_in, src_out),
                        Edit::MoveVideo { clip, to } => p.move_video(clip, to),
                    });
                }
                Action::EndDrag => self.refresh_preview(),
                Action::Drop { source, row, t } => match row {
                    Row::Video => self.edit(None, |p| {
                        let _ = p.insert_video(source, Some(t as usize));
                    }),
                    Row::Chain(chain) => self.edit(None, |p| {
                        let _ = p.add_audio(chain, source, t);
                    }),
                },
                Action::AddAudioFile { chain, at } => self.add_audio_file(chain, at),
                Action::AddVideoFile => self.add_video_files(),
                Action::Split { sel, t } => self.split_at(sel, self.project.snap(t)),
                Action::Delete(sel) => {
                    self.selection = sel;
                    self.delete_selected();
                }
                Action::ToggleMute(id) => self.toggle_mute(id),
                Action::RemoveChain(id) => self.edit(None, |p| p.remove_chain(id)),
                Action::AddChain => self.add_chain(),
            }
        }
    }

    // ------------------------------------------------------------------
    // ファイル

    fn title(&self) -> String {
        let name = self
            .project_path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "無題".into());
        format!("{}{name} — FDMV Editor", if self.dirty { "● " } else { "" })
    }

    fn reset_state(&mut self, ctx: &egui::Context) {
        self.history.clear();
        self.selection = Selection::None;
        self.playhead = 0.0;
        self.in_point = None;
        self.out_point = None;
        self.muted.clear();
        self.texture = None;
        self.dirty = false;
        self.fit_requested = true;
        self.start_preview(ctx);
        self.ensure_proxies();
    }

    fn new_project(&mut self, ctx: &egui::Context) {
        self.project = Project::new();
        self.project_path = None;
        self.reset_state(ctx);
    }

    pub(crate) fn open_project(&mut self, ctx: &egui::Context, path: &Path) {
        match Project::load(path) {
            Ok(p) => {
                self.project = p;
                self.project_path = Some(path.to_path_buf());
                self.reset_state(ctx);
                let missing: Vec<String> = self
                    .project
                    .sources
                    .iter()
                    .filter(|s| !s.path.exists())
                    .map(|s| s.path.display().to_string())
                    .collect();
                if !missing.is_empty() {
                    self.error = Some(format!(
                        "見つからない素材があります:\n{}",
                        missing.join("\n")
                    ));
                }
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }

    pub(crate) fn save(&mut self, save_as: bool) -> bool {
        let path = match (&self.project_path, save_as) {
            (Some(p), false) => p.clone(),
            _ => {
                let Some(mut p) = rfd::FileDialog::new()
                    .set_title("プロジェクトを保存")
                    .add_filter("FDMV プロジェクト", &[PROJECT_EXT])
                    .set_file_name("project.fdmvproj")
                    .save_file()
                else {
                    return false;
                };
                if p.extension().is_none() {
                    p.set_extension(PROJECT_EXT);
                }
                p
            }
        };
        match self.project.save(&path) {
            Ok(()) => {
                self.project_path = Some(path);
                self.dirty = false;
                true
            }
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                false
            }
        }
    }

    fn request(&mut self, ctx: &egui::Context, what: Pending) {
        if self.dirty {
            self.pending = Some(what);
        } else {
            self.run_pending(ctx, what);
        }
    }

    fn run_pending(&mut self, ctx: &egui::Context, what: Pending) {
        match what {
            Pending::New => self.new_project(ctx),
            Pending::Open(path) => {
                let path = path.or_else(|| {
                    rfd::FileDialog::new()
                        .set_title("プロジェクトを開く")
                        .add_filter("FDMV プロジェクト", &[PROJECT_EXT])
                        .pick_file()
                });
                if let Some(p) = path {
                    self.open_project(ctx, &p);
                }
            }
            Pending::Quit => {
                self.allow_close = true;
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
        }
    }

    pub(crate) fn start_export(&mut self, ctx: &egui::Context) -> Result<()> {
        let Some(ff) = self.ff.clone() else {
            anyhow::bail!("ffmpeg が見つかりません")
        };
        self.project.check_exportable()?;
        let mut out = PathBuf::from(self.export_path.trim());
        if out.as_os_str().is_empty() {
            anyhow::bail!("出力先を指定してください");
        }
        if out.extension().is_none() {
            out.set_extension("fdmv");
        }
        self.export = Some(ExportJob::start(
            ff,
            self.project.clone(),
            self.proxies.clone(),
            out,
            ctx.clone(),
        ));
        Ok(())
    }

    pub(crate) fn default_export_path(&self) -> String {
        let base = self
            .project_path
            .as_ref()
            .map(|p| p.with_extension("fdmv"))
            .or_else(|| dirs_home().map(|h| h.join("output.fdmv")))
            .unwrap_or_else(|| PathBuf::from("output.fdmv"));
        base.display().to_string()
    }

    // ------------------------------------------------------------------
    // 入力

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let sc = |m: Modifiers, k: Key| {
            ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(m, k)))
        };
        if sc(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z) || sc(Modifiers::COMMAND, Key::Y) {
            self.redo();
        }
        if sc(Modifiers::COMMAND, Key::Z) {
            self.undo();
        }
        if sc(Modifiers::COMMAND | Modifiers::SHIFT, Key::S) {
            self.save(true);
        }
        if sc(Modifiers::COMMAND, Key::S) {
            self.save(false);
        }
        if sc(Modifiers::COMMAND, Key::O) {
            self.request(ctx, Pending::Open(None));
        }
        if sc(Modifiers::COMMAND, Key::N) {
            self.request(ctx, Pending::New);
        }
        if sc(Modifiers::COMMAND, Key::E) {
            self.open_export_dialog();
        }
        if sc(Modifiers::COMMAND, Key::I) {
            self.add_source_files();
        }
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let fd = self.project.frame_duration();
        let key = |k: Key| sc(Modifiers::NONE, k);
        if key(Key::Space) {
            self.toggle_play();
        }
        if key(Key::ArrowLeft) {
            self.seek(self.project.snap(self.playhead - fd));
        }
        if key(Key::ArrowRight) {
            self.seek(self.project.snap(self.playhead + fd));
        }
        if sc(Modifiers::SHIFT, Key::ArrowLeft) {
            self.seek(self.playhead - 1.0);
        }
        if sc(Modifiers::SHIFT, Key::ArrowRight) {
            self.seek(self.playhead + 1.0);
        }
        if key(Key::Home) {
            self.seek(0.0);
        }
        if key(Key::End) {
            self.seek(self.project.duration());
        }
        if key(Key::S) {
            self.split();
        }
        if key(Key::I) {
            self.in_point = Some(self.project.snap(self.playhead));
        }
        if key(Key::O) {
            self.out_point = Some(self.project.snap(self.playhead));
        }
        if sc(Modifiers::SHIFT, Key::Delete) || sc(Modifiers::SHIFT, Key::Backspace) {
            self.delete_range();
        }
        if key(Key::Delete) || key(Key::Backspace) {
            self.delete_selected();
        }
        if key(Key::Escape) {
            self.in_point = None;
            self.out_point = None;
            self.selection = Selection::None;
        }
        if key(Key::Plus) || key(Key::Equals) {
            let x = 200.0;
            let t = self.timeline.scroll + x as f64 / self.timeline.pps as f64;
            self.timeline.zoom(1.25, t, x);
        }
        if key(Key::Minus) {
            let x = 200.0;
            let t = self.timeline.scroll + x as f64 / self.timeline.pps as f64;
            self.timeline.zoom(0.8, t, x);
        }
    }

    pub(crate) fn open_export_dialog(&mut self) {
        if self.export_path.is_empty() {
            self.export_path = self.default_export_path();
        }
        self.show_export = true;
    }

    fn handle_drops(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .filter(|p| !p.as_os_str().is_empty())
                .collect()
        });
        if dropped.is_empty() {
            return;
        }
        if let Some(p) = dropped
            .iter()
            .find(|p| p.extension().is_some_and(|e| e == PROJECT_EXT))
        {
            self.request(ctx, Pending::Open(Some(p.clone())));
            return;
        }
        // ウィンドウへのドロップは素材一覧へ（映像トラックが空なら並べる）
        let to_video = self.project.video.is_empty();
        self.import_files(&dropped, to_video);
    }

    // ------------------------------------------------------------------
    // 画面

    fn menu(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("ファイル", |ui| {
                if ui.button("新規プロジェクト").clicked() {
                    ui.close();
                    self.request(&ctx, Pending::New);
                }
                if ui.button("プロジェクトを開く…").clicked() {
                    ui.close();
                    self.request(&ctx, Pending::Open(None));
                }
                if ui.button("保存").clicked() {
                    ui.close();
                    self.save(false);
                }
                if ui.button("名前を付けて保存…").clicked() {
                    ui.close();
                    self.save(true);
                }
                ui.separator();
                if ui.button("素材を追加…").clicked() {
                    ui.close();
                    self.add_source_files();
                }
                if ui.button("書き出し…").clicked() {
                    ui.close();
                    self.open_export_dialog();
                }
                ui.separator();
                if ui.button("終了").clicked() {
                    ui.close();
                    self.request(&ctx, Pending::Quit);
                }
            });
            ui.menu_button("編集", |ui| {
                if ui
                    .add_enabled(self.can_undo(), egui::Button::new("元に戻す"))
                    .clicked()
                {
                    self.undo();
                }
                if ui
                    .add_enabled(self.can_redo(), egui::Button::new("やり直す"))
                    .clicked()
                {
                    self.redo();
                }
                ui.separator();
                if ui.button("再生位置で分割 (S)").clicked() {
                    ui.close();
                    self.split();
                }
                if ui
                    .add_enabled(
                        self.selection != Selection::None,
                        egui::Button::new("選択を削除 (Delete)"),
                    )
                    .clicked()
                {
                    ui.close();
                    self.delete_selected();
                }
                if ui
                    .add_enabled(
                        self.range().is_some(),
                        egui::Button::new("イン〜アウトを削除 (Shift+Delete)"),
                    )
                    .clicked()
                {
                    ui.close();
                    self.delete_range();
                }
                ui.separator();
                if ui.button("チェーンを追加").clicked() {
                    ui.close();
                    self.add_chain();
                }
            });
            ui.menu_button("表示", |ui| {
                if ui.button("全体を表示").clicked() {
                    ui.close();
                    self.fit_requested = true;
                }
            });
        });
    }

    fn unsaved_modal(&mut self, ctx: &egui::Context) {
        let Some(what) = self.pending.clone() else {
            return;
        };
        let mut choice = None;
        egui::Modal::new(egui::Id::new("unsaved")).show(ctx, |ui| {
            ui.set_max_width(380.0);
            ui.heading("変更が保存されていません");
            ui.label("保存してから続けますか？");
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("保存する").clicked() {
                    choice = Some(1);
                }
                if ui.button("保存しない").clicked() {
                    choice = Some(2);
                }
                if ui.button("キャンセル").clicked() {
                    choice = Some(0);
                }
            });
        });
        match choice {
            Some(1) => {
                self.pending = None;
                if self.save(false) {
                    self.run_pending(ctx, what);
                }
            }
            Some(2) => {
                self.pending = None;
                self.dirty = false;
                self.run_pending(ctx, what);
            }
            Some(0) => self.pending = None,
            _ => {}
        }
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

    fn debug_tick(&mut self, ctx: &egui::Context) {
        let Some((path, frames)) = &mut self.debug_screenshot else {
            return;
        };
        *frames += 1;
        let (path, frames) = (path.clone(), *frames);
        ctx.request_repaint();
        let ready = !self.jobs.as_ref().is_some_and(|j| j.busy());
        if frames == 20 {
            // 範囲・選択・再生位置を設定した状態で撮る
            self.fit_requested = true;
            self.in_point = Some(5.5);
            self.out_point = Some(6.5);
            if let Some(ch) = self.project.chains.get(1)
                && let Some(c) = ch.clips.first()
            {
                self.selection = Selection::Audio {
                    chain: ch.id,
                    clip: c.id,
                };
            }
            self.seek(3.0);
        }
        if frames > 30 && ready && frames % 10 == 0 && self.texture.is_some() {
            ctx.send_viewport_cmd(ViewportCommand::Screenshot(Default::default()));
        }
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
            let res = std::fs::write(&path, ppm);
            println!(
                "debug: duration={:.3} clips={} chains={} playhead={:.3} audio={:?} save={res:?}",
                self.project.duration(),
                self.project.video.len(),
                self.project.chains.len(),
                self.playhead,
                self.audio_device()
            );
            self.allow_close = true;
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
    }
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // 未保存のまま閉じようとしたら確認する
        if ctx.input(|i| i.viewport().close_requested()) && !self.allow_close && self.dirty {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.pending = Some(Pending::Quit);
        }
        self.shortcuts(&ctx);
        self.handle_drops(&ctx);
        self.update_preview(&ctx);
        ctx.send_viewport_cmd(ViewportCommand::Title(self.title()));

        egui::Panel::top("menu").show(ui, |ui| self.menu(ui));
        egui::Panel::bottom("timeline")
            .resizable(true)
            .default_size(260.0)
            .min_size(140.0)
            .show(ui, |ui| {
                let width = ui.available_width() - timeline::HEADER_W;
                if self.fit_requested {
                    self.fit_requested = false;
                    self.timeline.fit(self.project.duration().max(5.0), width);
                }
                if self.is_playing() && !self.timeline.is_dragging() {
                    self.timeline.follow(self.playhead, width);
                }
                ui.add_space(2.0);
                let notes: std::collections::HashMap<Id, String> = self
                    .project
                    .sources
                    .iter()
                    .filter_map(|s| {
                        let n = match self.proxy_state(s.id)? {
                            ProxyState::Queued => "プロキシ待ち".to_owned(),
                            ProxyState::Building(p) => format!("プロキシ {:.0}%", p * 100.0),
                            ProxyState::Failed(_) => "プロキシ失敗".to_owned(),
                            ProxyState::Ready => return None,
                        };
                        Some((s.id, n))
                    })
                    .collect();
                let note = |id: Id| notes.get(&id).cloned();
                let actions = egui::ScrollArea::vertical()
                    .scroll_source(egui::scroll_area::ScrollSource {
                        mouse_wheel: false,
                        ..Default::default()
                    })
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let cx = TimelineCtx {
                            project: &self.project,
                            playhead: self.playhead,
                            in_point: self.in_point,
                            out_point: self.out_point,
                            selection: self.selection,
                            muted: &self.muted,
                            source_note: &note,
                        };
                        timeline::show(ui, &mut self.timeline, &cx)
                    })
                    .inner;
                self.apply_actions(actions);
            });
        egui::Panel::left("sources")
            .resizable(true)
            .default_size(230.0)
            .show(ui, |ui| self.sources_panel(ui));
        egui::Panel::right("properties")
            .resizable(true)
            .default_size(270.0)
            .show(ui, |ui| self.properties_panel(ui));
        egui::CentralPanel::default().show(ui, |ui| self.preview_panel(ui));

        self.export_window(&ctx);
        self.unsaved_modal(&ctx);
        self.error_modal(&ctx);
        self.debug_tick(&ctx);
    }
}

pub(crate) fn time_label(t: f64) -> String {
    format_time(t)
}

pub(crate) const ACCENT: Color32 = Color32::from_rgb(0x3d, 0x7e, 0xd6);
