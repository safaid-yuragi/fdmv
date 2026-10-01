//! WAV（16 bit / 32 bit float、RF64 を含む）の任意位置読み出し。

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result, bail};

pub struct WavFile {
    file: File,
    data_offset: u64,
    frames: u64,
    channels: usize,
    float: bool,
    buf: Vec<u8>,
}

impl WavFile {
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let file_len = file.metadata()?.len();
        let mut h = [0u8; 12];
        file.read_exact(&mut h)?;
        if !(&h[0..4] == b"RIFF" || &h[0..4] == b"RF64") || &h[8..12] != b"WAVE" {
            bail!("{}: not a WAV file", path.display());
        }
        let (mut channels, mut bits, mut fmt_tag, mut rate) = (0usize, 0u16, 0u16, 0u32);
        let mut pos = 12u64;
        loop {
            let mut ch = [0u8; 8];
            file.seek(SeekFrom::Start(pos))?;
            if file.read_exact(&mut ch).is_err() {
                bail!("{}: no data chunk", path.display());
            }
            let size = u32::from_le_bytes(ch[4..8].try_into().unwrap()) as u64;
            match &ch[0..4] {
                b"fmt " => {
                    let mut f = [0u8; 16];
                    file.read_exact(&mut f)?;
                    fmt_tag = u16::from_le_bytes([f[0], f[1]]);
                    channels = u16::from_le_bytes([f[2], f[3]]) as usize;
                    rate = u32::from_le_bytes(f[4..8].try_into().unwrap());
                    bits = u16::from_le_bytes([f[14], f[15]]);
                    if fmt_tag == 0xFFFE && size >= 26 {
                        // WAVE_FORMAT_EXTENSIBLE: サブフォーマットの先頭 2 バイトが実際の形式。
                        let mut ext = [0u8; 10];
                        file.read_exact(&mut ext)?;
                        fmt_tag = u16::from_le_bytes([ext[8], ext[9]]);
                    }
                }
                b"data" => {
                    let data_offset = pos + 8;
                    // RF64 や 4 GiB 超えではサイズが 0xFFFFFFFF になるので、ファイル末尾まで読む。
                    let len = if size == u32::MAX as u64 {
                        file_len - data_offset
                    } else {
                        size.min(file_len - data_offset)
                    };
                    if channels == 0 || rate != 48_000 {
                        bail!("{}: expected 48 kHz audio", path.display());
                    }
                    let float = match (fmt_tag, bits) {
                        (1, 16) => false,
                        (3, 32) => true,
                        _ => bail!(
                            "{}: unsupported WAV format (tag {fmt_tag}, {bits} bit)",
                            path.display()
                        ),
                    };
                    let bps = if float { 4 } else { 2 };
                    return Ok(WavFile {
                        file,
                        data_offset,
                        frames: len / (bps * channels as u64),
                        channels,
                        float,
                        buf: Vec::new(),
                    });
                }
                _ => {}
            }
            pos += 8 + size + (size & 1);
        }
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// `frame` から `out.len() / 2` フレームを読み、ステレオで `out` に**加算**する（`gain` 倍）。
    /// 範囲外は何もしない。
    pub fn mix_into(&mut self, frame: i64, out: &mut [f32], gain: f32) -> Result<()> {
        let want = (out.len() / 2) as i64;
        let lo = frame.max(0);
        let hi = (frame + want).min(self.frames as i64);
        if lo >= hi {
            return Ok(());
        }
        let n = (hi - lo) as usize;
        let bps = if self.float { 4 } else { 2 };
        self.buf.resize(n * self.channels * bps, 0);
        self.file.seek(SeekFrom::Start(
            self.data_offset + lo as u64 * (self.channels * bps) as u64,
        ))?;
        self.file.read_exact(&mut self.buf)?;
        let skip = (lo - frame) as usize;
        let sample = |i: usize| -> f32 {
            if self.float {
                f32::from_le_bytes(self.buf[i * 4..i * 4 + 4].try_into().unwrap())
            } else {
                i16::from_le_bytes([self.buf[i * 2], self.buf[i * 2 + 1]]) as f32 / 32768.0
            }
        };
        for f in 0..n {
            let (l, r) = if self.channels == 1 {
                let v = sample(f);
                (v, v)
            } else {
                (sample(f * self.channels), sample(f * self.channels + 1))
            };
            let o = &mut out[(skip + f) * 2..(skip + f) * 2 + 2];
            o[0] += l * gain;
            o[1] += r * gain;
        }
        Ok(())
    }
}
