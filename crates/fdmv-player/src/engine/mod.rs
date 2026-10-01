//! 再生エンジン。UI から独立しており、音声をマスタークロックにして映像を同期させる。

mod audio;
mod video;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use eframe::egui;
use libfdmv::decode::ChainSelection;
use libfdmv::format::{OPUS_SAMPLE_RATE, Rational};
use libfdmv::{ChainRole, Directory, FdmvReader, Segment};

use audio::{AudioCommand, AudioEngine};
pub use video::Frame;
use video::VideoEngine;

pub struct ChainState {
    pub id: u16,
    pub name: String,
    pub is_default: bool,
    pub channels: u8,
    /// ファイルに記録された初期ゲイン（線形）。
    pub file_gain: f32,
    pub enabled: bool,
    /// ユーザが調整する音量（線形、1.0 = 等倍）。
    pub volume: f32,
    pub segments: Vec<Segment>,
    pub language: Option<String>,
    pub description: Option<String>,
}

impl ChainState {
    fn selection(&self) -> ChainSelection {
        ChainSelection {
            stream_id: self.id,
            gain: self.file_gain * self.volume,
        }
    }
}

pub struct Player {
    pub path: PathBuf,
    pub dir: Directory,
    pub chains: Vec<ChainState>,
    pub duration: f64,
    pub file_size: u64,
    audio: AudioEngine,
    video: VideoEngine,
}

impl Player {
    pub fn open(path: &Path, ctx: egui::Context) -> Result<Self> {
        let reader = FdmvReader::open(path).with_context(|| format!("{}", path.display()))?;
        let dir = reader.directory().clone();
        let file_size = reader.file_len();
        drop(reader);

        let chains: Vec<ChainState> = dir
            .chains()
            .filter_map(|s| {
                let c = s.chain()?;
                Some(ChainState {
                    id: s.id,
                    name: s.name.clone(),
                    is_default: c.role == ChainRole::Default,
                    channels: c.channels,
                    file_gain: c.gain_linear(),
                    // 通常再生ではデフォルトチェーンだけを鳴らす。
                    enabled: c.role == ChainRole::Default,
                    volume: 1.0,
                    segments: c.segments.clone(),
                    language: s.meta("language").map(str::to_owned),
                    description: s.meta("description").map(str::to_owned),
                })
            })
            .collect();
        let selections = chains
            .iter()
            .filter(|c| c.enabled)
            .map(|c| c.selection())
            .collect();
        let audio = AudioEngine::start(path, selections)?;
        let video = VideoEngine::start(path, ctx)?;
        let duration = dir.duration_seconds();
        let player = Player {
            path: path.to_owned(),
            dir,
            chains,
            duration,
            file_size,
            audio,
            video,
        };
        player.video.seek(0.0);
        Ok(player)
    }

    pub fn audio_device(&self) -> Option<&str> {
        self.audio.device_name()
    }

    /// 再生位置（秒）。
    pub fn position(&self) -> f64 {
        Rational::OPUS
            .to_seconds(self.audio.position())
            .min(self.duration)
    }

    pub fn is_playing(&self) -> bool {
        self.audio.is_playing()
    }

    pub fn play(&mut self) {
        if self.audio.finished() || self.position() >= self.duration {
            self.seek(0.0);
        }
        self.audio.set_playing(true);
    }

    pub fn pause(&mut self) {
        self.audio.set_playing(false);
    }

    pub fn toggle(&mut self) {
        if self.is_playing() {
            self.pause()
        } else {
            self.play()
        }
    }

    pub fn seek(&mut self, seconds: f64) {
        let t = seconds.clamp(0.0, self.duration);
        self.audio
            .seek((t * OPUS_SAMPLE_RATE as f64).round() as i64);
        self.video.seek(t);
    }

    /// 最後まで再生したら一時停止にする。毎フレーム呼ぶ。
    pub fn update(&mut self) {
        if self.is_playing() && self.audio.finished() {
            self.pause();
        }
    }

    pub fn set_master_volume(&self, v: f32) {
        self.audio.set_volume(v);
    }

    pub fn set_chain_enabled(&mut self, index: usize, enabled: bool) {
        let c = &mut self.chains[index];
        c.enabled = enabled;
        if enabled {
            self.audio.send(AudioCommand::AddChain(c.selection()));
        } else {
            self.audio.send(AudioCommand::RemoveChain(c.id));
        }
    }

