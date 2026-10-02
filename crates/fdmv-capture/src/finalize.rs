//! 停止後の変換: パートのミックス → 無音で区切ったセグメント → FDMV。

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use libfdmv::ChainRole;
use libfdmv::ffmpeg::{AudioEncodeOptions, Ffmpeg, VideoEncodeOptions};
use libfdmv::pack::{ChainSpec, PackOptions, PackReport, SegmentSource, pack_ivf};

use crate::SAMPLE_RATE;
use crate::chain::Part;

/// これより小さい振幅は無音とみなす（-80 dBFS）。
const SILENCE: f32 = 1e-4;
/// 無音がこれ以上続いたらセグメントを切る（秒）。
const SPLIT_SILENCE_SECONDS: f64 = 2.0;
/// セグメントの後ろに残す無音（秒）。
const TAIL_SECONDS: f64 = 0.1;
/// 処理の単位（フレーム）。
const BLOCK: usize = 960;

/// ミックス前のチェーン。
#[derive(Clone, Debug)]
pub struct ChainParts {
    pub name: String,
    pub role: ChainRole,
    pub meta: Vec<(String, String)>,
    pub parts: Vec<Part>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Stage {
    /// 音声のミックスと分割。
    Audio,
    /// 映像の再エンコード（[`crate::VideoMode::Reencode`] のときだけ）。
    Video,
    /// Opus へのエンコードと FDMV へのまとめ。
    Pack,
}

pub struct FinalizeJob<'a> {
    /// 録画した映像（AV1 の IVF。`reencode` があれば ffmpeg が読める任意の形式）。
    pub video: &'a Path,
    pub encoder: &'a str,
    /// 録画の長さ（秒）。これより後の音声は切り捨てる。
    pub duration: f64,
    pub chains: &'a [ChainParts],
    /// Some なら映像をこの設定で再エンコードする。
    pub reencode: Option<&'a VideoEncodeOptions>,
    pub audio: &'a AudioEncodeOptions,
    pub file_meta: Vec<(String, String)>,
    /// 中間ファイルを置く場所。
    pub workdir: &'a Path,
    pub output: &'a Path,
}

#[derive(Debug, Default)]
pub struct FinalizeReport {
    pub pack: PackReport,
    /// 音声が 1 度も無かったため入れなかったチェーンの名前。
    pub empty_chains: Vec<String>,
}

pub fn finalize(
    ff: &Ffmpeg,
    job: &FinalizeJob,
    progress: &mut dyn FnMut(Stage, f64),
    cancel: &AtomicBool,
) -> Result<FinalizeReport> {
    let mut report = FinalizeReport::default();
    let total_frames = (job.duration * SAMPLE_RATE as f64).round() as i64;
    let mut specs = Vec::new();
    for (ci, chain) in job.chains.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            bail!("キャンセルしました");
        }
        progress(Stage::Audio, ci as f64 / job.chains.len() as f64);
        let dir = job.workdir.join(format!("mix{ci}"));
        std::fs::create_dir_all(&dir)?;
        let parts = normalize_parts(ff, &chain.parts, &dir)?;
        let segments = mix_chain(&parts, total_frames, &dir)
            .with_context(|| format!("チェーン「{}」のミックス", chain.name))?;
        if segments.is_empty() {
            report.empty_chains.push(chain.name.clone());
            continue;
        }
        let mut spec = ChainSpec::new(chain.name.clone(), chain.role);
        spec.meta = chain.meta.clone();
        spec.segments = segments
            .into_iter()
            .map(|(path, start)| SegmentSource::new(path, start as f64 / SAMPLE_RATE as f64))
            .collect();
        specs.push(spec);
    }
    progress(Stage::Audio, 1.0);

    let mut ivf = job.video.to_path_buf();
    let mut encoder = job.encoder.to_owned();
    if let Some(opts) = job.reencode {
        let out = job.workdir.join("final.ivf");
        let input = ["-i".into(), job.video.as_os_str().to_owned()];
        encoder = ff.encode_video_complex(
            &input,
            "[0:v]null[v]",
            "[v]",
            &out,
            opts,
            job.duration,
            &mut |p| progress(Stage::Video, p),
            Some(cancel),
        )?;
        ivf = out;
    }

    if cancel.load(Ordering::Relaxed) {
        bail!("キャンセルしました");
    }
    progress(Stage::Pack, 0.0);
    let opts = PackOptions {
        audio: job.audio.clone(),
        file_meta: job.file_meta.clone(),
        ..Default::default()
    };
    report.pack = pack_ivf(ff, &ivf, &encoder, &specs, &opts, job.output)?;
    progress(Stage::Pack, 1.0);
    Ok(report)
}

