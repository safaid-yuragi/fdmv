use std::io::{Read, Seek};

use super::audio::ChainDecoder;
use crate::error::{Result, invalid};
use crate::format::{OPUS_PREROLL, Rational};
use crate::model::{Directory, IndexEntry};
use crate::reader::{FdmvReader, StreamCursor};

/// ミックスするチェーンと、その倍率（チェーンの gain_db とユーザ音量を掛けた線形値）。
#[derive(Clone, Copy, Debug)]
pub struct ChainSelection {
    pub stream_id: u16,
    pub gain: f32,
}

impl ChainSelection {
    /// チェーンの gain_db をそのまま使う。
    pub fn with_chain_gain(dir: &Directory, stream_id: u16) -> Self {
        let gain = dir
            .stream(stream_id)
            .and_then(|s| s.chain())
            .map(|c| c.gain_linear())
            .unwrap_or(1.0);
        ChainSelection { stream_id, gain }
    }
}

struct Track {
    stream_id: u16,
    gain: f32,
    cursor: StreamCursor,
    decoder: ChainDecoder,
    channels: usize,
    /// インデックスエントリと、それが属するセグメント番号。
    entries: Vec<(usize, IndexEntry)>,
    /// デコード済みで未消費のサンプル（インターリーブ）。`buf_start` から連続している。
    buf: Vec<f32>,
    buf_start: i64,
    exhausted: bool,
}

impl Track {
    fn new(dir: &Directory, sel: ChainSelection) -> Result<Self> {
        let Some(entry) = dir.stream(sel.stream_id).filter(|s| s.is_chain()) else {
            return invalid(format!("stream {} is not a chain", sel.stream_id));
        };
        let decoder = ChainDecoder::new(entry)?;
        let cursor = StreamCursor::new(dir, sel.stream_id);
        let mut seg = 0usize;
        let mut seen_start = false;
        let entries = cursor
            .entries()
            .iter()
            .map(|e| {
                if e.is_segment_start() {
                    if seen_start {
                        seg += 1;
                    }
                    seen_start = true;
                }
                (seg, *e)
            })
            .collect();
        Ok(Track {
            stream_id: sel.stream_id,
            gain: sel.gain,
            channels: decoder.channels(),
            cursor,
            decoder,
            entries,
            buf: Vec::new(),
            buf_start: 0,
            exhausted: false,
        })
    }

    fn buf_end(&self) -> i64 {
        self.buf_start + (self.buf.len() / self.channels) as i64
    }

    fn seek<R: Read + Seek>(&mut self, reader: &mut FdmvReader<R>, t: i64) -> Result<()> {
        self.buf.clear();
        self.buf_start = t;
        self.exhausted = false;
        let Some(si) = self.decoder.segments().iter().position(|s| s.end() > t) else {
            self.exhausted = true;
            return Ok(());
        };
        let target = t - OPUS_PREROLL;
        let in_seg = self
            .entries
            .iter()
            .filter(|(s, _)| *s == si)
            .map(|(_, e)| e);
        let mut chosen = None;
        for e in in_seg {
            if chosen.is_none() || e.pts <= target {
                chosen = Some(*e);
            }
        }
        let Some(e) = chosen else {
            self.exhausted = true;
            return Ok(());
        };
        self.cursor.seek_to_entry(reader, &e)?;
        self.decoder.begin_segment(si, e.is_segment_start());
        Ok(())
    }

    /// `until` までのサンプルを用意する。次のパケットが `until` 以降なら、そこまでは無音。
    fn fill<R: Read + Seek>(&mut self, reader: &mut FdmvReader<R>, until: i64) -> Result<()> {
        while !self.exhausted && self.buf_end() < until {
            match self.cursor.peek(reader)? {
                None => {
                    self.exhausted = true;
                    break;
                }
                Some(p) if p.pts >= until => break,
                Some(_) => {}
            }
            let pkt = self.cursor.next_packet(reader)?.unwrap();
            let Track {
                decoder,
                buf,
                buf_start,
                channels,
                ..
            } = self;
            if let Some(d) = decoder.decode(&pkt)? {
                push(buf, buf_start, *channels, d.start, d.samples);
            }
        }
        Ok(())
    }

    /// `until` より前のサンプルを捨てる。
    fn consume(&mut self, until: i64) {
        let frames = (until - self.buf_start).clamp(0, (self.buf.len() / self.channels) as i64);
        self.buf.drain(..frames as usize * self.channels);
        self.buf_start += frames;
        if self.buf.is_empty() && self.buf_start < until {
            self.buf_start = until;
        }
    }
}

fn push(buf: &mut Vec<f32>, buf_start: &mut i64, ch: usize, start: i64, samples: &[f32]) {
    if buf.is_empty() && start > *buf_start {
        *buf_start = start;
    }
    let end = *buf_start + (buf.len() / ch) as i64;
    let frames = samples.len() / ch;
    let mut skip = 0usize;
    if start < end {
        skip = (end - start) as usize;
        if skip >= frames {
            return;
        }
    } else if start > end {
        buf.resize(buf.len() + (start - end) as usize * ch, 0.0);
    }
    buf.extend_from_slice(&samples[skip * ch..]);
}

/// 複数のチェーンをタイムラインに沿ってデコード・ミックスし、48 kHz の PCM を出力する。
///
/// セグメントの外や、チェーンが 1 本も無い区間は無音で埋める。出力はタイムラインの長さ（映像の長さ）で終わる。
pub struct AudioRenderer {
    tracks: Vec<Track>,
    out_channels: usize,
    position: i64,
    end: i64,
}