    pub fn set_chain_volume(&mut self, index: usize, volume: f32) {
        let c = &mut self.chains[index];
        c.volume = volume;
        self.audio
            .send(AudioCommand::SetGain(c.id, c.file_gain * c.volume));
    }

    /// 現在の再生位置で表示すべき新しいフレーム。
    pub fn take_frame(&self) -> Option<Frame> {
        self.video.take_frame(self.position())
    }

    /// 次のフレームを表示するまでの秒数。
    pub fn time_to_next_frame(&self) -> Option<f64> {
        self.video
            .next_pts()
            .map(|p| (p - self.position()).max(0.0))
    }

    pub fn error(&self) -> Option<String> {
        self.video.error()
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::thread::sleep;
    use std::time::{Duration, Instant};

    use libfdmv::ChainRole;
    use libfdmv::ffmpeg::{Ffmpeg, VideoEncodeOptions};
    use libfdmv::pack::{ChainSpec, PackOptions, SegmentSource, pack};

    use super::*;

    fn make_file(dir: &Path) -> Option<PathBuf> {
        let ff = Ffmpeg::locate().ok()?;
        let src = dir.join("in.mkv");
        let ok = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
            ])
            .arg("testsrc2=size=160x90:rate=25")
            .args([
                "-f",
                "lavfi",
                "-i",
                "sine=f=440:r=48000",
                "-t",
                "3",
                "-c:v",
                "ffv1",
                "-c:a",
                "pcm_s16le",
            ])
            .arg(&src)
            .status()
            .ok()?
            .success();
        assert!(ok);
        let mut main = ChainSpec::new("main", ChainRole::Default);
        main.segments.push(SegmentSource::new(&src, 0.0));
        let mut sub = ChainSpec::new("sub", ChainRole::Sub);
        sub.segments.push(SegmentSource::new(&src, 1.0));
        let opts = PackOptions {
            video: VideoEncodeOptions {
                preset: Some(12),
                crf: 45,
                ..Default::default()
            },
            ..Default::default()
        };
        let out = dir.join("t.fdmv");
        pack(&ff, &src, &[main, sub], &opts, &out).unwrap();
        Some(out)
    }

    fn wait_frame(p: &Player) -> Frame {
        let t0 = Instant::now();
        loop {
            if let Some(f) = p.take_frame() {
                return f;
            }
            assert!(t0.elapsed() < Duration::from_secs(5), "no frame");
            sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn playback_seek_and_end() {
        // SAFETY: テスト内でのみ設定し、他のスレッドはまだ環境変数を読んでいない。
        unsafe { std::env::set_var("FDMV_NO_AUDIO", "1") };
        let tmp = tempfile::tempdir().unwrap();
        let Some(path) = make_file(tmp.path()) else {
            eprintln!("skipping: ffmpeg not available");
            return;
        };
        let mut p = Player::open(&path, egui::Context::default()).unwrap();
        assert_eq!(p.chains.len(), 2);
        assert!(p.chains[0].enabled && !p.chains[1].enabled);

        // 停止中は最初のフレームが出て、時刻は進まない
        let f = wait_frame(&p);
        assert_eq!(f.pts, 0.0);
        sleep(Duration::from_millis(100));
        assert_eq!(p.position(), 0.0);

        // 再生すると実時間で進む
        p.play();
        sleep(Duration::from_millis(400));
        let pos = p.position();
        assert!((0.25..0.6).contains(&pos), "position {pos}");

        // シークは即座に反映され、その時刻のフレームが出る
        p.pause();
        p.seek(2.0);
        assert!((p.position() - 2.0).abs() < 1e-6);
        let f = wait_frame(&p);
        assert!((f.pts - 2.0).abs() < 0.041, "frame {}", f.pts);

        // チェーンの切り替え
        p.set_chain_enabled(1, true);
        p.set_chain_volume(1, 0.5);
        p.set_chain_enabled(0, false);

        // 最後まで再生すると一時停止になる
        p.seek(2.8);
        p.play();
        let t0 = Instant::now();
        while p.is_playing() {
            p.update();
            assert!(t0.elapsed() < Duration::from_secs(3), "did not stop at end");
            sleep(Duration::from_millis(10));
        }
        assert!(
            (p.position() - 3.0).abs() < 0.05,
            "end position {}",
            p.position()
        );
        // 終端で再生すると先頭に戻る
        p.play();
        assert!(p.position() < 0.1);
    }
}
