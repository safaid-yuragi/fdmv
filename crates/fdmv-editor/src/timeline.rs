//! タイムライン（映像トラックとチェーンのトラック）の表示と操作。
//!
//! 操作の結果は [`Action`] として返し、プロジェクトの変更はアプリ側で行う（元に戻すの記録のため）。

use std::collections::HashSet;

use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, FontId, Pos2, Rect, Sense, Stroke, StrokeKind,
    Ui, UiBuilder, Vec2,
};
use fdmv_edit::{ChainRole, Id, Project};

pub const HEADER_W: f32 = 176.0;
const RULER_H: f32 = 22.0;
const VIDEO_H: f32 = 46.0;
const CHAIN_H: f32 = 34.0;
const FOOTER_H: f32 = 30.0;
const EDGE: f32 = 6.0;
const SNAP_PX: f32 = 8.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Selection {
    #[default]
    None,
    Video(Id),
    Audio {
        chain: Id,
        clip: Id,
    },
    Chain(Id),
    Source(Id),
}

/// 素材一覧からドラッグされたもの。
#[derive(Clone, Copy, Debug)]
pub struct SourceDrag(pub Id);

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Row {
    Video,
    Chain(Id),
}

#[derive(Clone, Debug)]
pub enum Edit {
    MoveAudio {
        chain: Id,
        clip: Id,
        start: f64,
    },
    TrimAudio {
        chain: Id,
        clip: Id,
        start: Option<f64>,
        end: Option<f64>,
    },
    TrimVideo {
        clip: Id,
        src_in: f64,
        src_out: f64,
    },
    MoveVideo {
        clip: Id,
        to: usize,
    },
}

#[derive(Clone, Debug)]
pub enum Action {
    Seek(f64),
    Select(Selection),
    /// `begin` が true なら、ドラッグなど一連の変更の最初（元に戻すの記録をする）。
    Edit {
        edit: Edit,
        begin: bool,
    },
    EndDrag,
    Drop {
        source: Id,
        row: Row,
        t: f64,
    },
    AddAudioFile {
        chain: Id,
        at: f64,
    },
    AddVideoFile,
    Split {
        sel: Selection,
        t: f64,
    },
    Delete(Selection),
    ToggleMute(Id),
    RemoveChain(Id),
    AddChain,
}

#[derive(Clone, Copy, Debug)]
enum DragKind {
    Playhead,
    AudioMove { chain: Id, clip: Id, orig: f64 },
    AudioTrimL { chain: Id, clip: Id, orig: f64 },
    AudioTrimR { chain: Id, clip: Id, orig: f64 },
    VideoTrimL { clip: Id, src_in: f64, src_out: f64 },
    VideoTrimR { clip: Id, src_in: f64, src_out: f64 },
    VideoMove { clip: Id },
}

#[derive(Clone, Copy, Debug)]
struct Drag {
    kind: DragKind,
    origin_t: f64,
    begun: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Zone {
    Left,
    Right,
    Body,
}

#[derive(Clone, Copy, Debug)]
enum Hit {
    Video { clip: Id, zone: Zone },
    Audio { chain: Id, clip: Id, zone: Zone },
}

pub struct TimelineState {
    /// 1 秒あたりのピクセル数。
    pub pps: f32,
    /// 左端の時刻。
    pub scroll: f64,
    drag: Option<Drag>,
    /// 右クリックした位置（コンテキストメニュー用）。
    context: Option<(Option<Hit>, Row, f64)>,
}

impl Default for TimelineState {
    fn default() -> Self {
        TimelineState {
            pps: 60.0,
            scroll: 0.0,
            drag: None,
            context: None,
        }
    }
}

impl TimelineState {
    pub fn zoom(&mut self, factor: f32, anchor_t: f64, anchor_x: f32) {
        self.pps = (self.pps * factor).clamp(2.0, 2000.0);
        self.scroll = (anchor_t - anchor_x as f64 / self.pps as f64).max(0.0);
    }

    /// 全体が収まるようにする。
    pub fn fit(&mut self, duration: f64, width: f32) {
        if duration > 0.0 && width > 50.0 {
            self.pps = ((width - 40.0) / duration as f32).clamp(2.0, 2000.0);
            self.scroll = 0.0;
        }
    }

    /// 再生位置が見えるようにスクロールする。
    pub fn follow(&mut self, t: f64, width: f32) {
        let visible = width as f64 / self.pps as f64;
        if t < self.scroll || t > self.scroll + visible * 0.95 {
            self.scroll = (t - visible * 0.1).max(0.0);
        }
    }

    pub fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }
}

pub struct TimelineCtx<'a> {
    pub project: &'a Project,
    pub playhead: f64,
    pub in_point: Option<f64>,
    pub out_point: Option<f64>,
    pub selection: Selection,
    pub muted: &'a HashSet<Id>,
    /// 素材ごとの注記（例: 「プロキシ 40%」）。
    pub source_note: &'a dyn Fn(Id) -> Option<String>,
}

