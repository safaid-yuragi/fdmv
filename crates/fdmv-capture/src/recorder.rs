//! 録画セッション: 映像のエンコーダとチェーンごとの音声の取り込みをまとめて動かす。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use libfdmv::ChainRole;
use libfdmv::ffmpeg::{AudioEncodeOptions, Ffmpeg, Quality, VideoEncodeOptions};

use crate::audio::{self, AudioApp, Capture, MicDevice};
use crate::chain::Chain;
use crate::codecs;
use crate::encoder::{EncoderStats, VideoEncoder};
use crate::finalize::{self, ChainParts, FinalizeJob, FinalizeReport, Stage};
use crate::video::FrameSlot;

/// 録画開始の操作から映像の 0 秒までの猶予。
const START_DELAY: Duration = Duration::from_millis(400);

#[derive(Clone, Debug, PartialEq)]
pub enum AudioSource {
    App(AudioApp),
    Mic(MicDevice),
}

#[derive(Clone, Debug)]
pub struct ChainSetup {
    pub name: String,
    /// メインチェーン（FDMV のデフォルトチェーン）にするか。1 本まで。
    pub main: bool,
    pub source: AudioSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoMode {
    /// 録画しながら最終的な画質でエンコードする。停止後すぐにできあがる。
    /// （GPU の HEVC / H.264 で録るときは、これを選んでも停止後に AV1 へ変換する。）
    Direct,
    /// 録画中は高画質・高速の設定で一時保存し、停止後に再エンコードして小さくする。
    Reencode,
}

#[derive(Clone, Debug)]
pub struct RecordOptions {
    pub fps: u32,
    pub quality: Quality,
    pub mode: VideoMode,
    /// 録画中に使う映像エンコーダ（[`codecs::available`] の名前）。None ならおすすめを自動で選ぶ。
    pub encoder: Option<String>,
    /// これより縦が大きい画面は縮小する。
    pub max_height: Option<u32>,
    pub audio_bitrate: String,
    pub title: Option<String>,
}

impl Default for RecordOptions {
    fn default() -> Self {
        RecordOptions {
            fps: 30,
            quality: Quality::High,
            mode: VideoMode::Direct,
            encoder: None,
            max_height: None,
            audio_bitrate: "128k".into(),
            title: None,
        }
    }
}

impl RecordOptions {
    /// 停止後に AV1 へ変換するときの設定（`encoder` = ソフトウェアの AV1 エンコーダ）。
    /// 長い録画でも待ち時間が長くなりすぎないよう、速度プリセットは 8 より遅くしない（最高画質を除く）。
    pub fn final_video_options(&self, encoder: &str, fps: u32) -> VideoEncodeOptions {
        let (crf, preset) = self.quality.crf_preset();
        let preset = if self.quality == Quality::Best {
            preset
        } else {
            preset.max(8)
        };
        VideoEncodeOptions {
            encoder: Some(encoder.to_owned()),
            crf,
            preset: Some(preset),
            extra_args: [
                "-fps_mode",
                "vfr",
                "-enc_time_base",
                &format!("1:{fps}"),
                "-g",
                &(fps * 5).to_string(),
                "-colorspace",
                "bt709",
                "-color_primaries",
                "bt709",
                "-color_trc",
                "bt709",
                "-color_range",
                "tv",
            ]
            .map(String::from)
            .to_vec(),
            ..Default::default()
        }
    }
}

/// 録画中のチェーンの様子。
#[derive(Clone, Debug)]
pub struct ChainStatus {
    pub name: String,
    pub main: bool,
    /// 取り込みを続けているか（途中で外したチェーンは false）。
    pub capturing: bool,
    /// いま音声が届いているか。
    pub receiving: bool,
    /// 前回の問い合わせからの最大振幅。
    pub peak: f32,
    pub error: Option<String>,
}

struct RecChain {
    chain: Arc<Chain>,
    capture: Option<Box<dyn Capture>>,
}

pub struct Recorder {
    epoch: Instant,
    workdir: PathBuf,
    output: PathBuf,
    /// 録画中の映像の書き出し先（AV1 なら IVF、それ以外は Matroska）。
    video: PathBuf,
    /// 停止後に AV1 へ変換するか。
    convert: bool,
    chains: Vec<RecChain>,
    encoder: Option<VideoEncoder>,
    opts: RecordOptions,
}

impl Recorder {
    /// 録画を始める。`slot` = 映像の取り込みの置き場（最初のフレームの大きさで録る）。
    /// 中間ファイルは `output` と同じフォルダの隠しフォルダに置く。
    pub fn start(
        ff: &Ffmpeg,
        slot: Arc<FrameSlot>,
        setups: Vec<ChainSetup>,
        opts: RecordOptions,
        output: PathBuf,
    ) -> Result<Self> {
        if setups.iter().filter(|s| s.main).count() > 1 {
            bail!("メインチェーンは 1 本だけ選べます");
        }
        let live = codecs::pick(ff, opts.encoder.as_deref())?;
        // 停止後に変換するか（GPU の HEVC などは必ず変換する）。
        let convert = live.needs_conversion() || opts.mode == VideoMode::Reencode;
        // 最初のフレームを待つ（画面に変化が無いと届かないこともあるので少し待つ）。
        let deadline = Instant::now() + Duration::from_secs(5);
        let frame = loop {
            if let (_, Some(f)) = slot.latest() {
                break f;
            }
            if Instant::now() > deadline {
                bail!("画面の映像が届きません");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let size = (frame.width & !1, frame.height & !1);

        let workdir = work_dir_for(&output)?;
        let video = workdir.join(if live.is_av1() {
            "video.ivf"
        } else {
            "video.mkv"
        });
        // 映像の 0 秒は少し先にして、そのあいだに音声の取り込みを立ち上げる
        // （アプリの一覧の取得などに時間がかかり、録画の頭の音を取りこぼさないように）。
        let epoch = Instant::now() + START_DELAY;
        let encoder = VideoEncoder::start(
            ff,
            slot,
            size,
            opts.fps,
            opts.max_height,
            &live,
            opts.quality,
            convert,
            &video,
            epoch,
        );
        let encoder = match encoder {
            Ok(e) => e,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&workdir);
                return Err(e);
            }
        };
        let mut rec = Recorder {
            epoch,
            workdir,
            output,
            video,
            convert,
            chains: Vec::new(),
            encoder: Some(encoder),
            opts,
        };
        for s in setups {
            if let Err(e) = rec.add_chain(s.clone()) {
                rec.abort();
                return Err(e.context(format!("「{}」を録音できません", s.name)));
            }
        }
        Ok(rec)
    }

