//! 素材一覧・プロパティ・プレビュー・書き出しダイアログ。

use eframe::egui::{self, Align, Color32, Layout, RichText, Sense, Vec2};
use fdmv_edit::ChainRole;
use libfdmv::ffmpeg::Quality;

use crate::app::{ACCENT, App, time_label};
use crate::jobs::ProxyState;
use crate::timeline::{Selection, SourceDrag, chain_color};

impl App {
    pub(crate) fn sources_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("素材");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .button("＋ 追加")
                    .on_hover_text("素材を追加 (Ctrl+I)")
                    .clicked()
                {
                    self.add_source_files();
                }
            });
        });
        ui.label(
            RichText::new("ドラッグしてタイムラインに配置")
                .small()
                .weak(),
        );
        ui.separator();
        if self.project.sources.is_empty() {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("動画や音声ファイルを\nウィンドウにドロップ").weak());
            });
            return;
        }
        let mut action: Option<(u64, u8)> = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            for s in self.project.sources.clone() {
                let selected = self.selection == Selection::Source(s.id);
                let resp = ui
                    .dnd_drag_source(egui::Id::new(("src", s.id)), SourceDrag(s.id), |ui| {
                        let frame = egui::Frame::group(ui.style()).fill(if selected {
                            ui.visuals().selection.bg_fill
                        } else {
                            ui.visuals().faint_bg_color
                        });
                        frame.show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.label(if s.video.is_some() { "🎞" } else { "🔊" });
                                ui.add(
                                    egui::Label::new(RichText::new(s.name()).strong()).truncate(),
                                );
                            });
                            let mut info = time_label(s.duration);
                            if let Some(v) = &s.video {
                                info.push_str(&format!(
                                    "  {}x{}  {:.2}fps",
                                    v.width,
                                    v.height,
                                    v.fps_num as f64 / v.fps_den as f64
                                ));
                            }
                            if s.video.is_some() && !s.has_audio {
                                info.push_str("  音声なし");
                            }
                            ui.label(RichText::new(info).small().weak());
                            if !s.path.exists() {
                                ui.colored_label(
                                    ui.visuals().error_fg_color,
                                    "ファイルが見つかりません",
                                );
                            }
                            match self.proxy_state(s.id) {
                                Some(ProxyState::Building(p)) => {
                                    ui.add(
                                        egui::ProgressBar::new(p)
                                            .desired_height(6.0)
                                            .text(RichText::new("プロキシ作成中").small()),
                                    );
                                }
                                Some(ProxyState::Queued) => {
                                    ui.label(RichText::new("プロキシ待ち").small().weak());
                                }
                                Some(ProxyState::Failed(e)) => {
                                    ui.colored_label(
                                        ui.visuals().error_fg_color,
                                        "プロキシ作成に失敗",
                                    )
                                    .on_hover_text(e);
                                }
                                _ => {}
                            }
                        });
                    })
                    .response;
                let resp = resp.interact(Sense::click());
                if resp.clicked() {
                    self.selection = Selection::Source(s.id);
                }
                resp.context_menu(|ui| {
                    if s.video.is_some() && ui.button("映像トラックの最後に追加").clicked()
                    {
                        action = Some((s.id, 0));
                        ui.close();
                    }
                    if s.has_audio && ui.button("選択中のチェーンの再生位置に追加").clicked()
                    {
                        action = Some((s.id, 1));
                        ui.close();
                    }
                    if ui.button("素材を削除").clicked() {
                        action = Some((s.id, 2));
                        ui.close();
                    }
                });
            }
        });
        if let Some((id, what)) = action {
            match what {
                0 => self.edit(None, |p| {
                    let _ = p.insert_video(id, None);
                }),
                1 => {
                    let chain = match self.selection {
                        Selection::Chain(c) | Selection::Audio { chain: c, .. } => Some(c),
                        _ => self.project.default_chain().map(|c| c.id),
                    };
                    if let Some(chain) = chain {
                        let t = self.playhead;
                        self.edit(None, |p| {
                            let _ = p.add_audio(chain, id, t);
                        });
                    }
                }
                _ => self.edit(None, |p| p.remove_source(id)),
            }
        }
    }

    pub(crate) fn properties_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("プロパティ");
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| match self.selection {
            Selection::Video(id) => self.video_clip_props(ui, id),
            Selection::Audio { chain, clip } => self.audio_clip_props(ui, chain, clip),
            Selection::Chain(id) => self.chain_props(ui, id),
            Selection::Source(id) => self.source_props(ui, id),
            Selection::None => self.project_props(ui),
        });
    }

    fn video_clip_props(&mut self, ui: &mut egui::Ui, id: u64) {
        let Some((start, c)) = self
            .project
            .video_layout()
            .into_iter()
            .find(|(_, c)| c.id == id)
            .map(|(s, c)| (s, c.clone()))
        else {
            return;
        };
        let src = self.project.source(c.source).cloned();
        ui.label(RichText::new("映像クリップ").strong());
        if let Some(s) = &src {
            ui.label(s.name());
        }
        let max = src.as_ref().map(|s| s.duration).unwrap_or(f64::MAX);
        let fd = self.project.frame_duration();
        let (mut src_in, mut src_out) = (c.src_in, c.src_out);
        egui::Grid::new("vclip").num_columns(2).show(ui, |ui| {
            ui.label("位置");
            ui.label(time_label(start));
            ui.end_row();
            ui.label("イン");
            let a = ui.add(
                egui::DragValue::new(&mut src_in)
                    .speed(fd)
                    .range(0.0..=src_out - fd)
                    .custom_formatter(|v, _| time_label(v)),
            );
            ui.end_row();
            ui.label("アウト");
            let b = ui.add(
                egui::DragValue::new(&mut src_out)
                    .speed(fd)
                    .range(src_in + fd..=max)
                    .custom_formatter(|v, _| time_label(v)),
            );
            ui.end_row();
            ui.label("長さ");
            ui.label(time_label(c.duration()));
            ui.end_row();
            if a.changed() || b.changed() {
                self.edit(Some(&format!("trim{id}")), |p| {
                    p.trim_video(id, src_in, src_out)
                });
            }
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("再生位置で分割").clicked() {
                self.split();
            }
            if ui.button("削除").clicked() {
                self.delete_selected();
            }
        });
    }

    fn audio_clip_props(&mut self, ui: &mut egui::Ui, chain: u64, id: u64) {
        let Some(c) = self.project.audio_clip(chain, id).cloned() else {
            return;
        };
        let ch_name = self
            .project
            .chain(chain)
            .map(|c| c.name.clone())
            .unwrap_or_default();
        let src = self.project.source(c.source).cloned();
        ui.label(RichText::new("音声クリップ").strong());
        ui.label(format!("チェーン: {ch_name}"));
        if let Some(s) = &src {
            ui.label(s.name());
        }
        let max = src.as_ref().map(|s| s.duration).unwrap_or(f64::MAX);
        let (mut start, mut src_in, mut src_out, mut gain) =
            (c.start, c.src_in, c.src_out, c.gain_db);
        egui::Grid::new("aclip").num_columns(2).show(ui, |ui| {
            ui.label("位置");
            let r0 = ui.add(
                egui::DragValue::new(&mut start)
                    .speed(0.01)
                    .range(0.0..=f64::MAX)
                    .custom_formatter(|v, _| time_label(v)),
            );
            ui.end_row();
            ui.label("イン");
            let r1 = ui.add(
                egui::DragValue::new(&mut src_in)
                    .speed(0.01)
                    .range(0.0..=src_out - 0.01)
                    .custom_formatter(|v, _| time_label(v)),
            );
            ui.end_row();
            ui.label("アウト");
            let r2 = ui.add(
                egui::DragValue::new(&mut src_out)
                    .speed(0.01)
                    .range(src_in + 0.01..=max)
                    .custom_formatter(|v, _| time_label(v)),
            );
            ui.end_row();
            ui.label("音量");
            let r3 = ui.add(egui::Slider::new(&mut gain, -40.0..=12.0).suffix(" dB"));
            ui.end_row();
            if r0.changed() || r1.changed() || r2.changed() || r3.changed() {
                self.edit(Some(&format!("aclip{id}")), |p| {
                    if let Some(x) = p.audio_clip_mut(chain, id) {
                        x.src_in = src_in;
                        x.src_out = src_out;
                        x.gain_db = gain;
                    }
                    p.move_audio(chain, id, start);
                });
            }
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("再生位置で分割").clicked() {
                self.split();
            }
            if ui.button("削除").clicked() {
                self.delete_selected();
            }
        });
    }

    fn chain_props(&mut self, ui: &mut egui::Ui, id: u64) {
        let Some(idx) = self.project.chains.iter().position(|c| c.id == id) else {
            return;
        };
        let mut ch = self.project.chains[idx].clone();
        let (dot, _) = ui.allocate_exact_size(Vec2::new(12.0, 12.0), Sense::hover());
        ui.painter()
            .circle_filled(dot.center(), 5.0, chain_color(idx));
        ui.label(
            RichText::new(if ch.role == ChainRole::Default {
                "デフォルトチェーン"
            } else {
                "サブチェーン"
            })
            .strong(),
        );
        let mut changed = false;
        egui::Grid::new("chain").num_columns(2).show(ui, |ui| {
            ui.label("名前");
            changed |= ui.text_edit_singleline(&mut ch.name).changed();
            ui.end_row();
            ui.label("音量");
            changed |= ui
                .add(egui::Slider::new(&mut ch.gain_db, -40.0..=12.0).suffix(" dB"))
                .changed();
            ui.end_row();
            ui.label("言語");
            changed |= ui
                .add(egui::TextEdit::singleline(&mut ch.language).hint_text("例: ja"))
                .changed();
            ui.end_row();
            ui.label("説明");
            changed |= ui.text_edit_multiline(&mut ch.description).changed();
            ui.end_row();
        });
        if ch.role == ChainRole::Default {
            changed |= ui
                .checkbox(&mut ch.video_audio, "映像クリップの音声を含める")
                .changed();
        } else if ui.button("デフォルトチェーンにする").clicked() {
            self.edit(None, |p| p.set_default_chain(id));
        }
        ui.label(RichText::new(format!("クリップ数: {}", ch.clips.len())).weak());
        let muted = self.muted.contains(&id);
        if ui
            .selectable_label(muted, "プレビューで鳴らさない")
            .clicked()
        {
            self.toggle_mute(id);
        }
        if changed {
            self.edit(Some(&format!("chain{id}")), |p| {
                if let Some(c) = p.chain_mut(id) {
                    c.name = ch.name.clone();
                    c.gain_db = ch.gain_db;
                    c.language = ch.language.clone();
                    c.description = ch.description.clone();
                    c.video_audio = ch.video_audio;
                }
            });
        }
        ui.add_space(8.0);
        if ui.button("音声ファイルを再生位置に追加…").clicked() {
            let files = self.pick_files("チェーンに追加する音声", false);
            let ids = self.import_files(&files, false);
            let mut t = self.playhead;
            for sid in ids {
                let Some(d) = self
                    .project
                    .source(sid)
                    .filter(|s| s.has_audio)
                    .map(|s| s.duration)
                else {
                    continue;
                };
                self.edit(None, |p| {
                    let _ = p.add_audio(id, sid, t);
                });
                t += d;
            }
        }
        if ui.button("チェーンを削除").clicked() {
            self.edit(None, |p| p.remove_chain(id));
        }
    }

    fn source_props(&mut self, ui: &mut egui::Ui, id: u64) {
        let Some(s) = self.project.source(id).cloned() else {
            return;
        };
        ui.label(RichText::new("素材").strong());
        ui.label(s.name());
        ui.label(RichText::new(s.path.display().to_string()).small().weak());
        ui.label(format!("長さ: {}", time_label(s.duration)));
        if let Some(v) = &s.video {
            ui.label(format!(
                "映像: {}x{}, {:.3} fps",
                v.width,
                v.height,
                v.fps_num as f64 / v.fps_den as f64
            ));
        }
        ui.label(format!(
            "音声: {}",
            if s.has_audio { "あり" } else { "なし" }
        ));
        ui.add_space(8.0);
        if s.video.is_some() && ui.button("映像トラックの最後に追加").clicked() {
            self.edit(None, |p| {
                let _ = p.insert_video(id, None);
            });
        }
        if ui
            .button("素材を削除（使っているクリップも削除）")
            .clicked()
        {
            self.edit(None, |p| p.remove_source(id));
        }
    }

    fn project_props(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("書き出し設定").strong());
        let mut s = self.project.settings.clone();
        let (w, h, fn_, fd) = self.project.format();
        let mut changed = false;
        egui::Grid::new("settings").num_columns(2).show(ui, |ui| {
            ui.label("タイトル");
            changed |= ui.text_edit_singleline(&mut s.title).changed();
            ui.end_row();
            ui.label("形式");
            changed |= ui
                .checkbox(&mut s.auto_format, "最初のクリップに合わせる")
                .changed();
            ui.end_row();
            if s.auto_format {
                ui.label("");
                ui.label(
                    RichText::new(format!("{w}x{h}, {:.3} fps", fn_ as f64 / fd as f64)).weak(),
                );
                ui.end_row();
            } else {
                ui.label("幅");
                changed |= ui
                    .add(egui::DragValue::new(&mut s.width).range(16..=7680))
                    .changed();
                ui.end_row();
                ui.label("高さ");
                changed |= ui
                    .add(egui::DragValue::new(&mut s.height).range(16..=4320))
                    .changed();
                ui.end_row();
                ui.label("fps");
                let mut fps = s.fps_num as f64 / s.fps_den.max(1) as f64;
                egui::ComboBox::from_id_salt("fps")
                    .selected_text(format!("{fps:.3}"))
                    .show_ui(ui, |ui| {
                        for (n, d) in [
                            (24000, 1001),
                            (24, 1),
                            (25, 1),
                            (30000, 1001),
                            (30, 1),
                            (50, 1),
                            (60000, 1001),
                            (60, 1),
                        ] {
                            if ui
                                .selectable_label(
                                    s.fps_num == n && s.fps_den == d,
                                    format!("{:.3}", n as f64 / d as f64),
                                )
                                .clicked()
                            {
                                s.fps_num = n;
                                s.fps_den = d;
                                fps = n as f64 / d as f64;
                                changed = true;
                            }
                        }
                    });
                let _ = fps;
                ui.end_row();
            }
            ui.label("画質");
            let current = Quality::from_crf_preset(s.crf, s.preset);
            egui::ComboBox::from_id_salt("quality")
                .selected_text(current.map(quality_label).unwrap_or("カスタム"))
                .show_ui(ui, |ui| {
                    for q in Quality::ALL {
                        if ui
                            .selectable_label(current == Some(q), quality_label(q))
                            .clicked()
                        {
                            let (crf, preset) = q.crf_preset();
                            s.crf = crf;
                            s.preset = Some(preset);
                            changed = true;
                        }
                    }
                });
            ui.end_row();
            ui.label("");
            ui.label(RichText::new(quality_note(current)).small().weak());
            ui.end_row();
            ui.label("CRF");
            changed |= ui
                .add(egui::Slider::new(&mut s.crf, 10..=50))
                .on_hover_text("小さいほど高画質・大容量（18 でほぼ無劣化、23 が高画質）")
                .changed();
            ui.end_row();
            ui.label("速度");
            let mut preset = s.preset.unwrap_or(6) as i32;
            if ui
                .add(egui::Slider::new(&mut preset, 0..=13))
                .on_hover_text(
                    "SVT-AV1 のプリセット。小さいほど遅いが、同じ画質でファイルが小さくなる",
                )
                .changed()
            {
                s.preset = Some(preset as u32);
                changed = true;
            }
            ui.end_row();
            ui.label("10 bit");
            changed |= ui.checkbox(&mut s.ten_bit, "").changed();
            ui.end_row();
            ui.label("音声");
            egui::ComboBox::from_id_salt("abr")
                .selected_text(&s.audio_bitrate)
                .show_ui(ui, |ui| {
                    for b in ["64k", "96k", "128k", "160k", "192k", "256k"] {
                        if ui.selectable_label(s.audio_bitrate == b, b).clicked() {
                            s.audio_bitrate = b.into();
                            changed = true;
                        }
                    }
                });
            ui.end_row();
        });
        if changed {
            self.edit(Some("settings"), |p| p.settings = s);
        }
        ui.add_space(12.0);
        ui.label(RichText::new("操作").strong());
        for (k, v) in [
            ("Space", "再生 / 一時停止"),
            ("← / →", "1 フレーム移動（Shift で 1 秒）"),
            ("S", "再生位置で分割"),
            ("Delete", "選択を削除"),
            ("I / O", "イン点 / アウト点"),
            ("Shift+Delete", "イン〜アウトを削除"),
            ("Ctrl+Z / Ctrl+Y", "元に戻す / やり直す"),
            ("Ctrl+ホイール", "タイムラインの拡大縮小"),
        ] {
            ui.horizontal(|ui| {
                ui.label(RichText::new(k).monospace().small());
                ui.label(RichText::new(v).small().weak());
            });
        }
    }

    pub(crate) fn preview_panel(&mut self, ui: &mut egui::Ui) {
        let controls_h = 64.0;
        let avail = ui.available_size();
        let (rect, resp) = ui.allocate_exact_size(
            Vec2::new(avail.x, (avail.y - controls_h).max(50.0)),
            Sense::click(),
        );
        ui.painter().rect_filled(rect, 0.0, Color32::BLACK);
        let (w, h, _, _) = self.project.format();
        if self.project.video.is_empty() {
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "素材をドロップして編集を始めましょう",
                egui::FontId::proportional(16.0),
                Color32::GRAY,
            );
        } else if let Some((tex, size)) = self.preview_texture() {
            // 書き出し時の画面比率の枠に、素材をそのアスペクト比で収める
            let out_aspect = w as f32 / h.max(1) as f32;
            let frame = fit(rect.size(), out_aspect);
            let frame_rect = egui::Rect::from_center_size(rect.center(), frame);
            let inner = fit(frame_rect.size(), size.x / size.y.max(1.0));
            let r = egui::Rect::from_center_size(rect.center(), inner);
            ui.painter().image(
                tex.id(),
                r,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );
            ui.painter().rect_stroke(
                frame_rect,
                0.0,
                egui::Stroke::new(1.0, Color32::from_gray(60)),
                egui::StrokeKind::Outside,
            );
        }
        if resp.clicked() {
            self.toggle_play();
        }

        ui.horizontal(|ui| {
            let fd = self.project.frame_duration();
            if ui.button("⏮").on_hover_text("先頭 (Home)").clicked() {
                self.seek(0.0);
            }
            if ui.button("◀").on_hover_text("1 フレーム戻る (←)").clicked() {
                self.seek(self.project.snap(self.playhead - fd));
            }
            let icon = if self.is_playing() { "⏸" } else { "▶" };
            if ui
                .add(
                    egui::Button::new(RichText::new(icon).size(16.0))
                        .min_size(Vec2::new(40.0, 24.0)),
                )
                .clicked()
            {
                self.toggle_play();
            }
            if ui
                .button("▶|")
                .on_hover_text("1 フレーム進む (→)")
                .clicked()
            {
                self.seek(self.project.snap(self.playhead + fd));
            }
            ui.label(
                RichText::new(format!(
                    "{} / {}",
                    time_label(self.playhead),
                    time_label(self.project.duration())
                ))
                .monospace(),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let mut v = self.master_volume;
                if ui
                    .add(egui::Slider::new(&mut v, 0.0..=1.5).show_value(false))
                    .on_hover_text("プレビューの音量")
                    .changed()
                {
                    self.set_master_volume(v);
                }
                ui.label("🔊");
            });
        });
        ui.horizontal(|ui| {
            if ui
                .button("✂ 分割")
                .on_hover_text("再生位置で分割 (S)")
                .clicked()
            {
                self.split();
            }
            if ui
                .button("イン")
                .on_hover_text("イン点を設定 (I)")
                .clicked()
            {
                self.in_point = Some(self.project.snap(self.playhead));
            }
            if ui
                .button("アウト")
                .on_hover_text("アウト点を設定 (O)")
                .clicked()
            {
                self.out_point = Some(self.project.snap(self.playhead));
            }
            let range = self.range();
            if ui
                .add_enabled(range.is_some(), egui::Button::new("範囲を削除"))
                .on_hover_text("イン〜アウトをすべてのトラックから削除して詰める (Shift+Delete)")
                .clicked()
            {
                self.delete_range();
            }
            if let Some((a, b)) = range {
                ui.label(
                    RichText::new(format!("{}–{}", time_label(a), time_label(b)))
                        .small()
                        .color(Color32::from_rgb(255, 200, 60)),
                );
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new(RichText::new("書き出し…").color(Color32::WHITE))
                            .fill(ACCENT),
                    )
                    .on_hover_text("Ctrl+E")
                    .clicked()
                {
                    self.open_export_dialog();
                }
            });
        });
    }

    pub(crate) fn export_window(&mut self, ctx: &egui::Context) {
        if !self.show_export {
            return;
        }
        let mut open = true;
        let mut start = false;
        let mut cancel = false;
        let mut close = false;
        egui::Window::new("書き出し")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(460.0)
            .show(ctx, |ui| {
                let running = self
                    .export
                    .as_ref()
                    .is_some_and(|j| j.status.lock().unwrap().done.is_none());
                ui.add_enabled_ui(!running, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("出力先");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.export_path).desired_width(300.0),
                        );
                        if ui.button("参照…").clicked()
                            && let Some(p) = rfd::FileDialog::new()
                                .set_title("書き出し先")
                                .add_filter("FDMV 動画", &["fdmv"])
                                .set_file_name("output.fdmv")
                                .save_file()
                        {
                            self.export_path = p.display().to_string();
                        }
                    });
                });
                let (w, h, n, d) = self.project.format();
                let chains: Vec<String> = self
                    .project
                    .chains
                    .iter()
                    .map(|c| {
                        format!(
                            "{}{}",
                            c.name,
                            if c.role == ChainRole::Default {
                                "（デフォルト）"
                            } else {
                                ""
                            }
                        )
                    })
                    .collect();
                ui.label(
                    RichText::new(format!(
                        "{}  {w}x{h}  {:.3} fps  {}（CRF {}）\nチェーン: {}",
                        time_label(self.project.duration()),
                        n as f64 / d as f64,
                        Quality::from_crf_preset(
                            self.project.settings.crf,
                            self.project.settings.preset
                        )
                        .map(quality_label)
                        .unwrap_or("カスタム"),
                        self.project.settings.crf,
                        chains.join("、")
                    ))
                    .small()
                    .weak(),
                );
                ui.separator();
                if let Some(job) = &self.export {
                    let st = job.status.lock().unwrap();
                    match &st.done {
                        None => {
                            let (stage, frac) = st
                                .progress
                                .as_ref()
                                .map(|p| (p.stage.clone(), p.fraction as f32))
                                .unwrap_or(("準備中".into(), 0.0));
                            ui.label(stage);
                            ui.add(egui::ProgressBar::new(frac).show_percentage().animate(true));
                            if ui.button("キャンセル").clicked() {
                                cancel = true;
                            }
                        }
                        Some(Ok(size)) => {
                            ui.colored_label(
                                Color32::from_rgb(0x59, 0xc1, 0x6b),
                                "書き出しが完了しました",
                            );
                            ui.label(format!(
                                "{}（{:.1} MiB）",
                                job.output.display(),
                                *size as f64 / 1048576.0
                            ));
                            if ui.button("閉じる").clicked() {
                                close = true;
                            }
                        }
                        Some(Err(e)) => {
                            ui.colored_label(ui.visuals().error_fg_color, "書き出しに失敗しました");
                            ui.label(RichText::new(e).small());
                            if ui.button("もう一度").clicked() {
                                start = true;
                            }
                        }
                    }
                } else if ui
                    .add(
                        egui::Button::new(RichText::new("書き出し開始").color(Color32::WHITE))
                            .fill(ACCENT),
                    )
                    .clicked()
                {
                    start = true;
                }
            });
        if cancel && let Some(j) = &self.export {
            j.cancel();
        }
        if start && let Err(e) = self.start_export(ctx) {
            self.error = Some(format!("{e:#}"));
        }
        if close || !open {
            let running = self
                .export
                .as_ref()
                .is_some_and(|j| j.status.lock().unwrap().done.is_none());
            if running && !open {
                // 実行中に閉じたらキャンセルする
                if let Some(j) = &self.export {
                    j.cancel();
                }
            }
            if !running {
                self.export = None;
            }
            self.show_export = running;
        }
    }
}

fn quality_label(q: Quality) -> &'static str {
    match q {
        Quality::Best => "最高画質（ほぼ無劣化）",
        Quality::High => "高画質",
        Quality::Standard => "標準",
        Quality::Small => "小容量",
    }
}

fn quality_note(q: Option<Quality>) -> &'static str {
    match q {
        Some(Quality::Best) => "元の映像と見分けがつかない画質。ファイルは大きめ",
        Some(Quality::High) => "細かい模様でもほとんど劣化が分からない画質（おすすめ）",
        Some(Quality::Standard) => "ファイルサイズとのバランス重視。細部はやや甘くなる",
        Some(Quality::Small) => "共有向けの小さいファイル。劣化が見える場合がある",
        None => "CRF と速度を個別に指定しています",
    }
}

fn fit(area: Vec2, aspect: f32) -> Vec2 {
    if area.x / area.y.max(1.0) > aspect {
        Vec2::new(area.y * aspect, area.y)
    } else {
        Vec2::new(area.x, area.x / aspect.max(0.01))
    }
}
