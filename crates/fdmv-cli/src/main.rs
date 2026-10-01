mod commands;
mod util;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// FDMV 動画フォーマットのツール。
#[derive(Parser)]
#[command(name = "fdmv", version, about, propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 動画ファイルと音声素材から .fdmv を作る
    Pack(PackArgs),
    /// 既存の .fdmv にチェーンを追加する
    AddChain(AddChainArgs),
    /// ファイルの情報を表示する
    Info(InfoArgs),
    /// チェーンを音声ファイルとして取り出す（複数指定するとミックス）
    Extract(ExtractArgs),
    /// 映像を取り出す（.ivf はそのまま、それ以外は ffmpeg で再エンコードせずに格納）
    ExtractVideo(ExtractVideoArgs),
    /// 映像と選んだチェーンのミックスを、一般的な動画ファイル（mp4 / mkv / webm）に書き出す
    Export(ExportArgs),
    /// 指定した時刻のフレームを画像として保存する
    Snapshot(SnapshotArgs),
    /// ファイルの整合性を検査する（CRC、構造、デコード）
    Verify(VerifyArgs),
}

/// 音声素材の指定: `PATH` または `PATH@開始時刻`（例: `voice.wav@1:23.5`）
pub type SourceSpec = String;

#[derive(clap::Args)]
pub struct PackArgs {
    /// 入力動画（ffmpeg が読める形式）
    pub input: PathBuf,
    #[arg(short, long)]
    pub output: PathBuf,

    /// デフォルトチェーンの音声。省略時は入力動画の音声を使う
    #[arg(long, value_name = "SRC")]
    pub default_audio: Option<SourceSpec>,
    /// デフォルトチェーンを作らない
    #[arg(long, conflicts_with = "default_audio")]
    pub no_default_audio: bool,
    /// デフォルトチェーンの名前
    #[arg(long, default_value = "main")]
    pub default_name: String,

    /// サブチェーンを追加: `NAME=SRC`。同じ NAME を繰り返すと複数のセグメントになる
    #[arg(short, long, value_name = "NAME=SRC")]
    pub chain: Vec<String>,
    /// チェーンの初期ゲイン: `NAME=DB`（例: `解説=-3`）
    #[arg(long, value_name = "NAME=DB", allow_hyphen_values = true)]
    pub gain: Vec<String>,
    /// チェーンのメタデータ: `NAME:KEY=VALUE`（例: `解説:language=ja`）
    #[arg(long, value_name = "NAME:KEY=VALUE")]
    pub chain_meta: Vec<String>,
    /// ファイルのメタデータ: `KEY=VALUE`
    #[arg(long, value_name = "KEY=VALUE")]
    pub meta: Vec<String>,
    /// タイトル（`--meta title=...` と同じ）
    #[arg(long)]
    pub title: Option<String>,

    #[command(flatten)]
    pub video: VideoOpts,
    #[command(flatten)]
    pub audio: AudioOpts,
    /// ffmpeg の進捗を表示しない
    #[arg(short, long)]
    pub quiet: bool,
}

#[derive(clap::Args)]
pub struct VideoOpts {
    /// AV1 エンコーダ（libsvtav1 / libaom-av1 / librav1e）。省略時は自動
    #[arg(long)]
    pub encoder: Option<String>,
    /// 画質プリセット（best: CRF 18 / high: CRF 23 / standard: CRF 30 / small: CRF 38）
    #[arg(short = 'Q', long, value_enum, default_value_t = QualityArg::High)]
    pub quality: QualityArg,
    /// 画質 0–63（小さいほど高画質・大容量）。指定すると --quality より優先
    #[arg(long, value_parser = clap::value_parser!(u32).range(0..=63))]
    pub crf: Option<u32>,
    /// 速度プリセット（SVT-AV1: 0–13。値が小さいほど遅く高効率）。指定すると --quality より優先
    #[arg(long)]
    pub preset: Option<u32>,
    /// 10 bit で符号化する（グラデーションの帯が出にくく、圧縮効率も上がる）
    #[arg(long)]
    pub ten_bit: bool,
    /// ffmpeg に追加で渡す出力オプション（例: `--ffmpeg-arg=-vf --ffmpeg-arg=scale=1280:-2`）
    #[arg(long, allow_hyphen_values = true)]
    pub ffmpeg_arg: Vec<String>,
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum QualityArg {
    /// ほぼ無劣化（CRF 18）
    Best,
    /// 高画質（CRF 23、既定）
    High,
    /// 標準（CRF 30）
    Standard,
    /// 小容量（CRF 38）
    Small,
}

#[derive(clap::Args)]
pub struct AudioOpts {
    /// Opus のビットレート
    #[arg(long, default_value = "128k")]
    pub audio_bitrate: String,
    /// チャンネル数（1 または 2）
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u8).range(1..=2))]
    pub channels: u8,
}