pub fn chain_color(index: usize) -> Color32 {
    const PALETTE: [Color32; 8] = [
        Color32::from_rgb(0x59, 0xc1, 0x6b),
        Color32::from_rgb(0xf2, 0x8e, 0x2b),
        Color32::from_rgb(0xb0, 0x7a, 0xe0),
        Color32::from_rgb(0xe1, 0x57, 0x59),
        Color32::from_rgb(0xed, 0xc9, 0x48),
        Color32::from_rgb(0x4b, 0xc6, 0xc6),
        Color32::from_rgb(0xff, 0x9d, 0xa7),
        Color32::from_rgb(0x9c, 0xa3, 0xaf),
    ];
    PALETTE[index % PALETTE.len()]
}

const VIDEO_COLOR: Color32 = Color32::from_rgb(0x3d, 0x7e, 0xd6);

/// 目盛りの時刻表示。
fn ruler_label(t: f64, step: f64) -> String {
    let m = (t / 60.0).floor() as i64;
    let s = t - m as f64 * 60.0;
    if step < 1.0 {
        format!("{m}:{s:04.1}")
    } else {
        format!("{m}:{:02}", s.round() as i64)
    }
}

/// 表示する時間の長さ（再生位置や素材を置ける余白を含む）。
fn content_end(cx: &TimelineCtx) -> f64 {
    let mut end = cx.project.duration();
    for ch in &cx.project.chains {
        for c in &ch.clips {
            end = end.max(c.end());
        }
    }
    end.max(cx.playhead)
}

