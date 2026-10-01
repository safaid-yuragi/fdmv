//! `fdmv-edit` — FDMV の簡易動画編集の中核（GUI に依存しない部分）。
//!
//! - [`project`] — プロジェクト（`.fdmvproj`）のデータ構造と編集操作
//! - [`proxy`] — 素材の取り込みとプレビュー用プロキシ
//! - [`preview`] — タイムラインの音声ミックスと映像（プレビュー・書き出し共通）
//! - [`export`] — .fdmv への書き出し

pub mod export;
pub mod preview;
pub mod project;
pub mod proxy;
pub mod wavfile;

pub use project::{AudioClip, Chain, ChainRole, Id, Project, Settings, Source, VideoClip};
pub use proxy::ProxyStore;
