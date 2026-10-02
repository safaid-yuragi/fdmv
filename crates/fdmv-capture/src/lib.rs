//! `fdmv-capture` — 画面とアプリごとの音声を録って FDMV にする。
//!
//! 録画中は、映像を ffmpeg で逐次エンコードし（GPU の AV1 / HEVC、または CPU の AV1）、音声はチェーンごとに
//! 生の PCM（パート）としてディスクに書く。停止後に音声をミックス・無音区間で分割して
//! セグメントにし、[`libfdmv::pack::pack_ivf`] で FDMV にまとめる。
//!
//! - [`chain`] — チェーン 1 本分の録音（パートの書き込みと時刻合わせ）
//! - [`audio`] — アプリごとの音声・マイクの取り込み（OS ごとの実装）
//! - [`video`] — 画面・ウィンドウの取り込み（OS ごとの実装）
//! - [`codecs`] — 録画中に使う映像エンコーダ（GPU / CPU）の選択
//! - [`encoder`] — 時刻付きで ffmpeg に映像を送るエンコーダ
//! - [`recorder`] — 録画セッション全体
//! - [`finalize`] — 停止後の FDMV への変換

pub mod audio;
pub mod chain;
pub mod codecs;
pub mod encoder;
pub mod finalize;
mod mkvpipe;
pub mod recorder;
pub mod video;

pub use audio::{AudioApp, MicDevice};
pub use chain::Chain;
pub use recorder::{AudioSource, ChainSetup, RecordOptions, Recorder, Recording, VideoMode};
pub use video::{Frame, FrameSlot, VideoCapture, VideoTarget};

/// チェーンのサンプルレート（Opus の 48 kHz）。
pub const SAMPLE_RATE: u32 = libfdmv::format::OPUS_SAMPLE_RATE;
