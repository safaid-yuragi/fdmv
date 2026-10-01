//! IVF（AV1 の素のビットストリーム）の読み書き。ffmpeg との受け渡しに使う。

use std::io::{Read, Seek, SeekFrom, Write};

use crate::error::{Result, malformed};
use crate::format::Rational;

pub struct IvfHeader {
    pub fourcc: [u8; 4],
    pub width: u16,
    pub height: u16,
    pub timebase: Rational,
    pub frame_count: u32,
}

pub struct IvfFrame {
    pub pts: i64,
    pub data: Vec<u8>,
}

pub struct IvfReader<R: Read> {
    r: R,
    pub header: IvfHeader,
}

impl<R: Read> IvfReader<R> {
    pub fn new(mut r: R) -> Result<Self> {
        let mut h = [0u8; 32];
        r.read_exact(&mut h)?;
        if &h[0..4] != b"DKIF" {
            return malformed("not an IVF file");
        }
        let header_len = u16::from_le_bytes([h[6], h[7]]) as usize;
        if header_len < 32 {
            return malformed("invalid IVF header length");
        }
        // 拡張ヘッダがあれば読み捨てる。
        std::io::copy(
            &mut (&mut r).take((header_len - 32) as u64),
            &mut std::io::sink(),
        )?;
        let rate = u32::from_le_bytes(h[16..20].try_into().unwrap());
        let scale = u32::from_le_bytes(h[20..24].try_into().unwrap());
        let timebase = Rational::new(scale, rate);
        if !timebase.is_valid() {
            return malformed("IVF timebase is zero");
        }
        let header = IvfHeader {
            fourcc: h[8..12].try_into().unwrap(),
            width: u16::from_le_bytes([h[12], h[13]]),
            height: u16::from_le_bytes([h[14], h[15]]),
            timebase,
            frame_count: u32::from_le_bytes(h[24..28].try_into().unwrap()),
        };
        Ok(IvfReader { r, header })
    }

    pub fn next_frame(&mut self) -> Result<Option<IvfFrame>> {
        let mut h = [0u8; 12];
        match self.r.read_exact(&mut h[..1]) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        self.r.read_exact(&mut h[1..])?;
        let size = u32::from_le_bytes(h[0..4].try_into().unwrap()) as usize;
        let pts = u64::from_le_bytes(h[4..12].try_into().unwrap()) as i64;
        if size as u64 > crate::format::MAX_PACKET_SIZE {
            return malformed("IVF frame too large");
        }
        let mut data = vec![0u8; size];
        self.r.read_exact(&mut data)?;
        Ok(Some(IvfFrame { pts, data }))
    }
}

pub struct IvfWriter<W: Write + Seek> {
    w: W,
    frames: u32,
}

impl<W: Write + Seek> IvfWriter<W> {
    pub fn new(mut w: W, width: u16, height: u16, timebase: Rational) -> Result<Self> {
        let mut h = Vec::with_capacity(32);
        h.extend_from_slice(b"DKIF");
        h.extend_from_slice(&0u16.to_le_bytes());
        h.extend_from_slice(&32u16.to_le_bytes());
        h.extend_from_slice(b"AV01");
        h.extend_from_slice(&width.to_le_bytes());
        h.extend_from_slice(&height.to_le_bytes());
        h.extend_from_slice(&timebase.den.to_le_bytes());
        h.extend_from_slice(&timebase.num.to_le_bytes());
        h.extend_from_slice(&0u32.to_le_bytes());
        h.extend_from_slice(&0u32.to_le_bytes());
        w.write_all(&h)?;
        Ok(IvfWriter { w, frames: 0 })
    }

    pub fn write_frame(&mut self, pts: i64, data: &[u8]) -> Result<()> {
        self.w.write_all(&(data.len() as u32).to_le_bytes())?;
        self.w.write_all(&(pts as u64).to_le_bytes())?;
        self.w.write_all(data)?;
        self.frames += 1;
        Ok(())
    }

    pub fn finish(mut self) -> Result<W> {
        self.w.seek(SeekFrom::Start(24))?;
        self.w.write_all(&self.frames.to_le_bytes())?;
        self.w.seek(SeekFrom::End(0))?;
        self.w.flush()?;
        Ok(self.w)
    }
}