pub fn show(ui: &mut Ui, state: &mut TimelineState, cx: &TimelineCtx) -> Vec<Action> {
    let mut actions = Vec::new();
    let p = cx.project;
    let rows: Vec<Row> = std::iter::once(Row::Video)
        .chain(p.chains.iter().map(|c| Row::Chain(c.id)))
        .collect();
    let row_h = |r: &Row| if *r == Row::Video { VIDEO_H } else { CHAIN_H };
    let height = RULER_H + rows.iter().map(row_h).sum::<f32>() + FOOTER_H;
    let width = ui.available_width();
    let (full, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    let area = Rect::from_min_max(Pos2::new(full.left() + HEADER_W, full.top()), full.max);
    let tracks = Rect::from_min_max(
        Pos2::new(area.left(), area.top() + RULER_H),
        Pos2::new(area.right(), full.bottom() - FOOTER_H),
    );
    let visuals = ui.visuals().clone();
    let painter = ui.painter_at(full);

    // ------------------------------------------------------------------
    // ホイール: 横スクロール、Ctrl+ホイール: ズーム
    if ui.rect_contains_pointer(area) {
        let (scroll, zoom, hover) = ui.input(|i| {
            (
                i.smooth_scroll_delta(),
                i.zoom_delta(),
                i.pointer.hover_pos(),
            )
        });
        if zoom != 1.0
            && let Some(h) = hover
        {
            let t = state.scroll + ((h.x - area.left()) / state.pps) as f64;
            state.zoom(zoom, t, h.x - area.left());
        }
        let d = if scroll.x != 0.0 { scroll.x } else { scroll.y };
        if d != 0.0 {
            state.scroll = (state.scroll - d as f64 / state.pps as f64).max(0.0);
            ui.input_mut(|i| i.smooth_scroll_delta = Vec2::ZERO);
        }
    }

    // このフレームの表示範囲（ドラッグ中の自動スクロールは次のフレームから反映）
    let (scroll, pps) = (state.scroll, state.pps);
    let x_of = |t: f64| area.left() + ((t - scroll) * pps as f64) as f32;
    let t_of = |x: f32| scroll + ((x - area.left()) / pps) as f64;

    // 行の位置
    let mut row_rects = Vec::new();
    let mut y = tracks.top();
    for r in &rows {
        let h = row_h(r);
        row_rects.push((
            *r,
            Rect::from_min_max(Pos2::new(full.left(), y), Pos2::new(full.right(), y + h)),
        ));
        y += h;
    }
    let row_at = |pos: Pos2| {
        row_rects
            .iter()
            .find(|(_, rr)| rr.contains(pos))
            .map(|(r, rr)| (*r, *rr))
    };

    // ------------------------------------------------------------------
    // 背景と目盛り
    painter.rect_filled(area, 0.0, visuals.extreme_bg_color);
    for (i, (_, rr)) in row_rects.iter().enumerate() {
        let r = Rect::from_x_y_ranges(area.x_range(), rr.y_range());
        if i % 2 == 1 {
            painter.rect_filled(r, 0.0, visuals.faint_bg_color);
        }
        painter.hline(
            area.x_range(),
            rr.bottom(),
            Stroke::new(1.0, visuals.widgets.noninteractive.bg_stroke.color),
        );
    }
    let ruler = Rect::from_min_max(area.min, Pos2::new(area.right(), area.top() + RULER_H));
    painter.rect_filled(ruler, 0.0, visuals.panel_fill);
    let steps = [
        0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0,
    ];
    let step = steps
        .iter()
        .copied()
        .find(|s| s * state.pps as f64 >= 70.0)
        .unwrap_or(3600.0);
    let first = (scroll / step).floor() * step;
    let mut t = first;
    let right_t = t_of(area.right());
    while t <= right_t {
        let x = x_of(t);
        if x >= area.left() {
            painter.vline(
                x,
                ruler.y_range(),
                Stroke::new(1.0, visuals.weak_text_color()),
            );
            painter.text(
                Pos2::new(x + 3.0, ruler.top() + 2.0),
                Align2::LEFT_TOP,
                ruler_label(t, step),
                FontId::monospace(11.0),
                visuals.text_color(),
            );
        }
        t += step;
    }

    // ------------------------------------------------------------------
    // クリップ
    struct ClipBox {
        rect: Rect,
        hit: Option<Hit>,
    }
    let mut boxes: Vec<ClipBox> = Vec::new();
    let tracks_painter = painter.with_clip_rect(tracks);
    let video_row = row_rects[0].1;
    let dragging_video = match state.drag {
        Some(Drag {
            kind: DragKind::VideoMove { clip },
            ..
        }) => Some(clip),
        _ => None,
    };
    for (start, c) in p.video_layout() {
        let r = Rect::from_x_y_ranges(
            x_of(start)..=x_of(start + c.duration()),
            video_row.top() + 3.0..=video_row.bottom() - 3.0,
        );
        let name = p.source(c.source).map(|s| s.name()).unwrap_or_default();
        let note = (cx.source_note)(c.source);
        let selected = cx.selection == Selection::Video(c.id);
        let fill = if dragging_video == Some(c.id) {
            VIDEO_COLOR.gamma_multiply(0.4)
        } else {
            VIDEO_COLOR
        };
        draw_clip(&tracks_painter, r, fill, selected, &name, note.as_deref());
        boxes.push(ClipBox {
            rect: r,
            hit: Some(Hit::Video {
                clip: c.id,
                zone: Zone::Body,
            }),
        });
    }
    for (ci, ch) in p.chains.iter().enumerate() {
        let rr = row_rects[ci + 1].1;
        let color = chain_color(ci);
        let muted = cx.muted.contains(&ch.id);
        let color = if muted {
            color.gamma_multiply(0.35)
        } else {
            color
        };
        if ch.role == ChainRole::Default && ch.video_audio {
            // 映像クリップの音声（映像に連動。直接は操作できない）
            for (start, c) in p.video_layout() {
                if !p.source(c.source).is_some_and(|s| s.has_audio) {
                    continue;
                }
                let r = Rect::from_x_y_ranges(
                    x_of(start)..=x_of(start + c.duration()),
                    rr.top() + 5.0..=rr.bottom() - 5.0,
                );
                tracks_painter.rect_filled(
                    r.shrink(0.5),
                    CornerRadius::same(3),
                    color.gamma_multiply(0.25),
                );
                tracks_painter.text(
                    r.left_center() + Vec2::new(4.0, 0.0),
                    Align2::LEFT_CENTER,
                    "映像の音声",
                    FontId::proportional(11.0),
                    visuals.weak_text_color(),
                );
                boxes.push(ClipBox { rect: r, hit: None });
            }
        }
        for c in &ch.clips {
            let r = Rect::from_x_y_ranges(
                x_of(c.start)..=x_of(c.end()),
                rr.top() + 3.0..=rr.bottom() - 3.0,
            );
            let name = p.source(c.source).map(|s| s.name()).unwrap_or_default();
            let note = (cx.source_note)(c.source);
            let selected = cx.selection
                == (Selection::Audio {
                    chain: ch.id,
                    clip: c.id,
                });
            draw_clip(&tracks_painter, r, color, selected, &name, note.as_deref());
            boxes.push(ClipBox {
                rect: r,
                hit: Some(Hit::Audio {
                    chain: ch.id,
                    clip: c.id,
                    zone: Zone::Body,
                }),
            });
        }
    }

    let hit_test = |pos: Pos2| -> Option<Hit> {
        if !tracks.contains(pos) {
            return None;
        }
        for b in boxes.iter().rev() {
            let Some(h) = b.hit else { continue };
            if !b.rect.contains(pos) {
                continue;
            }
            let zone = if b.rect.width() > EDGE * 3.0 && pos.x - b.rect.left() < EDGE {
                Zone::Left
            } else if b.rect.width() > EDGE * 3.0 && b.rect.right() - pos.x < EDGE {
                Zone::Right
            } else {
                Zone::Body
            };
            return Some(match h {
                Hit::Video { clip, .. } => Hit::Video { clip, zone },
                Hit::Audio { chain, clip, .. } => Hit::Audio { chain, clip, zone },
            });
        }
        None
    };

    // 吸着先: 0、再生位置、クリップの端
    let mut snaps: Vec<f64> = vec![0.0, cx.playhead];
    for (s, c) in p.video_layout() {
        snaps.push(s);
        snaps.push(s + c.duration());
    }
    for ch in &p.chains {
        for c in &ch.clips {
            snaps.push(c.start);
            snaps.push(c.end());
        }
    }
    let snap = |t: f64, exclude: Option<(f64, f64)>| -> f64 {
        let mut best = (SNAP_PX as f64 / pps as f64, None);
        for &s in &snaps {
            if exclude.is_some_and(|(a, b)| (s - a).abs() < 1e-9 || (s - b).abs() < 1e-9) {
                continue;
            }
            let d = (s - t).abs();
            if d < best.0 {
                best = (d, Some(s));
            }
        }
        best.1.unwrap_or_else(|| p.snap(t))
    };

    // ------------------------------------------------------------------
    // 目盛り上の操作: 押している間シーク
    let ruler_resp = ui.interact(ruler, ui.id().with("ruler"), Sense::click_and_drag());
    if ruler_resp.is_pointer_button_down_on()
        && let Some(pos) = ui.ctx().pointer_interact_pos()
    {
        actions.push(Action::Seek(p.snap(t_of(pos.x).max(0.0))));
    }

    // トラック上の操作
    let resp = ui.interact(tracks, ui.id().with("tracks"), Sense::click_and_drag());
    let hover = resp.hover_pos();
    if let Some(h) = hover.and_then(hit_test) {
        let icon = match h {
            Hit::Video {
                zone: Zone::Body, ..
            } => CursorIcon::Grab,
            Hit::Audio {
                zone: Zone::Body, ..
            } => CursorIcon::Grab,
            _ => CursorIcon::ResizeHorizontal,
        };
        ui.ctx().set_cursor_icon(icon);
    }

    if resp.drag_started()
        && let Some(origin) = ui.input(|i| i.pointer.press_origin())
    {
        let origin_t = t_of(origin.x);
        let kind = match hit_test(origin) {
            Some(Hit::Audio { chain, clip, zone }) => {
                actions.push(Action::Select(Selection::Audio { chain, clip }));
                let c = p.audio_clip(chain, clip).unwrap();
                Some(match zone {
                    Zone::Body => DragKind::AudioMove {
                        chain,
                        clip,
                        orig: c.start,
                    },
                    Zone::Left => DragKind::AudioTrimL {
                        chain,
                        clip,
                        orig: c.start,
                    },
                    Zone::Right => DragKind::AudioTrimR {
                        chain,
                        clip,
                        orig: c.end(),
                    },
                })
            }
            Some(Hit::Video { clip, zone }) => {
                actions.push(Action::Select(Selection::Video(clip)));
                let c = p.video.iter().find(|c| c.id == clip).unwrap();
                Some(match zone {
                    Zone::Body => DragKind::VideoMove { clip },
                    Zone::Left => DragKind::VideoTrimL {
                        clip,
                        src_in: c.src_in,
                        src_out: c.src_out,
                    },
                    Zone::Right => DragKind::VideoTrimR {
                        clip,
                        src_in: c.src_in,
                        src_out: c.src_out,
                    },
                })
            }
            None => Some(DragKind::Playhead),
        };
        state.drag = kind.map(|kind| Drag {
            kind,
            origin_t,
            begun: false,
        });
    }

    if let Some(drag) = &mut state.drag
        && let Some(pos) = ui.ctx().pointer_interact_pos()
        && resp.dragged()
    {
        let t = t_of(pos.x);
        let dt = t - drag.origin_t;
        let begin = !drag.begun;
        let edit = match drag.kind {
            DragKind::Playhead => {
                actions.push(Action::Seek(p.snap(t.max(0.0))));
                None
            }
            DragKind::AudioMove { chain, clip, orig } => {
                let c = p.audio_clip(chain, clip).unwrap();
                let dur = c.duration();
                let raw = (orig + dt).max(0.0);
                let st_snap = snap(raw, Some((c.start, c.end())));
                let end_snap = snap(raw + dur, Some((c.start, c.end())));
                let start = if (st_snap - raw).abs() <= (end_snap - dur - raw).abs() {
                    st_snap
                } else {
                    end_snap - dur
                };
                Some(Edit::MoveAudio {
                    chain,
                    clip,
                    start: start.max(0.0),
                })
            }
            DragKind::AudioTrimL { chain, clip, orig } => {
                let c = p.audio_clip(chain, clip).unwrap();
                Some(Edit::TrimAudio {
                    chain,
                    clip,
                    start: Some(snap(orig + dt, Some((c.start, c.start)))),
                    end: None,
                })
            }
            DragKind::AudioTrimR { chain, clip, orig } => {
                let c = p.audio_clip(chain, clip).unwrap();
                Some(Edit::TrimAudio {
                    chain,
                    clip,
                    start: None,
                    end: Some(snap(orig + dt, Some((c.end(), c.end())))),
                })
            }
            DragKind::VideoTrimL {
                clip,
                src_in,
                src_out,
            } => {
                let fd = p.frame_duration();
                let d = (dt / fd).round() * fd;
                Some(Edit::TrimVideo {
                    clip,
                    src_in: src_in + d,
                    src_out,
                })
            }
            DragKind::VideoTrimR {
                clip,
                src_in,
                src_out,
            } => {
                let fd = p.frame_duration();
                let d = (dt / fd).round() * fd;
                Some(Edit::TrimVideo {
                    clip,
                    src_in,
                    src_out: src_out + d,
                })
            }
            DragKind::VideoMove { .. } => None,
        };
        if let Some(edit) = edit {
            actions.push(Action::Edit { edit, begin });
            drag.begun = true;
        }
        // 端に近づいたら自動でスクロール
        if pos.x > area.right() - 20.0 {
            state.scroll += 4.0 / pps as f64;
        } else if pos.x < area.left() + 20.0 {
            state.scroll = (state.scroll - 4.0 / pps as f64).max(0.0);
        }
    }

    // 映像クリップの並べ替え: 挿入位置を表示
    let insert_index = |x: f32, exclude: Option<Id>| -> usize {
        let t = t_of(x);
        p.video_layout()
            .iter()
            .filter(|(_, c)| Some(c.id) != exclude)
            .filter(|(s, c)| s + c.duration() / 2.0 < t)
            .count()
    };
    let insert_x = |index: usize, exclude: Option<Id>| -> f32 {
        let mut t = 0.0;
        for (i, c) in p.video.iter().filter(|c| Some(c.id) != exclude).enumerate() {
            if i == index {
                break;
            }
            t += c.duration();
        }
        x_of(t)
    };
    if let (Some(clip), Some(pos)) = (dragging_video, ui.ctx().pointer_interact_pos()) {
        let idx = insert_index(pos.x, Some(clip));
        let x = insert_x(idx, Some(clip));
        tracks_painter.vline(
            x,
            video_row.y_range(),
            Stroke::new(3.0, visuals.selection.stroke.color),
        );
    }

    if resp.drag_stopped()
        && let Some(drag) = state.drag.take()
    {
        if let DragKind::VideoMove { clip } = drag.kind
            && let Some(pos) = ui.ctx().pointer_interact_pos()
        {
            let to = insert_index(pos.x, Some(clip));
            // move_video は「取り除く前」の番号で受け取る
            let from = p.video.iter().position(|c| c.id == clip).unwrap_or(0);
            let to = if to >= from { to + 1 } else { to };
            actions.push(Action::Edit {
                edit: Edit::MoveVideo { clip, to },
                begin: true,
            });
        }
        actions.push(Action::EndDrag);
    }

    if resp.clicked()
        && let Some(pos) = resp.interact_pointer_pos()
    {
        match hit_test(pos) {
            Some(Hit::Video { clip, .. }) => actions.push(Action::Select(Selection::Video(clip))),
            Some(Hit::Audio { chain, clip, .. }) => {
                actions.push(Action::Select(Selection::Audio { chain, clip }))
            }
            None => {
                actions.push(Action::Seek(p.snap(t_of(pos.x).max(0.0))));
                let sel = match row_at(pos) {
                    Some((Row::Chain(id), _)) => Selection::Chain(id),
                    _ => Selection::None,
                };
                actions.push(Action::Select(sel));
            }
        }
    }

    // 右クリックメニュー
    if resp.secondary_clicked()
        && let Some(pos) = resp.interact_pointer_pos()
        && let Some((row, _)) = row_at(pos)
    {
        let hit = hit_test(pos);
        match hit {
            Some(Hit::Video { clip, .. }) => actions.push(Action::Select(Selection::Video(clip))),
            Some(Hit::Audio { chain, clip, .. }) => {
                actions.push(Action::Select(Selection::Audio { chain, clip }))
            }
            None => {}
        }
        state.context = Some((hit, row, p.snap(t_of(pos.x).max(0.0))));
    }
    resp.context_menu(|ui| {
        let Some((hit, row, t)) = state.context else {
            return;
        };
        match hit {
            Some(Hit::Video { clip, .. }) => {
                if ui.button("再生位置で分割").clicked() {
                    actions.push(Action::Split {
                        sel: Selection::Video(clip),
                        t: cx.playhead,
                    });
                    ui.close();
                }
                if ui.button("削除（後ろを詰める）").clicked() {
                    actions.push(Action::Delete(Selection::Video(clip)));
                    ui.close();
                }
            }
            Some(Hit::Audio { chain, clip, .. }) => {
                if ui.button("再生位置で分割").clicked() {
                    actions.push(Action::Split {
                        sel: Selection::Audio { chain, clip },
                        t: cx.playhead,
                    });
                    ui.close();
                }
                if ui.button("削除").clicked() {
                    actions.push(Action::Delete(Selection::Audio { chain, clip }));
                    ui.close();
                }
            }
            None => match row {
                Row::Video => {
                    if ui.button("動画ファイルを追加…").clicked() {
                        actions.push(Action::AddVideoFile);
                        ui.close();
                    }
                }
                Row::Chain(chain) => {
                    if ui.button("ここに音声ファイルを追加…").clicked() {
                        actions.push(Action::AddAudioFile { chain, at: t });
                        ui.close();
                    }
                }
            },
        }
    });

    // 素材一覧からのドロップ
    if let Some(payload) = resp.dnd_hover_payload::<SourceDrag>()
        && let Some(pos) = resp.hover_pos()
        && let Some((row, rr)) = row_at(pos)
    {
        let src = p.source(payload.0);
        let ok = match row {
            Row::Video => src.is_some_and(|s| s.video.is_some()),
            Row::Chain(_) => src.is_some_and(|s| s.has_audio),
        };
        let color = if ok {
            visuals.selection.stroke.color
        } else {
            visuals.error_fg_color
        };
        let x = match row {
            Row::Video => insert_x(insert_index(pos.x, None), None),
            Row::Chain(_) => x_of(snap(t_of(pos.x), None)),
        };
        tracks_painter.vline(x, rr.y_range(), Stroke::new(3.0, color));
        if !ok {
            ui.ctx().set_cursor_icon(CursorIcon::NotAllowed);
        }
    }
    if let Some(payload) = resp.dnd_release_payload::<SourceDrag>()
        && let Some(pos) = resp.hover_pos()
        && let Some((row, _)) = row_at(pos)
    {
        let t = match row {
            Row::Video => insert_index(pos.x, None) as f64,
            Row::Chain(_) => snap(t_of(pos.x), None).max(0.0),
        };
        actions.push(Action::Drop {
            source: payload.0,
            row,
            t,
        });
    }

    // ------------------------------------------------------------------
    // イン／アウト範囲と再生位置
    if let Some(a) = cx.in_point {
        let b = cx.out_point.unwrap_or(a);
        let r = Rect::from_x_y_ranges(
            x_of(a.min(b))..=x_of(a.max(b)).max(x_of(a.min(b)) + 1.0),
            area.y_range(),
        );
        painter.with_clip_rect(area).rect_filled(
            r,
            0.0,
            Color32::from_rgba_unmultiplied(255, 220, 80, 36),
        );
        for x in [x_of(a), x_of(b)] {
            painter.with_clip_rect(area).vline(
                x,
                area.y_range(),
                Stroke::new(1.0, Color32::from_rgb(255, 200, 60)),
            );
        }
    }
    let px = x_of(cx.playhead);
    if px >= area.left() && px <= area.right() {
        let red = Color32::from_rgb(0xff, 0x4d, 0x4d);
        painter.vline(
            px,
            Rect::from_x_y_ranges(area.x_range(), area.top()..=tracks.bottom()).y_range(),
            Stroke::new(1.5, red),
        );
        painter.add(egui::Shape::convex_polygon(
            vec![
                Pos2::new(px - 6.0, area.top()),
                Pos2::new(px + 6.0, area.top()),
                Pos2::new(px, area.top() + 8.0),
            ],
            red,
            Stroke::NONE,
        ));
    }

    // 終端
    let end_x = x_of(p.duration());
    if end_x >= area.left() && end_x <= area.right() {
        painter.with_clip_rect(tracks).vline(
            end_x,
            tracks.y_range(),
            Stroke::new(1.0, visuals.weak_text_color()),
        );
    }

    // ------------------------------------------------------------------
    // トラックの見出し
    let header_bg = Rect::from_min_max(full.min, Pos2::new(full.left() + HEADER_W, full.bottom()));
    painter.rect_filled(header_bg, 0.0, visuals.panel_fill);
    for (i, (row, rr)) in row_rects.iter().enumerate() {
        let r = Rect::from_min_max(rr.min, Pos2::new(rr.left() + HEADER_W - 4.0, rr.bottom()))
            .shrink2(Vec2::new(6.0, 2.0));
        let mut child = ui.new_child(
            UiBuilder::new()
                .max_rect(r)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        match row {
            Row::Video => {
                child.label(egui::RichText::new("🎞 映像").strong());
                if child
                    .small_button("＋")
                    .on_hover_text("動画ファイルを追加")
                    .clicked()
                {
                    actions.push(Action::AddVideoFile);
                }
            }
            Row::Chain(id) => {
                let ch = p.chain(*id).unwrap();
                let (dot, _) = child.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                child
                    .painter()
                    .circle_filled(dot.center(), 4.5, chain_color(i - 1));
                let mut name = egui::RichText::new(&ch.name);
                if ch.role == ChainRole::Default {
                    name = name.strong();
                }
                let selected = cx.selection == Selection::Chain(*id);
                let label = child.add(egui::Button::selectable(selected, name).truncate());
                if label.clicked() {
                    actions.push(Action::Select(Selection::Chain(*id)));
                }
                label.on_hover_text(if ch.role == ChainRole::Default {
                    "デフォルトチェーン"
                } else {
                    "サブチェーン"
                });
                child.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button("✖")
                        .on_hover_text("チェーンを削除")
                        .clicked()
                    {
                        actions.push(Action::RemoveChain(*id));
                    }
                    if ui
                        .small_button("＋")
                        .on_hover_text("再生位置に音声ファイルを追加")
                        .clicked()
                    {
                        actions.push(Action::AddAudioFile {
                            chain: *id,
                            at: cx.playhead,
                        });
                    }
                    let muted = cx.muted.contains(id);
                    if ui
                        .selectable_label(muted, if muted { "🔇" } else { "🔊" })
                        .on_hover_text("プレビューで鳴らさない")
                        .clicked()
                    {
                        actions.push(Action::ToggleMute(*id));
                    }
                });
            }
        }
    }
    let footer = Rect::from_min_max(
        Pos2::new(full.left() + 6.0, full.bottom() - FOOTER_H + 4.0),
        Pos2::new(full.left() + HEADER_W - 6.0, full.bottom() - 2.0),
    );
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(footer)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    if child.button("＋ チェーンを追加").clicked() {
        actions.push(Action::AddChain);
    }
    painter.vline(
        full.left() + HEADER_W,
        full.y_range(),
        Stroke::new(1.0, visuals.widgets.noninteractive.bg_stroke.color),
    );

    // 表示範囲を内容に合わせて広げすぎない
    let max_scroll = (content_end(cx) - 1.0).max(0.0);
    state.scroll = state.scroll.min(max_scroll);
    actions
}