    /// 録画中にチェーンを足す（そのとき以降の音声が入る）。
    pub fn add_chain(&mut self, setup: ChainSetup) -> Result<()> {
        if setup.main
            && self
                .chains
                .iter()
                .any(|c| c.chain.role == ChainRole::Default)
        {
            bail!("メインチェーンは 1 本だけ選べます");
        }
        if self.chains.iter().any(|c| c.chain.name == setup.name) {
            bail!("チェーン「{}」はもうあります", setup.name);
        }
        let description = match &setup.source {
            AudioSource::App(a) => format!("録音元: {}", a.name),
            AudioSource::Mic(m) => format!("録音元: マイク（{}）", m.name),
        };
        let role = if setup.main {
            ChainRole::Default
        } else {
            ChainRole::Sub
        };
        let dir = self.workdir.join(format!("chain{}", self.chains.len()));
        let chain = Chain::new(
            setup.name,
            role,
            vec![("description".into(), description)],
            dir,
            self.epoch,
        )?;
        let capture = match &setup.source {
            AudioSource::App(app) => audio::capture_app(app, chain.clone())?,
            AudioSource::Mic(dev) => audio::capture_mic(dev, chain.clone())?,
        };
        self.chains.push(RecChain {
            chain,
            capture: Some(capture),
        });
        Ok(())
    }

    /// チェーンの取り込みをやめる（それまでの音声は残る）。
    pub fn stop_chain(&mut self, index: usize) {
        if let Some(c) = self.chains.get_mut(index) {
            c.capture = None;
        }
    }

    /// 録画した長さ（開始直後の準備中は 0）。
    pub fn elapsed(&self) -> Duration {
        Instant::now().saturating_duration_since(self.epoch)
    }

    pub fn video_stats(&self) -> Arc<EncoderStats> {
        self.encoder.as_ref().unwrap().stats().clone()
    }

    pub fn chain_status(&self) -> Vec<ChainStatus> {
        self.chains
            .iter()
            .map(|c| ChainStatus {
                name: c.chain.name.clone(),
                main: c.chain.role == ChainRole::Default,
                capturing: c.capture.is_some(),
                receiving: c.chain.is_receiving(),
                peak: c.chain.take_peak(),
                error: c
                    .chain
                    .error()
                    .or_else(|| c.capture.as_ref().and_then(|cap| cap.error())),
            })
            .collect()
    }