#[derive(clap::Args)]
pub struct AddChainArgs {
    /// 対象の .fdmv
    pub file: PathBuf,
    /// チェーン名
    #[arg(short, long)]
    pub name: String,
    /// 音声素材（繰り返すと複数のセグメント）
    #[arg(short, long = "source", value_name = "SRC", required = true)]
    pub sources: Vec<SourceSpec>,
    /// 初期ゲイン [dB]
    #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
    pub gain: f32,
    /// メタデータ: `KEY=VALUE`（例: `language=ja`）
    #[arg(long, value_name = "KEY=VALUE")]
    pub meta: Vec<String>,
    /// デフォルトチェーンとして追加する（デフォルトチェーンがまだ無い場合のみ）
    #[arg(long)]
    pub default: bool,
    /// 結果を別ファイルに書く
    #[arg(
        short,
        long,
        required_unless_present = "in_place",
        conflicts_with = "in_place"
    )]
    pub output: Option<PathBuf>,
    /// 元のファイルに追記する
    #[arg(long)]
    pub in_place: bool,
    #[command(flatten)]
    pub audio: AudioOpts,
    #[arg(short, long)]
    pub quiet: bool,
}

#[derive(clap::Args)]
pub struct InfoArgs {
    pub file: PathBuf,
    /// JSON で出力する
    #[arg(long)]
    pub json: bool,
    /// インデックスも表示する
    #[arg(long)]
    pub index: bool,
}

#[derive(clap::Args)]
pub struct ExtractArgs {
    pub file: PathBuf,
    /// 出力ファイル。.wav 以外は ffmpeg で変換する
    #[arg(short, long)]
    pub output: PathBuf,
    /// 取り出すチェーン名（繰り返すとミックス）。省略時はデフォルトチェーン
    #[arg(short, long)]
    pub chain: Vec<String>,
    /// チェーンの初期ゲインを適用する
    #[arg(long)]
    pub apply_gain: bool,
    /// WAV のサンプル形式
    #[arg(long, value_enum, default_value_t = WavFormat::S16)]
    pub format: WavFormat,
    /// 出力チャンネル数（省略時はチェーンに合わせる）
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..=2))]
    pub channels: Option<u8>,
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum WavFormat {
    S16,
    F32,
}

#[derive(clap::Args)]
pub struct ExtractVideoArgs {
    pub file: PathBuf,
    #[arg(short, long)]
    pub output: PathBuf,
}

#[derive(clap::Args)]
pub struct ExportArgs {
    pub file: PathBuf,
    #[arg(short, long)]
    pub output: PathBuf,
    /// デフォルトチェーンに重ねるサブチェーン（繰り返し可）
    #[arg(short, long)]
    pub chain: Vec<String>,
    /// デフォルトチェーンを含めない
    #[arg(long)]
    pub no_default: bool,
    /// 音声コーデック（省略時: mp4/mov は aac、それ以外は libopus）
    #[arg(long)]
    pub audio_codec: Option<String>,
    #[arg(long, default_value = "192k")]
    pub audio_bitrate: String,
}

#[derive(clap::Args)]
pub struct SnapshotArgs {
    pub file: PathBuf,
    /// 時刻（例: `12.5`、`1:02:03`）
    #[arg(long, default_value = "0")]
    pub at: String,
    /// 出力画像。.ppm 以外は ffmpeg で変換する
    #[arg(short, long)]
    pub output: PathBuf,
}

#[derive(clap::Args)]
pub struct VerifyArgs {
    pub file: PathBuf,
    /// デコードによる検査を省く
    #[arg(long)]
    pub no_decode: bool,
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Pack(a) => commands::pack(a),
        Command::AddChain(a) => commands::add_chain(a),
        Command::Info(a) => commands::info(a),
        Command::Extract(a) => commands::extract(a),
        Command::ExtractVideo(a) => commands::extract_video(a),
        Command::Export(a) => commands::export(a),
        Command::Snapshot(a) => commands::snapshot(a),
        Command::Verify(a) => commands::verify(a),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