fn draw_clip(
    painter: &egui::Painter,
    r: Rect,
    fill: Color32,
    selected: bool,
    name: &str,
    note: Option<&str>,
) {
    if r.width() < 1.0 {
        return;
    }
    painter.rect_filled(r, CornerRadius::same(4), fill.gamma_multiply(0.85));
    painter.rect_stroke(
        r,
        CornerRadius::same(4),
        Stroke::new(1.0, fill.gamma_multiply(1.3)),
        StrokeKind::Inside,
    );
    if selected {
        painter.rect_stroke(
            r,
            CornerRadius::same(4),
            Stroke::new(2.0, Color32::WHITE),
            StrokeKind::Inside,
        );
    }
    let text_painter = painter.with_clip_rect(r.shrink(2.0).intersect(painter.clip_rect()));
    let label = match note {
        Some(n) => format!("{name}  ({n})"),
        None => name.to_owned(),
    };
    text_painter.text(
        r.left_top() + Vec2::new(5.0, 3.0),
        Align2::LEFT_TOP,
        label,
        FontId::proportional(12.0),
        Color32::WHITE,
    );
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use eframe::egui::{Pos2, Vec2};
    use egui_kittest::Harness;
    use fdmv_edit::project::SourceVideo;
    use fdmv_edit::{Project, Source};

    use super::*;

    struct State {
        tl: TimelineState,
        project: Project,
        origin: Pos2,
        actions: Vec<Action>,
        chain: Id,
    }

    fn harness() -> Harness<'static, State> {
        let mut p = Project::new();
        let src = |video: bool, dur: f64| Source {
            id: 0,
            path: PathBuf::from("/x"),
            relative_path: None,
            duration: dur,
            video: video.then_some(SourceVideo {
                width: 640,
                height: 360,
                fps_num: 10,
                fps_den: 1,
            }),
            has_audio: true,
            audio_offset: 0.0,
            video_ss_offset: 0.0,
        };
        let v = p.add_source(src(true, 10.0));
        let a = p.add_source(src(false, 2.0));
        p.insert_video(v, None).unwrap();
        let chain = p.add_chain("解説");
        p.add_audio(chain, a, 2.0).unwrap();
        let state = State {
            tl: TimelineState {
                pps: 50.0,
                ..Default::default()
            },
            project: p,
            origin: Pos2::ZERO,
            actions: Vec::new(),
            chain,
        };
        let mut h = Harness::builder()
            .with_size(Vec2::new(1000.0, 300.0))
            .build_ui_state(
                |ui, st: &mut State| {
                    st.origin = ui.max_rect().min;
                    let muted = HashSet::new();
                    let note = |_: Id| None;
                    let cx = TimelineCtx {
                        project: &st.project,
                        playhead: 0.0,
                        in_point: None,
                        out_point: None,
                        selection: Selection::None,
                        muted: &muted,
                        source_note: &note,
                    };
                    let actions = show(ui, &mut st.tl, &cx);
                    st.actions.extend(actions);
                },
                state,
            );
        h.run();
        h.state_mut().actions.clear();
        h
    }

    /// 時刻 `t` と行の中央の位置（行 0 = 映像、1 = main、2 = 解説）。
    fn at(h: &Harness<State>, t: f64, row: usize) -> Pos2 {
        let o = h.state().origin;
        let y = RULER_H
            + if row == 0 {
                VIDEO_H / 2.0
            } else {
                VIDEO_H + (row as f32 - 0.5) * CHAIN_H
            };
        Pos2::new(o.x + HEADER_W + t as f32 * 50.0, o.y + y)
    }

    fn drag(h: &mut Harness<State>, from: Pos2, to: Pos2) {
        h.hover_at(from);
        h.run();
        h.drag_at(from);
        h.run();
        for i in 1..=10 {
            h.hover_at(from + (to - from) * (i as f32 / 10.0));
            h.run();
        }
        h.drop_at(to);
        h.run();
    }

    fn last_edit(h: &Harness<State>) -> Option<Edit> {
        h.state().actions.iter().rev().find_map(|a| match a {
            Action::Edit { edit, .. } => Some(edit.clone()),
            _ => None,
        })
    }

    #[test]
    fn drag_moves_audio_clip() {
        let mut h = harness();
        let chain = h.state().chain;
        let (from, to) = (at(&h, 3.0, 2), at(&h, 4.0, 2));
        drag(&mut h, from, to);
        match last_edit(&h) {
            Some(Edit::MoveAudio {
                chain: c, start, ..
            }) => {
                assert_eq!(c, chain);
                assert!((start - 3.0).abs() < 1e-9, "start {start}");
            }
            e => panic!("unexpected {e:?}"),
        }
        let begins = h
            .state()
            .actions
            .iter()
            .filter(|a| matches!(a, Action::Edit { begin: true, .. }))
            .count();
        assert_eq!(begins, 1, "undo is recorded once per drag");
        assert!(
            h.state()
                .actions
                .iter()
                .any(|a| matches!(a, Action::EndDrag))
        );
    }

    #[test]
    fn drag_left_edge_trims_audio_clip() {
        let mut h = harness();
        let from = at(&h, 2.0, 2) + Vec2::new(2.0, 0.0);
        drag(&mut h, from, from + Vec2::new(25.0, 0.0));
        match last_edit(&h) {
            Some(Edit::TrimAudio {
                start: Some(s),
                end: None,
                ..
            }) => assert!((s - 2.5).abs() < 1e-9, "start {s}"),
            e => panic!("unexpected {e:?}"),
        }
    }

    #[test]
    fn drag_right_edge_trims_video_clip_by_frames() {
        let mut h = harness();
        let from = at(&h, 10.0, 0) - Vec2::new(2.0, 0.0);
        drag(&mut h, from, from - Vec2::new(52.0, 0.0));
        match last_edit(&h) {
            Some(Edit::TrimVideo {
                src_in, src_out, ..
            }) => {
                assert_eq!(src_in, 0.0);
                // 52px = 1.04 秒 → フレーム (0.1 秒) 単位に丸めて 1.0 秒
                assert!((src_out - 9.0).abs() < 1e-9, "out {src_out}");
            }
            e => panic!("unexpected {e:?}"),
        }
    }

    #[test]
    fn click_on_empty_track_seeks_and_selects_chain() {
        let mut h = harness();
        let chain = h.state().chain;
        let p = at(&h, 7.0, 2);
        h.hover_at(p);
        h.run();
        h.drag_at(p);
        h.run();
        h.drop_at(p);
        h.run();
        let acts = &h.state().actions;
        assert!(
            acts.iter()
                .any(|a| matches!(a, Action::Seek(t) if (t - 7.0).abs() < 1e-9)),
            "{acts:?}"
        );
        assert!(
            acts.iter()
                .any(|a| matches!(a, Action::Select(Selection::Chain(c)) if *c == chain))
        );
    }

    #[test]
    fn clicking_a_clip_selects_it() {
        let mut h = harness();
        let p = at(&h, 5.0, 0);
        h.hover_at(p);
        h.run();
        h.drag_at(p);
        h.run();
        h.drop_at(p);
        h.run();
        let video = h.state().project.video[0].id;
        assert!(
            h.state()
                .actions
                .iter()
                .any(|a| matches!(a, Action::Select(Selection::Video(v)) if *v == video))
        );
    }

    #[test]
    fn drag_reorders_video_clips() {
        let mut h = harness();
        // 映像を 4 秒で分割して [0,4) [4,10) にし、前半を後ろへ移動する
        h.state_mut().project.split_video(4.0).unwrap();
        h.run();
        let first = h.state().project.video[0].id;
        let (from, to) = (at(&h, 2.0, 0), at(&h, 9.5, 0));
        drag(&mut h, from, to);
        let edit = h.state().actions.iter().rev().find_map(|a| match a {
            Action::Edit {
                edit: e @ Edit::MoveVideo { .. },
                begin: true,
            } => Some(e.clone()),
            _ => None,
        });
        let Some(Edit::MoveVideo { clip, to }) = edit else {
            panic!("no move: {:?}", h.state().actions)
        };
        assert_eq!(clip, first);
        let p = &mut h.state_mut().project;
        p.move_video(clip, to);
        let spans: Vec<(f64, f64)> = p.video.iter().map(|c| (c.src_in, c.src_out)).collect();
        assert_eq!(spans, vec![(4.0, 10.0), (0.0, 4.0)]);
    }
}