    /// 録画を止める。できあがるのは中間ファイルで、[`Recording::finalize`] で FDMV にする。
    pub fn stop(mut self) -> Result<Recording> {
        let duration = self.elapsed();
        // 先に映像の終わりを決める（音声の取り込みを止めるあいだに映像が延びないように）。
        let encoder = self.encoder.take().unwrap();
        encoder.stop_at(duration);
        for c in &mut self.chains {
            c.capture = None;
            c.chain.stop();
        }
        let encoder_name = encoder.encoder.name.clone();
        let fps = encoder.fps;
        let video_duration = match encoder.finish(duration) {
            Ok(d) => d,
            Err(e) => {
                self.abort();
                return Err(e);
            }
        };
        let chains = self
            .chains
            .iter()
            .map(|c| ChainParts {
                name: c.chain.name.clone(),
                role: c.chain.role,
                meta: c.chain.meta.clone(),
                parts: c.chain.parts(),
            })
            .collect();
        Ok(Recording {
            workdir: std::mem::take(&mut self.workdir),
            output: std::mem::take(&mut self.output),
            video: std::mem::take(&mut self.video),
            convert: self.convert,
            encoder: encoder_name,
            fps,
            duration: video_duration,
            chains,
            opts: self.opts.clone(),
        })
    }

    /// 録画を捨てる。
    pub fn abort(&mut self) {
        for c in &mut self.chains {
            c.capture = None;
        }
        self.encoder = None;
        if !self.workdir.as_os_str().is_empty() {
            let _ = std::fs::remove_dir_all(&self.workdir);
            self.workdir = PathBuf::new();
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        // stop() されずに捨てられた場合。
        if self.encoder.is_some() {
            self.abort();
        }
    }
}

/// 停止した録画（FDMV に変換する前）。
pub struct Recording {
    workdir: PathBuf,
    pub output: PathBuf,
    video: PathBuf,
    convert: bool,
    /// 録画に使ったエンコーダ。
    encoder: String,
    fps: u32,
    /// 映像の長さ（秒）。
    pub duration: f64,
    pub chains: Vec<ChainParts>,
    opts: RecordOptions,
}

impl Recording {
    /// FDMV に変換する。成功したら中間ファイルを消す。
    pub fn finalize(
        &self,
        ff: &Ffmpeg,
        progress: &mut dyn FnMut(Stage, f64),
        cancel: &AtomicBool,
    ) -> Result<FinalizeReport> {
        let reencode = if self.convert {
            let software = ff.pick_av1_encoder(None)?;
            Some(self.opts.final_video_options(&software, self.fps))
        } else {
            None
        };
        let mut file_meta = Vec::new();
        if let Some(t) = self.opts.title.as_ref().filter(|t| !t.is_empty()) {
            file_meta.push(("title".to_owned(), t.clone()));
        }
        let job = FinalizeJob {
            video: &self.video,
            encoder: &self.encoder,
            duration: self.duration,
            chains: &self.chains,
            reencode: reencode.as_ref(),
            audio: &AudioEncodeOptions {
                bitrate: self.opts.audio_bitrate.clone(),
                channels: 2,
            },
            file_meta,
            workdir: &self.workdir,
            output: &self.output,
        };
        let report = finalize::finalize(ff, &job, progress, cancel)?;
        let _ = std::fs::remove_dir_all(&self.workdir);
        Ok(report)
    }

    /// 中間ファイルを消す。
    pub fn discard(self) {
        let _ = std::fs::remove_dir_all(&self.workdir);
    }

    /// 中間ファイルの場所。
    pub fn workdir(&self) -> &Path {
        &self.workdir
    }
}

/// `output` の隣に中間ファイル用の隠しフォルダを作る（/tmp は RAM のことがあり、長い録画に向かない）。
fn work_dir_for(output: &Path) -> Result<PathBuf> {
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let stem = output
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "recording".into());
    for n in 0..1000 {
        let dir = parent.join(format!(
            ".{stem}.fdmvrec{}",
            if n == 0 { String::new() } else { n.to_string() }
        ));
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(e).with_context(|| format!("{} を作れません", dir.display()));
            }
        }
    }
    bail!("中間ファイルのフォルダを作れません")
}
