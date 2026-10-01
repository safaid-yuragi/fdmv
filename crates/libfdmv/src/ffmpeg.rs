//! 外部コマンド `ffmpeg` / `ffprobe` の呼び出し。ライブラリとしてはリンクしない。
//!
//! 実行ファイルは環境変数 `FDMV_FFMPEG` / `FDMV_FFPROBE` で変更できる（既定は PATH 上のもの）。

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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
    pub duration: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct VideoEncodeOptions {
    /// `libsvtav1` / `libaom-av1` / `librav1e`。None なら使えるものを自動で選ぶ。
    pub encoder: Option<String>,
    /// 画質（0–63、小さいほど高画質・大容量）。
    pub crf: u32,
    /// 速度プリセット。エンコーダごとの意味（SVT-AV1: 0–13, libaom: cpu-used 0–8, rav1e: speed 0–10）。
    pub preset: Option<u32>,
    pub pix_fmt: String,
    /// ffmpeg に追加で渡す引数（出力オプションとして）。
    pub extra_args: Vec<String>,
}

impl Default for VideoEncodeOptions {
    fn default() -> Self {
        VideoEncodeOptions {
            encoder: None,
            crf: 32,
            preset: None,
            pix_fmt: "yuv420p".into(),
            extra_args: Vec::new(),
        }
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
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=index,codec_type,start_time:format=duration",
            ])
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
        let mut types: Vec<(usize, String)> = Vec::new();
        let mut starts: Vec<(usize, f64)> = Vec::new();
        let mut info = ProbeInfo::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim_matches('"');
            if key == "format.duration" {
                info.duration = value.parse().ok();
            } else if let Some(rest) = key.strip_prefix("streams.stream.") {
                let Some((n, field)) = rest.split_once('.') else {
                    continue;
                };
                let Ok(n) = n.parse::<usize>() else { continue };
                match field {
                    "codec_type" => types.push((n, value.to_owned())),
                    "start_time" => {
                        if let Ok(v) = value.parse() {
                            starts.push((n, v));
                        }
                    }
                    _ => {}
                }
            }
        }
        let start_of = |n: usize| starts.iter().find(|(i, _)| *i == n).map(|(_, v)| *v);
        for (n, t) in &types {
            match t.as_str() {
                "video" if !info.has_video => {
                    info.has_video = true;
                    info.video_start = start_of(*n);
                }
                "audio" => {
                    if info.audio_streams == 0 {
                        info.audio_start = start_of(*n);
                    }
                    info.audio_streams += 1;
                }
                _ => {}
            }
        }
        Ok(info)
    }

    /// 入力の最初の映像ストリームを AV1 の IVF にエンコードする。使ったエンコーダ名を返す。
    pub fn encode_video(
        &self,
        input: &Path,
        output: &Path,
        opts: &VideoEncodeOptions,
    ) -> Result<String> {
        let encoder = self.pick_av1_encoder(opts.encoder.as_deref())?;
        let mut args: Vec<OsString> = Vec::new();
        args.extend(["-i".into(), input.as_os_str().to_owned()]);
        args.extend(strs(&[
            "-map",
            "0:v:0",
            "-an",
            "-sn",
            "-dn",
            "-map_metadata",
            "-1",
        ]));
        args.extend(strs(&["-pix_fmt", &opts.pix_fmt, "-c:v", &encoder]));
        let crf = opts.crf.min(63);
        match encoder.as_str() {
            "libsvtav1" => {
                args.extend(strs(&["-crf", &crf.to_string()]));
                args.extend(strs(&["-preset", &opts.preset.unwrap_or(8).to_string()]));
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
        args.extend(strs(&["-f", "ivf"]));
        args.push(output.as_os_str().to_owned());
        self.run(&args)?;
        Ok(encoder)
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
