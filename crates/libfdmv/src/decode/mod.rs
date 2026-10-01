//! デコード（`decode` feature）。
//!
//! - [`VideoDecoder`] / [`VideoStream`] — AV1 → YUV フレーム（dav1d）
//! - [`ChainDecoder`] — 1 本のチェーンの Opus パケット → タイムライン上の PCM（libopus）
//! - [`AudioRenderer`] — 複数のチェーンをタイムラインに沿ってミックスする。無音区間も埋める

mod audio;
mod mixer;
mod video;

pub use audio::{ChainDecoder, DecodedAudio, OpusDecoder};
pub use mixer::{AudioRenderer, ChainSelection};
pub use video::{PixelLayout, PlaneKind, VideoDecoder, VideoFrame, VideoStream};
