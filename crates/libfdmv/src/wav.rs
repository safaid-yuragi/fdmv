//! WAV（RIFF）出力。全体の長さを先に与えるので、シークできない出力（パイプ）にも書ける。

use std::io::Write;

use crate::error::Result;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleFormat {
    S16,
    F32,
}

impl SampleFormat {
    fn bytes(self) -> u16 {
        match self {
            SampleFormat::S16 => 2,
            SampleFormat::F32 => 4,
        }
    }
}

pub struct WavWriter<W: Write> {
    w: W,
    channels: u16,
    format: SampleFormat,
    buf: Vec<u8>,
}

impl<W: Write> WavWriter<W> {
    pub fn new(
        mut w: W,
        sample_rate: u32,
        channels: u16,
        format: SampleFormat,
        total_frames: u64,
    ) -> Result<Self> {
        let block_align = channels * format.bytes();
        let data_len = total_frames * block_align as u64;
        // 4 GiB を超える場合は長さを最大値にしておく（多くのツールが読み飛ばしてくれる）。
        let data_len32 = u32::try_from(data_len).unwrap_or(u32::MAX);
        let riff_len = data_len32.saturating_add(36);
        let mut h = Vec::with_capacity(44);
        h.extend_from_slice(b"RIFF");
        h.extend_from_slice(&riff_len.to_le_bytes());
        h.extend_from_slice(b"WAVEfmt ");
        h.extend_from_slice(&16u32.to_le_bytes());
        h.extend_from_slice(
            &(if format == SampleFormat::F32 {
                3u16
            } else {
                1u16
            })
            .to_le_bytes(),
        );
        h.extend_from_slice(&channels.to_le_bytes());
        h.extend_from_slice(&sample_rate.to_le_bytes());
        h.extend_from_slice(&(sample_rate * block_align as u32).to_le_bytes());
        h.extend_from_slice(&block_align.to_le_bytes());
        h.extend_from_slice(&(format.bytes() * 8).to_le_bytes());
        h.extend_from_slice(b"data");
        h.extend_from_slice(&data_len32.to_le_bytes());
        w.write_all(&h)?;
        Ok(WavWriter {
            w,
            channels,
            format,
            buf: Vec::new(),
        })
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// インターリーブされたサンプルを書く。S16 では [-1, 1] にクリップする。
    pub fn write(&mut self, samples: &[f32]) -> Result<()> {
        self.buf.clear();
        match self.format {
            SampleFormat::S16 => {
                for &s in samples {
                    let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
                    self.buf.extend_from_slice(&v.to_le_bytes());
                }
            }
            SampleFormat::F32 => {
                for &s in samples {
                    self.buf.extend_from_slice(&s.to_le_bytes());
                }
            }
        }
        self.w.write_all(&self.buf)?;
        Ok(())
    }

    pub fn finish(mut self) -> Result<W> {
        self.w.flush()?;
        Ok(self.w)
    }
}
