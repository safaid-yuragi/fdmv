//! Opus 関連: OpusHead の解析、パケット長の計算、Ogg Opus からのパケット取り出し。

use std::collections::VecDeque;
use std::io::Read;

use crate::error::{Result, malformed};
use crate::ogg::OggPageReader;

#[derive(Clone, Debug, PartialEq)]
pub struct OpusHead {
    pub channels: u8,
    pub pre_skip: u16,
    pub input_sample_rate: u32,
    /// Q7.8 形式の dB。
    pub output_gain: i16,
    pub mapping_family: u8,
    pub raw: Vec<u8>,
}

impl OpusHead {
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < 19 || &data[0..8] != b"OpusHead" {
            return malformed("invalid OpusHead");
        }
        if data[8] >> 4 != 0 {
            return malformed(format!("unsupported OpusHead version {}", data[8]));
        }
        Ok(OpusHead {
            channels: data[9],
            pre_skip: u16::from_le_bytes([data[10], data[11]]),
            input_sample_rate: u32::from_le_bytes(data[12..16].try_into().unwrap()),
            output_gain: i16::from_le_bytes([data[16], data[17]]),
            mapping_family: data[18],
            raw: data.to_vec(),
        })
    }

    pub fn output_gain_db(&self) -> f32 {
        self.output_gain as f32 / 256.0
    }
}

/// Opus パケットのサンプル数（48 kHz 換算）を TOC バイトから求める（RFC 6716 §3.1）。
pub fn packet_samples(data: &[u8]) -> Result<u32> {
    let Some(&toc) = data.first() else {
        return malformed("empty Opus packet");
    };
    let config = toc >> 3;
    let frame = match config {
        0..=11 => [480, 960, 1920, 2880][(config % 4) as usize],
        12..=15 => [480, 960][(config % 2) as usize],
        _ => [120, 240, 480, 960][(config % 4) as usize],
    };
    let count = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => match data.get(1) {
            Some(&b) => (b & 0x3f) as u32,
            None => return malformed("truncated Opus packet"),
        },
    };
    let total = frame * count;
    if total > 5760 {
        return malformed("Opus packet longer than 120 ms");
    }
    Ok(total)
}

pub struct OpusPacket {
    /// PCM 位置（グラニュール位置 - pre_skip）。負の値は pre_skip 区間。
    pub pcm_pos: i64,
    pub duration: u32,
    pub data: Vec<u8>,
}

/// Ogg Opus ファイルから Opus パケットを順に取り出す。
pub struct OggOpusReader<R: Read> {
    pages: OggPageReader<R>,
    pub head: OpusHead,
    queue: VecDeque<Vec<u8>>,
    pending_first_page: bool,
    granule: i64,
    last_page_granule: Option<i64>,
    finished: bool,
}

impl<R: Read> OggOpusReader<R> {
    pub fn new(r: R) -> Result<Self> {
        let mut pages = OggPageReader::new(r);
        let mut headers: Vec<Vec<u8>> = Vec::new();
        let mut queue = VecDeque::new();
        // OpusHead と OpusTags を読み飛ばす。同じページに音声パケットが続くことは仕様上ない。
        while headers.len() < 2 {
            let Some(page) = pages.next_page()? else {
                return malformed("Ogg Opus stream ended before headers");
            };
            for p in page.packets {
                if headers.len() < 2 {
                    headers.push(p);
                } else {
                    queue.push_back(p);
                }
            }
        }
        if !queue.is_empty() {
            return malformed("audio data on an Opus header page");
        }
        let head = OpusHead::parse(&headers[0])?;
        if headers[1].get(0..8) != Some(b"OpusTags".as_slice()) {
            return malformed("missing OpusTags");
        }
        Ok(OggOpusReader {
            pages,
            head,
            queue,
            pending_first_page: true,
            granule: 0,
            last_page_granule: None,
            finished: false,
        })
    }

    pub fn next_packet(&mut self) -> Result<Option<OpusPacket>> {
        while self.queue.is_empty() {
            if self.finished {
                return Ok(None);
            }
            let Some(page) = self.pages.next_page()? else {
                self.finished = true;
                continue;
            };
            if page.granule >= 0 && !page.packets.is_empty() {
                self.last_page_granule = Some(page.granule);
            }
            if self.pending_first_page && !page.packets.is_empty() {
                self.pending_first_page = false;
                // 最初の音声ページのグラニュール位置から、開始位置を逆算する（RFC 7845 §4）。
                if !page.eos {
                    let mut total = 0i64;
                    for p in &page.packets {
                        total += packet_samples(p)? as i64;
                    }
                    self.granule = page.granule - total;
                    if self.granule < 0 {
                        return malformed("negative starting granule position");
                    }
                }
            }
            self.queue.extend(page.packets);
            if page.eos {
                self.finished = true;
            }
        }
        let data = self.queue.pop_front().unwrap();
        let duration = packet_samples(&data)?;
        let pcm_pos = self.granule - self.head.pre_skip as i64;
        self.granule += duration as i64;
        Ok(Some(OpusPacket {
            pcm_pos,
            duration,
            data,
        }))
    }

    /// 全パケットを読み終えた後の、鳴るサンプル数（PCM 位置の終端）。
    pub fn pcm_length(&self) -> i64 {
        let end = self
            .last_page_granule
            .unwrap_or(self.granule)
            .min(self.granule);
        (end - self.head.pre_skip as i64).max(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toc_durations() {
        // CELT 20 ms, 1 フレーム
        assert_eq!(packet_samples(&[(31 << 3)]).unwrap(), 960);
        // SILK 60 ms, 2 フレーム
        assert_eq!(packet_samples(&[(3 << 3) | 1]).unwrap(), 5760);
        // CELT 2.5 ms, code 3 で 4 フレーム
        assert_eq!(packet_samples(&[(16 << 3) | 3, 4]).unwrap(), 480);
    }
}
