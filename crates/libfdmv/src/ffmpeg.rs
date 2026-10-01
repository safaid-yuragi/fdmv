//! 外部コマンド `ffmpeg` / `ffprobe` の呼び出し。ライブラリとしてはリンクしない。
//!
//! 実行ファイルは環境変数 `FDMV_FFMPEG` / `FDMV_FFPROBE` で変更できる（既定は PATH 上のもの）。

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::{Error, Result};

#[derive(Clone, Debug)]
pub struct Ffmpeg {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    /// true なら ffmpeg の進捗を標準エラーにそのまま表示する。
    pub verbose: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ProbeInfo {
    pub has_video: bool,
    /// 音声ストリームの数。
    pub audio_streams: usize,
    pub video_start: Option<f64>,
    pub audio_start: Option<f64>,
    /// ファイル全体の開始時刻（ffmpeg の `-ss` はここからの相対位置）。
    pub start: Option<f64>,
    pub duration: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// 映像のフレームレート（分子, 分母）。
    pub frame_rate: Option<(u32, u32)>,
}

impl ProbeInfo {
    /// 映像の開始時刻に対する音声の開始時刻のずれ（秒）。
    pub fn audio_offset(&self) -> f64 {
        self.audio_start.unwrap_or(0.0) - self.video_start.unwrap_or(0.0)
    }
}

#[derive(Clone, Debug)]
pub struct VideoEncodeOptions {
    /// `libsvtav1` / `libaom-av1` / `librav1e`。None なら使えるものを自動で選ぶ。
    pub encoder: Option<String>,
    /// 画質（0–63、小さいほど高画質・大容量）。既定は [`Quality::High`]。
    pub crf: u32,
    /// 速度プリセット。エンコーダごとの意味（SVT-AV1: 0–13, libaom: cpu-used 0–8, rav1e: speed 0–10）。
    pub preset: Option<u32>,
    pub pix_fmt: String,
    /// ffmpeg に追加で渡す引数（出力オプションとして）。
    pub extra_args: Vec<String>,
}

impl Default for VideoEncodeOptions {
    fn default() -> Self {
        let (crf, preset) = Quality::High.crf_preset();
        VideoEncodeOptions {
            encoder: None,
            crf,
            preset: Some(preset),
            pix_fmt: "yuv420p".into(),
            extra_args: Vec::new(),
        }
    }
}

/// 画質のプリセット（SVT-AV1 の CRF と速度プリセットの組）。
///
/// 720p / 1080p の素材で VMAF を測って決めた値（VMAF 95 以上で原本との差がほぼ分からない）:
///
/// | プリセット | CRF | preset | 細かい模様の多い映像 | 文字・図形 |
/// |---|---|---|---|---|
/// | Best | 18 | 6 | 約 97 | 99 以上 |
/// | High（既定） | 23 | 6 | 約 96 | 99 以上 |
/// | Standard | 30 | 8 | 約 94 | 約 98 |
/// | Small | 38 | 8 | 90 前後 | 約 96 |
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quality {
    Best,
    High,
    Standard,
    Small,
}

impl Quality {
    pub const ALL: [Quality; 4] = [
        Quality::Best,
        Quality::High,
        Quality::Standard,
        Quality::Small,
    ];

    /// (CRF, SVT-AV1 の preset)
    pub fn crf_preset(self) -> (u32, u32) {
        match self {
            Quality::Best => (18, 6),
            Quality::High => (23, 6),
            Quality::Standard => (30, 8),
            Quality::Small => (38, 8),
        }
    }

