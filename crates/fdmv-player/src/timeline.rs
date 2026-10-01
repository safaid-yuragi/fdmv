//! シークバー。チェーンごとにセグメントのある区間を色付きの帯で表示する。

use eframe::egui::{Color32, CornerRadius, Pos2, Rect, Sense, Stroke, Ui, Vec2};
use libfdmv::Segment;
use libfdmv::format::Rational;
use libfdmv::time::format_time;

pub struct Lane<'a> {
    pub color: Color32,
    pub segments: &'a [Segment],
    pub active: bool,
}

pub struct TimelineResponse {
    /// クリック／ドラッグされた時刻。
    pub seek: Option<f64>,
    /// ドラッグを終えた。
    pub released: bool,
}

const LANE_HEIGHT: f32 = 4.0;
const LANE_GAP: f32 = 2.0;
const TRACK_HEIGHT: f32 = 6.0;
const HANDLE_RADIUS: f32 = 7.0;

pub fn timeline(ui: &mut Ui, position: f64, duration: f64, lanes: &[Lane]) -> TimelineResponse {
    let lanes_h = lanes.len() as f32 * (LANE_HEIGHT + LANE_GAP);
    let height = lanes_h + HANDLE_RADIUS * 2.0 + 2.0;
    let width = ui.available_width();
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(width, height), Sense::click_and_drag());
    let painter = ui.painter_at(rect.expand(HANDLE_RADIUS));
    let visuals = ui.visuals();

    let inner = rect.shrink2(Vec2::new(HANDLE_RADIUS, 0.0));
    let x_of =
        |t: f64| inner.left() + (t / duration.max(1e-9)).clamp(0.0, 1.0) as f32 * inner.width();
    let t_of = |x: f32| ((x - inner.left()) / inner.width()).clamp(0.0, 1.0) as f64 * duration;

    // チェーンの区間
    for (i, lane) in lanes.iter().enumerate() {
        let y = rect.top() + i as f32 * (LANE_HEIGHT + LANE_GAP);
        let row = Rect::from_x_y_ranges(inner.x_range(), y..=y + LANE_HEIGHT);
        painter.rect_filled(row, CornerRadius::same(1), visuals.extreme_bg_color);
        let color = if lane.active {
            lane.color
        } else {
            lane.color.gamma_multiply(0.35)
        };
        for s in lane.segments {
            let a = x_of(Rational::OPUS.to_seconds(s.start));
            let b = x_of(Rational::OPUS.to_seconds(s.end())).max(a + 1.0);
            painter.rect_filled(
                Rect::from_x_y_ranges(a..=b, row.y_range()),
                CornerRadius::same(1),
                color,
            );
        }
    }

    // トラックと再生位置
    let cy = rect.top() + lanes_h + HANDLE_RADIUS + 1.0;
    let track = Rect::from_center_size(
        Pos2::new(inner.center().x, cy),
        Vec2::new(inner.width(), TRACK_HEIGHT),
    );
    painter.rect_filled(track, CornerRadius::same(3), visuals.extreme_bg_color);
    let px = x_of(position);
    let played = Rect::from_x_y_ranges(track.left()..=px, track.y_range());
    painter.rect_filled(played, CornerRadius::same(3), visuals.selection.bg_fill);
    let handle_color = if response.hovered() || response.dragged() {
        visuals.strong_text_color()
    } else {
        visuals.text_color()
    };
    painter.circle(
        Pos2::new(px, cy),
        HANDLE_RADIUS * 0.75,
        handle_color,
        Stroke::NONE,
    );

    // ホバー位置の時刻
    if let Some(p) = response.hover_pos() {
        painter.line_segment(
            [Pos2::new(p.x, rect.top()), Pos2::new(p.x, rect.bottom())],
            Stroke::new(1.0, visuals.weak_text_color()),
        );
        response
            .clone()
            .on_hover_text_at_pointer(format_time(t_of(p.x)));
    }

    let seek = if response.is_pointer_button_down_on() || response.clicked() {
        ui.ctx().pointer_interact_pos().map(|p| t_of(p.x))
    } else {
        None
    };
    TimelineResponse {
        seek,
        released: response.drag_stopped() || response.clicked(),
    }
}

/// チェーンの表示色。
pub fn chain_color(index: usize) -> Color32 {
    const PALETTE: [Color32; 8] = [
        Color32::from_rgb(0x4e, 0x9a, 0xf1),
        Color32::from_rgb(0xf2, 0x8e, 0x2b),
        Color32::from_rgb(0x59, 0xc1, 0x6b),
        Color32::from_rgb(0xe1, 0x57, 0x59),
        Color32::from_rgb(0xb0, 0x7a, 0xe0),
        Color32::from_rgb(0xed, 0xc9, 0x48),
        Color32::from_rgb(0x4b, 0xc6, 0xc6),
        Color32::from_rgb(0xff, 0x9d, 0xa7),
    ];
    PALETTE[index % PALETTE.len()]
}