/// 48 kHz ステレオでないパートを ffmpeg で変換する。
fn normalize_parts(ff: &Ffmpeg, parts: &[Part], dir: &Path) -> Result<Vec<Part>> {
    let mut out = Vec::with_capacity(parts.len());
    for (i, p) in parts.iter().enumerate() {
        if p.rate == SAMPLE_RATE && p.channels == 2 {
            out.push(p.clone());
            continue;
        }
        let path = dir.join(format!("conv{i}.f32"));
        let args: Vec<std::ffi::OsString> = vec![
            "-f".into(),
            "f32le".into(),
            "-ar".into(),
            p.rate.to_string().into(),
            "-ac".into(),
            p.channels.to_string().into(),
            "-i".into(),
            p.path.clone().into(),
            "-f".into(),
            "f32le".into(),
            "-ar".into(),
            SAMPLE_RATE.to_string().into(),
            "-ac".into(),
            "2".into(),
            path.clone().into(),
        ];
        ff.run(&args)?;
        let frames = std::fs::metadata(&path)?.len() / 8;
        out.push(Part {
            path,
            start: p.start,
            rate: SAMPLE_RATE,
            channels: 2,
            frames,
        });
    }
    Ok(out)
}

/// ステレオ 48 kHz のパートを読む。
struct PartReader {
    /// タイムライン上の開始位置（フレーム）。負もありうる。
    start: i64,
    frames: i64,
    r: BufReader<File>,
    /// 次に読むフレーム（パートの先頭から）。
    pos: i64,
    bytes: Vec<u8>,
}

impl PartReader {
    fn end(&self) -> i64 {
        self.start + self.frames
    }

    /// タイムラインの [from, from + out.len()/2) と重なる部分を `out` に足す。
    fn add_to(&mut self, from: i64, out: &mut [f32]) -> Result<()> {
        let n = out.len() as i64 / 2;
        let a = from.max(self.start);
        let b = (from + n).min(self.end());
        if a >= b {
            return Ok(());
        }
        let local = a - self.start;
        if local != self.pos {
            self.r.seek(SeekFrom::Start(local as u64 * 8))?;
        }
        let count = (b - a) as usize;
        self.bytes.resize(count * 8, 0);
        self.r.read_exact(&mut self.bytes)?;
        self.pos = local + count as i64;
        let off = (a - from) as usize * 2;
        for (o, c) in out[off..off + count * 2]
            .iter_mut()
            .zip(self.bytes.chunks_exact(4))
        {
            *o += f32::from_le_bytes(c.try_into().unwrap());
        }
        Ok(())
    }
}

/// パートをミックスし、無音で区切って WAV に書く。(WAV のパス, 開始フレーム) の並びを返す。
pub fn mix_chain(parts: &[Part], total_frames: i64, dir: &Path) -> Result<Vec<(PathBuf, i64)>> {
    let mut readers = Vec::new();
    for p in parts {
        debug_assert!(p.rate == SAMPLE_RATE && p.channels == 2);
        let start = (p.start * SAMPLE_RATE as f64).round() as i64;
        let frames = (std::fs::metadata(&p.path)?.len() / 8).min(p.frames) as i64;
        if frames == 0 || start + frames <= 0 || start >= total_frames {
            continue;
        }
        readers.push(PartReader {
            start,
            frames,
            r: BufReader::with_capacity(1 << 16, File::open(&p.path)?),
            pos: 0,
            bytes: Vec::new(),
        });
    }
    readers.sort_by_key(|r| r.start);

    let split_blocks = (SPLIT_SILENCE_SECONDS * SAMPLE_RATE as f64 / BLOCK as f64).ceil() as usize;
    let tail_blocks = (TAIL_SECONDS * SAMPLE_RATE as f64 / BLOCK as f64).ceil() as usize;
    let mut out = Vec::new();
    let mut seg: Option<SegmentWriter> = None;
    // セグメント中に続いている無音のブロック（まだ書いていない）。
    let mut pending: Vec<Vec<f32>> = Vec::new();
    let mut buf = vec![0.0f32; BLOCK * 2];
    let mut pos = 0i64;
    while pos < total_frames {
        let n = (total_frames - pos).min(BLOCK as i64) as usize;
        let block = &mut buf[..n * 2];
        let overlapping = readers
            .iter()
            .any(|r| r.start < pos + n as i64 && r.end() > pos);
        if !overlapping && seg.is_none() {
            // 次のパートまで飛ばす。
            match readers.iter().map(|r| r.start).find(|&s| s > pos) {
                Some(next) if next < total_frames => {
                    pos = next;
                    continue;
                }
                _ => break,
            }
        }
        block.fill(0.0);
        for r in readers.iter_mut() {
            r.add_to(pos, block)?;
        }
        for s in block.iter_mut() {
            *s = s.clamp(-1.0, 1.0);
        }
        let silent = block.iter().all(|s| s.abs() < SILENCE);
        match (&mut seg, silent) {
            (None, true) => {}
            (None, false) => {
                let path = dir.join(format!("seg{:04}.wav", out.len()));
                let mut w = SegmentWriter::create(&path)?;
                w.write(block)?;
                out.push((path, pos));
                seg = Some(w);
            }
            (Some(w), false) => {
                for p in pending.drain(..) {
                    w.write(&p)?;
                }
                w.write(block)?;
            }
            (Some(_), true) => {
                pending.push(block.to_vec());
                if pending.len() >= split_blocks {
                    let mut w = seg.take().unwrap();
                    for p in pending.drain(..).take(tail_blocks) {
                        w.write(&p)?;
                    }
                    w.finish()?;
                }
            }
        }
        pos += n as i64;
    }
    if let Some(mut w) = seg {
        for p in pending.drain(..).take(tail_blocks) {
            w.write(&p)?;
        }
        w.finish()?;
    }
    Ok(out)
}

