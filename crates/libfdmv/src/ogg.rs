//! Ogg ページの読み込み（単一の論理ストリームのみ）。ffmpeg が出力した Ogg Opus の取り込みに使う。

use std::io::Read;

use crate::error::{Result, malformed};

/// 1 ページ分の、このページで完結したパケット。
pub struct OggPage {
    pub packets: Vec<Vec<u8>>,
    pub granule: i64,
    pub bos: bool,
    pub eos: bool,
}

pub struct OggPageReader<R: Read> {
    r: R,
    serial: Option<u32>,
    partial: Vec<u8>,
}

static CRC_TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = (i as u32) << 24;
        let mut k = 0;
        while k < 8 {
            c = if c & 0x8000_0000 != 0 {
                (c << 1) ^ 0x04c1_1db7
            } else {
                c << 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

fn ogg_crc(parts: &[&[u8]]) -> u32 {
    let mut crc = 0u32;
    for p in parts {
        for &b in *p {
            crc = (crc << 8) ^ CRC_TABLE[((crc >> 24) as u8 ^ b) as usize];
        }
    }
    crc
}

impl<R: Read> OggPageReader<R> {
    pub fn new(r: R) -> Self {
        OggPageReader {
            r,
            serial: None,
            partial: Vec::new(),
        }
    }

    pub fn next_page(&mut self) -> Result<Option<OggPage>> {
        let mut h = [0u8; 27];
        match self.r.read_exact(&mut h[..1]) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        self.r.read_exact(&mut h[1..])?;
        if &h[0..4] != b"OggS" || h[4] != 0 {
            return malformed("invalid Ogg page");
        }
        let header_type = h[5];
        let granule = i64::from_le_bytes(h[6..14].try_into().unwrap());
        let serial = u32::from_le_bytes(h[14..18].try_into().unwrap());
        let crc = u32::from_le_bytes(h[22..26].try_into().unwrap());
        let mut lacing = vec![0u8; h[26] as usize];
        self.r.read_exact(&mut lacing)?;
        let body_len: usize = lacing.iter().map(|&l| l as usize).sum();
        let mut body = vec![0u8; body_len];
        self.r.read_exact(&mut body)?;

        let mut hz = h;
        hz[22..26].fill(0);
        if ogg_crc(&[&hz, &lacing, &body]) != crc {
            return malformed("Ogg page CRC mismatch");
        }
        match self.serial {
            None => self.serial = Some(serial),
            Some(s) if s != serial => {
                return malformed("multiplexed Ogg streams are not supported");
            }
            _ => {}
        }
        if header_type & 0x01 == 0 && !self.partial.is_empty() {
            return malformed("Ogg packet continuation missing");
        }

        let mut packets = Vec::new();
        let mut pos = 0;
        for &l in &lacing {
            self.partial.extend_from_slice(&body[pos..pos + l as usize]);
            pos += l as usize;
            if l < 255 {
                packets.push(std::mem::take(&mut self.partial));
            }
        }
        Ok(Some(OggPage {
            packets,
            granule,
            bos: header_type & 0x02 != 0,
            eos: header_type & 0x04 != 0,
        }))
    }
}
