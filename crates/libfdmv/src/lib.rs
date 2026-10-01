//! `libfdmv` — FDMV 動画フォーマットのリファレンス実装。
//!
//! FDMV は AV1 映像 1 本と、映像に同期して重ねて再生される Opus 音声「チェーン」複数本を
//! 格納するコンテナ。仕様はリポジトリ直下の `SPEC.md` を参照。
//!
//! 主なモジュール:
//!
//! - [`reader`] / [`writer`] — コンテナの読み書きとチェーンの追記
//! - [`model`] — ディレクトリ（ストリーム定義・セグメント・インデックス）
//! - [`decode`] — AV1 / Opus のデコードとチェーンのミックス（`decode` feature）
//! - [`ffmpeg`] / [`pack`] — 外部コマンドの ffmpeg を使った素材の取り込み

pub mod av1;
pub mod block;
pub mod error;
pub mod ffmpeg;
pub mod format;
pub mod ivf;
pub mod model;
pub mod ogg;
pub mod opus;
pub mod pack;
pub mod reader;
pub mod time;
pub mod wav;
pub mod writer;

#[cfg(feature = "decode")]
pub mod decode;

pub use error::{Error, Result};
pub use format::Rational;
pub use model::{
    ChainParams, ChainRole, Directory, IndexEntry, Packet, Segment, StreamEntry, StreamParams,
    VideoParams,
};
pub use reader::{FdmvReader, StreamCursor};
pub use writer::Muxer;