    /// CRF と preset がどのプリセットに一致するか。
    pub fn from_crf_preset(crf: u32, preset: Option<u32>) -> Option<Quality> {
        Quality::ALL.into_iter().find(|q| {
            let (c, p) = q.crf_preset();
            c == crf && preset == Some(p)
        })
    }
}

#[derive(Clone, Debug)]
pub struct AudioEncodeOptions {
    /// 例: `128k`
    pub bitrate: String,
    pub channels: u8,
}

impl Default for AudioEncodeOptions {
    fn default() -> Self {
        AudioEncodeOptions {
            bitrate: "128k".into(),
            channels: 2,
        }
    }
}

pub const AV1_ENCODERS: [&str; 3] = ["libsvtav1", "libaom-av1", "librav1e"];

impl Ffmpeg {
    pub fn locate() -> Result<Self> {
        let ffmpeg = std::env::var_os("FDMV_FFMPEG").unwrap_or_else(|| "ffmpeg".into());
        let ffprobe = std::env::var_os("FDMV_FFPROBE").unwrap_or_else(|| "ffprobe".into());
        let ff = Ffmpeg {
            ffmpeg: ffmpeg.into(),
            ffprobe: ffprobe.into(),
            verbose: false,
        };
        for exe in [&ff.ffmpeg, &ff.ffprobe] {
            let ok = Command::new(exe)
                .arg("-version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if !ok {
                return Err(Error::Ffmpeg(format!(
                    "{} not found; install ffmpeg or set FDMV_FFMPEG / FDMV_FFPROBE",
                    exe.display()
                )));
            }
        }
        Ok(ff)
    }

