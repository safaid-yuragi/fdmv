//! 素材の取り込みとプレビュー用プロキシ。
//!
//! - 映像: 高さ 360 px の AV1 を、映像だけの .fdmv にする（キーフレームを短い間隔で入れ、シークを軽くする）
//! - 音声: 48 kHz ステレオの 16 bit WAV。映像の先頭（素材上の時刻 0）にそろえる
//!
//! 音声プロキシは非可逆圧縮をしていないので、書き出し時のミックスにもそのまま使う。

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, bail};
use libfdmv::ffmpeg::{Ffmpeg, VideoEncodeOptions};
use libfdmv::pack::{PackOptions, pack_ivf};

use crate::project::{Source, SourceVideo};

/// 素材を調べて [`Source`] を作る（ID は未割り当て）。
pub fn probe_source(ff: &Ffmpeg, path: &Path) -> Result<Source> {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let info = ff.probe(&path)?;
    let duration = info
        .duration
        .filter(|d| *d > 0.0)
        .with_context(|| format!("{}: unknown duration", path.display()))?;
    if !info.has_video && info.audio_streams == 0 {
        bail!("{}: no video or audio stream", path.display());
    }
    let video = if info.has_video {
        let (fps_num, fps_den) = info.frame_rate.unwrap_or((30, 1));
        Some(SourceVideo {
            width: info.width.unwrap_or(1920),
            height: info.height.unwrap_or(1080),
            fps_num,
            fps_den,
        })
    } else {
        None
    };
    let audio_offset = if info.has_video {
        info.audio_offset()
    } else {
        0.0
    };
    // 映像の素材では、素材上の時刻 0 = 映像の先頭。長さもそこから数える。
    let video_ss_offset = match (info.video_start, info.start) {
        (Some(v), Some(f)) if info.has_video => (v - f).max(0.0),
        _ => 0.0,
    };
    Ok(Source {
        id: 0,
        path,
        relative_path: None,
        duration: (duration - video_ss_offset).max(0.0),
        video,
        has_audio: info.audio_streams > 0,
        audio_offset,
        video_ss_offset,
    })
}

#[derive(Clone, Debug)]
pub struct ProxyStore {
    dir: PathBuf,
}

impl ProxyStore {
    /// OS のキャッシュディレクトリ（例: `~/.cache/fdmv-editor/proxies`）。
    pub fn default_dir() -> PathBuf {
        dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("fdmv-editor")
            .join("proxies")
    }

    pub fn new(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(ProxyStore { dir })
    }

    /// 素材のパス・サイズ・更新時刻から決まるキー。素材が変われば作り直される。
    fn key(&self, src: &Source) -> String {
        let meta = std::fs::metadata(&src.path).ok();
        let len = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let mtime = meta
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        // FNV-1a（Rust のバージョンに依存しない安定したハッシュ）
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let text = format!("{}\0{len}\0{mtime}", src.path.display());
        for b in text.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        format!("{h:016x}")
    }

    pub fn video_path(&self, src: &Source) -> Option<PathBuf> {
        src.video
            .as_ref()
            .map(|_| self.dir.join(format!("{}.fdmv", self.key(src))))
    }

    pub fn audio_path(&self, src: &Source) -> Option<PathBuf> {
        src.has_audio
            .then(|| self.dir.join(format!("{}.wav", self.key(src))))
    }

    pub fn is_ready(&self, src: &Source) -> bool {
        self.video_path(src).is_none_or(|p| p.exists())
            && self.audio_path(src).is_none_or(|p| p.exists())
    }

    /// プロキシを作る（既にあるものは作らない）。進捗は 0.0–1.0。
    pub fn build(
        &self,
        ff: &Ffmpeg,
        src: &Source,
        progress: &mut dyn FnMut(f64),
        cancel: &AtomicBool,
    ) -> Result<()> {
        let tmp = tempfile::tempdir_in(&self.dir)?;
        let has_both = self.video_path(src).is_some() && self.audio_path(src).is_some();
        let (vw, aw) = if has_both { (0.85, 0.15) } else { (1.0, 1.0) };

        if let Some(out) = self.video_path(src)
            && !out.exists()
        {
            let encoder = ff.pick_av1_encoder(None)?;
            let preset = match encoder.as_str() {
                "libsvtav1" => 12,
                "libaom-av1" => 8,
                _ => 10,
            };
            let opts = VideoEncodeOptions {
                encoder: Some(encoder),
                crf: 45,
                preset: Some(preset),
                pix_fmt: "yuv420p".into(),
                extra_args: vec!["-g".into(), "15".into()],
            };
            // 最初のフレームが時刻 0 になる（pack_ivf が先頭にそろえる）。
            let input: Vec<OsString> = vec!["-i".into(), src.path.clone().into()];
            let ivf = tmp.path().join("proxy.ivf");
            let encoder = ff.encode_video_complex(
                &input,
                "[0:v:0]scale=-2:'min(360,ih)':flags=bilinear,format=yuv420p[v]",
                "[v]",
                &ivf,
                &opts,
                src.duration,
                &mut |p| progress(p * vw),
                Some(cancel),
            )?;
            let partial = tmp.path().join("proxy.fdmv");
            pack_ivf(ff, &ivf, &encoder, &[], &PackOptions::default(), &partial)?;
            std::fs::rename(&partial, &out)?;
        }

        if let Some(out) = self.audio_path(src)
            && !out.exists()
        {
            let base = 1.0 - aw;
            let mut filters = Vec::new();
            if src.audio_offset > 0.0 {
                filters.push(format!(
                    "adelay=delays={}:all=1",
                    (src.audio_offset * 1000.0).round() as i64
                ));
            } else if src.audio_offset < 0.0 {
                filters.push(format!(
                    "atrim=start={:.6},asetpts=PTS-STARTPTS",
                    -src.audio_offset
                ));
            }
            filters.push("aresample=48000".into());
            let partial = tmp.path().join("proxy.wav");
            let args: Vec<OsString> = vec![
                "-i".into(),
                src.path.clone().into(),
                "-map".into(),
                "0:a:0".into(),
                "-vn".into(),
                "-af".into(),
                filters.join(",").into(),
                "-ac".into(),
                "2".into(),
                "-c:a".into(),
                "pcm_s16le".into(),
                "-rf64".into(),
                "auto".into(),
                "-f".into(),
                "wav".into(),
                partial.clone().into(),
            ];
            ff.run_with_progress(
                &args,
                src.duration,
                &mut |p| progress(base + p * aw),
                Some(cancel),
            )?;
            std::fs::rename(&partial, &out)?;
        }
        progress(1.0);
        Ok(())
    }
}
