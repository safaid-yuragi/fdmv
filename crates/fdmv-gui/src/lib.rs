//! FDMV の GUI アプリ（プレイヤー・エディタ）で共有する部品。
//!
//! - [`audio`] — 音声出力（cpal）。再生位置のマスタークロックを兼ねる
//! - [`video`] — 映像の先読みデコードとフレームキュー
//! - [`fonts`] — 日本語フォントの検出と登録

pub mod audio;
pub mod fonts;
pub mod video;

pub use audio::{AudioOutput, PcmSource};
pub use video::{Frame, FrameSource, VideoOutput};

/// libfdmv の映像フレームを RGBA の [`Frame`] に変換する。
pub fn frame_from_decoded(f: &libfdmv::decode::VideoFrame, pts: f64, buf: &mut Vec<u8>) -> Frame {
    f.write_rgba8(buf);
    Frame {
        pts,
        width: f.width() as usize,
        height: f.height() as usize,
        rgba: std::mem::take(buf),
    }
}