    pub fn encoders(&self) -> Result<Vec<String>> {
        let out = Command::new(&self.ffmpeg)
            .args(["-hide_banner", "-encoders"])
            .stderr(Stdio::null())
            .output()?;
        let text = String::from_utf8_lossy(&out.stdout);
        Ok(text
            .lines()
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                let flags = it.next()?;
                let name = it.next()?;
                // 例: " V....D libaom-av1   libaom AV1 (codec av1)"。凡例の行 " V..... = Video" は除く。
                (flags.len() == 6 && name != "=").then(|| name.to_owned())
            })
            .collect())
    }

    pub fn pick_av1_encoder(&self, preferred: Option<&str>) -> Result<String> {
        let available = self.encoders()?;
        if let Some(p) = preferred {
            if available.iter().any(|e| e == p) {
                return Ok(p.to_owned());
            }
            return Err(Error::Ffmpeg(format!(
                "encoder {p} is not available in this ffmpeg"
            )));
        }
        AV1_ENCODERS
            .iter()
            .find(|e| available.iter().any(|a| a == *e))
            .map(|e| e.to_string())
            .ok_or_else(|| {
                Error::Ffmpeg("no AV1 encoder (libsvtav1 / libaom-av1 / librav1e) found".into())
            })
    }

    pub fn probe(&self, input: &Path) -> Result<ProbeInfo> {
        let out = Command::new(&self.ffprobe)
            .args(["-v", "error", "-show_entries"])
            .arg("stream=index,codec_type,start_time,width,height,r_frame_rate:format=duration,start_time")
            .args(["-of", "flat"])
            .arg(input)
            .stdin(Stdio::null())
            .output()?;
        if !out.status.success() {
            return Err(Error::Ffmpeg(format!(
                "ffprobe failed for {}: {}",
                input.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut streams: BTreeMap<usize, BTreeMap<String, String>> = BTreeMap::new();
        let mut info = ProbeInfo::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim_matches('"');
            if key == "format.duration" {
                info.duration = value.parse().ok();
            } else if key == "format.start_time" {
                info.start = value.parse().ok();
            } else if let Some(rest) = key.strip_prefix("streams.stream.")
                && let Some((n, field)) = rest.split_once('.')
                && let Ok(n) = n.parse::<usize>()
            {
                streams
                    .entry(n)
                    .or_default()
                    .insert(field.to_owned(), value.to_owned());
            }
        }
        for fields in streams.values() {
            let get = |k: &str| fields.get(k).map(String::as_str);
            let start = get("start_time").and_then(|v| v.parse().ok());
            match get("codec_type") {
                Some("video") if !info.has_video => {
                    info.has_video = true;
                    info.video_start = start;
                    info.width = get("width").and_then(|v| v.parse().ok());
                    info.height = get("height").and_then(|v| v.parse().ok());
                    info.frame_rate = get("r_frame_rate").and_then(|v| {
                        let (n, d) = v.split_once('/')?;
                        let (n, d) = (n.parse().ok()?, d.parse().ok()?);
                        (n > 0 && d > 0).then_some((n, d))
                    });
                }
                Some("audio") => {
                    if info.audio_streams == 0 {
                        info.audio_start = start;
                    }
                    info.audio_streams += 1;
                }
                _ => {}
            }
        }
        Ok(info)
    }

    /// AV1 エンコーダの選択と、出力側の引数（画素形式・エンコーダ・品質・追加引数）。
    pub fn av1_output_args(&self, opts: &VideoEncodeOptions) -> Result<(String, Vec<OsString>)> {
        let encoder = self.pick_av1_encoder(opts.encoder.as_deref())?;
        let mut args = strs(&["-pix_fmt", &opts.pix_fmt, "-c:v", &encoder]);
        let crf = opts.crf.min(63);
        match encoder.as_str() {
            "libsvtav1" => {
                args.extend(strs(&["-crf", &crf.to_string()]));
                args.extend(strs(&["-preset", &opts.preset.unwrap_or(6).to_string()]));
                // 見た目の画質を優先するチューニング（既定は PSNR 優先）。
                if !opts.extra_args.iter().any(|a| a == "-svtav1-params") {
                    args.extend(strs(&["-svtav1-params", "tune=0"]));
                }
            }
            "libaom-av1" => {
                args.extend(strs(&[
                    "-crf",
                    &crf.to_string(),
                    "-b:v",
                    "0",
                    "-row-mt",
                    "1",
                ]));
                args.extend(strs(&["-cpu-used", &opts.preset.unwrap_or(6).to_string()]));
            }
            "librav1e" => {
                // rav1e は量子化パラメータ (0–255) で指定する。
                args.extend(strs(&["-qp", &(crf * 4).min(255).to_string()]));
                args.extend(strs(&["-speed", &opts.preset.unwrap_or(6).to_string()]));
            }
            _ => {}
        }
        args.extend(opts.extra_args.iter().map(OsString::from));
        Ok((encoder, args))
    }

    /// 入力の最初の映像ストリームを AV1 の IVF にエンコードする。使ったエンコーダ名を返す。
    pub fn encode_video(
        &self,
        input: &Path,
        output: &Path,
        opts: &VideoEncodeOptions,
    ) -> Result<String> {
        let (encoder, codec_args) = self.av1_output_args(opts)?;
        let mut args: Vec<OsString> = vec!["-i".into(), input.as_os_str().to_owned()];
        args.extend(strs(&[
            "-map",
            "0:v:0",
            "-an",
            "-sn",
            "-dn",
            "-map_metadata",
            "-1",
        ]));
        args.extend(codec_args);
        args.extend(strs(&["-f", "ivf"]));
        args.push(output.as_os_str().to_owned());
        self.run(&args)?;
        Ok(encoder)
    }

    /// 任意の入力と `-filter_complex` から AV1 の IVF を作る。`map` は映像の出力ラベル（例: `[v]`）。
    /// `total_secs` は進捗計算用の出力の長さ。使ったエンコーダ名を返す。
    #[allow(clippy::too_many_arguments)]
    pub fn encode_video_complex(
        &self,
        input_args: &[OsString],
        filter: &str,
        map: &str,
        output: &Path,
        opts: &VideoEncodeOptions,
        total_secs: f64,
        progress: &mut dyn FnMut(f64),
        cancel: Option<&AtomicBool>,
    ) -> Result<String> {
        let (encoder, codec_args) = self.av1_output_args(opts)?;
        let mut args: Vec<OsString> = input_args.to_vec();
        args.extend(strs(&[
            "-filter_complex",
            filter,
            "-map",
            map,
            "-an",
            "-sn",
            "-dn",
        ]));
        args.extend(strs(&["-map_metadata", "-1"]));
        args.extend(codec_args);
        args.extend(strs(&["-f", "ivf"]));
        args.push(output.as_os_str().to_owned());
        self.run_with_progress(&args, total_secs, progress, cancel)?;
        Ok(encoder)
    }

    /// ffmpeg を実行し、`-progress` の出力から進捗（0.0–1.0）を報告する。
    /// `cancel` が立つと ffmpeg を止めてエラーを返す。
    pub fn run_with_progress<S: AsRef<OsStr>>(
        &self,
        args: &[S],
        total_secs: f64,
        progress: &mut dyn FnMut(f64),
        cancel: Option<&AtomicBool>,
    ) -> Result<()> {
        let mut cmd = Command::new(&self.ffmpeg);
        cmd.args([
            "-hide_banner",
            "-nostdin",
            "-y",
            "-loglevel",
            "error",
            "-nostats",
        ]);
        cmd.args(["-progress", "pipe:1"]);
        cmd.args(args);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn()?;
        let stderr = child.stderr.take().unwrap();
        let err_thread = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = BufReader::new(stderr).read_to_string(&mut s);
            s
        });
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut cancelled = false;
        for line in stdout.lines() {
            let line = line?;
            if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                let _ = child.kill();
                cancelled = true;
                break;
            }
            if let Some(v) = line
                .strip_prefix("out_time_us=")
                .or_else(|| line.strip_prefix("out_time_ms="))
                && let Ok(us) = v.trim().parse::<f64>()
                && total_secs > 0.0
            {
                progress((us / 1e6 / total_secs).clamp(0.0, 1.0));
            }
        }
        let status = child.wait()?;
        let err = err_thread.join().unwrap_or_default();
        if cancelled {
            return Err(Error::Ffmpeg("cancelled".into()));
        }
        if !status.success() {
            return Err(Error::Ffmpeg(format!(
                "ffmpeg exited with {status}: {}",
                err.trim()
            )));
        }
        progress(1.0);
        Ok(())
    }

    /// 入力の `audio_index` 番目の音声ストリームを Ogg Opus にエンコードする。
    /// `trim_start` 秒だけ先頭を切り落とす。
    pub fn encode_audio(
        &self,
        input: &Path,
        audio_index: usize,
        trim_start: f64,
        output: &Path,
        opts: &AudioEncodeOptions,
    ) -> Result<()> {
        let mut args: Vec<OsString> = Vec::new();
        args.extend(["-i".into(), input.as_os_str().to_owned()]);
        args.extend(strs(&[
            "-map",
            &format!("0:a:{audio_index}"),
            "-vn",
            "-sn",
            "-dn",
        ]));
        args.extend(strs(&["-map_metadata", "-1"]));
        if trim_start > 0.0 {
            args.extend(strs(&[
                "-af",
                &format!("atrim=start={trim_start:.6},asetpts=PTS-STARTPTS"),
            ]));
        }
        args.extend(strs(&[
            "-c:a",
            "libopus",
            "-b:a",
            &opts.bitrate,
            "-vbr",
            "on",
        ]));
        args.extend(strs(&[
            "-ac",
            &opts.channels.to_string(),
            "-ar",
            "48000",
            "-f",
            "ogg",
        ]));
        args.push(output.as_os_str().to_owned());
        self.run(&args)
    }

    /// 任意の ffmpeg 引数で実行する（入出力の前に共通オプションを付ける）。
    pub fn run<S: AsRef<OsStr>>(&self, args: &[S]) -> Result<()> {
        let mut cmd = Command::new(&self.ffmpeg);
        cmd.args(["-hide_banner", "-nostdin", "-y", "-loglevel", "error"]);
        cmd.arg(if self.verbose { "-stats" } else { "-nostats" });
        cmd.args(args);
        cmd.stdin(Stdio::null());
        if self.verbose {
            let status = cmd.status()?;
            if !status.success() {
                return Err(Error::Ffmpeg(format!("ffmpeg exited with {status}")));
            }
        } else {
            let out = cmd.stdout(Stdio::null()).output()?;
            if !out.status.success() {
                let err = String::from_utf8_lossy(&out.stderr);
                return Err(Error::Ffmpeg(format!(
                    "ffmpeg exited with {}: {}",
                    out.status,
                    err.trim()
                )));
            }
        }
        Ok(())
    }

    /// 標準入力にデータを流し込む ffmpeg を起動する（WAV のパイプ出力などに使う）。
    pub fn spawn_with_stdin<S: AsRef<OsStr>>(&self, args: &[S]) -> Result<std::process::Child> {
        let mut cmd = Command::new(&self.ffmpeg);
        cmd.args(["-hide_banner", "-y", "-loglevel", "error", "-nostats"]);
        cmd.args(args);
        cmd.stdin(Stdio::piped()).stdout(Stdio::null());
        Ok(cmd.spawn()?)
    }
}

fn strs(v: &[&str]) -> Vec<OsString> {
    v.iter().map(OsString::from).collect()
}