/// 32 bit float ステレオ 48 kHz の WAV。長さは閉じるときにヘッダへ書き戻す。
struct SegmentWriter {
    w: BufWriter<File>,
    frames: u64,
}

impl SegmentWriter {
    fn create(path: &Path) -> Result<Self> {
        let mut w = BufWriter::with_capacity(1 << 16, File::create(path)?);
        w.write_all(&wav_header(0))?;
        Ok(SegmentWriter { w, frames: 0 })
    }

    fn write(&mut self, samples: &[f32]) -> Result<()> {
        for s in samples {
            self.w.write_all(&s.to_le_bytes())?;
        }
        self.frames += samples.len() as u64 / 2;
        Ok(())
    }

    fn finish(mut self) -> Result<()> {
        self.w.flush()?;
        let mut f = self.w.into_inner().map_err(|e| e.into_error())?;
        f.seek(SeekFrom::Start(0))?;
        f.write_all(&wav_header(self.frames))?;
        Ok(())
    }
}

fn wav_header(frames: u64) -> [u8; 44] {
    let channels = 2u16;
    let block_align = channels * 4;
    let data_len = u32::try_from(frames * block_align as u64).unwrap_or(u32::MAX - 36);
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(data_len + 36).to_le_bytes());
    h[8..16].copy_from_slice(b"WAVEfmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&3u16.to_le_bytes()); // IEEE float
    h[22..24].copy_from_slice(&channels.to_le_bytes());
    h[24..28].copy_from_slice(&SAMPLE_RATE.to_le_bytes());
    h[28..32].copy_from_slice(&(SAMPLE_RATE * block_align as u32).to_le_bytes());
    h[32..34].copy_from_slice(&block_align.to_le_bytes());
    h[34..36].copy_from_slice(&32u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_len.to_le_bytes());
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_part(dir: &Path, name: &str, start: f64, secs: f64, value: f32) -> Part {
        let frames = (secs * SAMPLE_RATE as f64) as u64;
        let path = dir.join(name);
        let mut bytes = Vec::new();
        for _ in 0..frames * 2 {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        std::fs::write(&path, bytes).unwrap();
        Part {
            path,
            start,
            rate: SAMPLE_RATE,
            channels: 2,
            frames,
        }
    }

    fn read_wav(path: &Path) -> Vec<f32> {
        let bytes = std::fs::read(path).unwrap();
        let len = u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize;
        assert_eq!(len, bytes.len() - 44);
        bytes[44..]
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
            .collect()
    }

    #[test]
    fn mixes_overlaps_and_splits_on_silence() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let parts = vec![
            // 録画開始前から始まっている
            write_part(d, "a", -0.5, 1.5, 0.25),
            // 重なる（ミックスされる）
            write_part(d, "b", 0.5, 1.0, 0.25),
            // 0.5 秒の隙間 → 同じセグメント
            write_part(d, "c", 2.0, 1.0, 0.1),
            // 3 秒の隙間 → 新しいセグメント。録画の終わりで切れる
            write_part(d, "d", 6.0, 10.0, 0.1),
        ];
        let segs = mix_chain(&parts, 8 * SAMPLE_RATE as i64, d).unwrap();
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].1, 0);
        let s0 = read_wav(&segs[0].0);
        // 0–3 秒（＋末尾の無音 0.1 秒、ブロック単位で切り上げ）
        let frames0 = s0.len() / 2;
        assert!(
            (3 * 48000..=3 * 48000 + 6000).contains(&frames0),
            "{frames0}"
        );
        assert_eq!(s0[0], 0.25);
        assert_eq!(s0[(48000 / 2 + 10) * 2], 0.5);
        assert_eq!(s0[(48000 * 3 / 2 + 10) * 2], 0.0);
        assert_eq!(s0[(48000 * 2 + 10) * 2], 0.1);
        assert_eq!(segs[1].1, 6 * 48000);
        let s1 = read_wav(&segs[1].0);
        assert_eq!(s1.len() / 2, 2 * 48000);
    }

    #[test]
    fn silent_chain_has_no_segments() {
        let dir = tempfile::tempdir().unwrap();
        let parts = vec![write_part(dir.path(), "a", 0.0, 1.0, 0.0)];
        assert!(mix_chain(&parts, 48000, dir.path()).unwrap().is_empty());
    }
}