impl AudioRenderer {
    /// `out_channels` が None なら、選んだチェーンの最大チャンネル数（無ければ 2）。
    pub fn new(
        dir: &Directory,
        selections: &[ChainSelection],
        out_channels: Option<usize>,
    ) -> Result<Self> {
        let mut tracks = Vec::new();
        for sel in selections {
            if tracks.iter().any(|t: &Track| t.stream_id == sel.stream_id) {
                continue;
            }
            tracks.push(Track::new(dir, *sel)?);
        }
        let out_channels =
            out_channels.unwrap_or_else(|| tracks.iter().map(|t| t.channels).max().unwrap_or(2));
        if out_channels == 0 {
            return invalid("output must have at least one channel");
        }
        let end = dir
            .timeline()
            .map(|(d, tb)| tb.rescale(d, Rational::OPUS))
            .unwrap_or(0);
        Ok(AudioRenderer {
            tracks,
            out_channels,
            position: 0,
            end,
        })
    }

    pub fn channels(&self) -> usize {
        self.out_channels
    }
    pub fn sample_rate(&self) -> u32 {
        crate::format::OPUS_SAMPLE_RATE
    }
    /// 現在位置（サンプル）。
    pub fn position(&self) -> i64 {
        self.position
    }
    /// タイムラインの長さ（サンプル）。
    pub fn len(&self) -> i64 {
        self.end
    }
    pub fn is_empty(&self) -> bool {
        self.end == 0
    }
    pub fn chains(&self) -> impl Iterator<Item = ChainSelection> + '_ {
        self.tracks.iter().map(|t| ChainSelection {
            stream_id: t.stream_id,
            gain: t.gain,
        })
    }

    pub fn set_gain(&mut self, stream_id: u16, gain: f32) {
        if let Some(t) = self.tracks.iter_mut().find(|t| t.stream_id == stream_id) {
            t.gain = gain;
        }
    }

    /// 再生中にチェーンを有効にする。現在位置から鳴り始める。
    pub fn add_chain<R: Read + Seek>(
        &mut self,
        reader: &mut FdmvReader<R>,
        sel: ChainSelection,
    ) -> Result<()> {
        if self.tracks.iter().any(|t| t.stream_id == sel.stream_id) {
            self.set_gain(sel.stream_id, sel.gain);
            return Ok(());
        }
        let mut t = Track::new(reader.directory(), sel)?;
        t.seek(reader, self.position)?;
        self.tracks.push(t);
        Ok(())
    }

    pub fn remove_chain(&mut self, stream_id: u16) {
        self.tracks.retain(|t| t.stream_id != stream_id);
    }

    /// 位置をサンプル単位で変更する。
    pub fn seek<R: Read + Seek>(&mut self, reader: &mut FdmvReader<R>, sample: i64) -> Result<()> {
        let sample = sample.clamp(0, self.end);
        for t in &mut self.tracks {
            t.seek(reader, sample)?;
        }
        self.position = sample;
        Ok(())
    }

    /// `out`（インターリーブ）を埋め、書いたフレーム数を返す。タイムラインの終わりで 0 を返す。
    /// 値は [-1, 1] にクリップしない。
    pub fn render<R: Read + Seek>(
        &mut self,
        reader: &mut FdmvReader<R>,
        out: &mut [f32],
    ) -> Result<usize> {
        let oc = self.out_channels;
        let n = ((out.len() / oc) as i64)
            .min(self.end - self.position)
            .max(0) as usize;
        let out = &mut out[..n * oc];
        out.fill(0.0);
        let (lo, hi) = (self.position, self.position + n as i64);
        for t in &mut self.tracks {
            t.fill(reader, hi)?;
            let a = lo.max(t.buf_start);
            let b = hi.min(t.buf_end());
            let tc = t.channels;
            let g = t.gain;
            for f in a..b {
                let src = &t.buf[(f - t.buf_start) as usize * tc..][..tc];
                let dst = &mut out[(f - lo) as usize * oc..][..oc];
                mix_frame(src, dst, g);
            }
            t.consume(hi);
        }
        self.position = hi;
        Ok(n)
    }
}

fn mix_frame(src: &[f32], dst: &mut [f32], gain: f32) {
    match (src.len(), dst.len()) {
        (s, d) if s == d => {
            for (o, i) in dst.iter_mut().zip(src) {
                *o += i * gain;
            }
        }
        (1, _) => {
            for o in dst.iter_mut() {
                *o += src[0] * gain;
            }
        }
        (_, 1) => {
            dst[0] += src.iter().sum::<f32>() / src.len() as f32 * gain;
        }
        _ => {
            for (c, o) in dst.iter_mut().enumerate() {
                if let Some(i) = src.get(c) {
                    *o += i * gain;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_fills_gaps_and_trims_overlap() {
        let mut buf = Vec::new();
        let mut start = 0i64;
        push(&mut buf, &mut start, 1, 10, &[1.0, 1.0]);
        assert_eq!(start, 10);
        push(&mut buf, &mut start, 1, 14, &[2.0]);
        assert_eq!(buf, vec![1.0, 1.0, 0.0, 0.0, 2.0]);
        push(&mut buf, &mut start, 1, 13, &[9.0, 9.0, 3.0]);
        assert_eq!(buf, vec![1.0, 1.0, 0.0, 0.0, 2.0, 3.0]);
    }
}
