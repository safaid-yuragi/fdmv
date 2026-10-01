//! ブロック（種別 + 長さ + payload + CRC-32）の読み書き。

use std::io::{Read, Seek, SeekFrom, Write};

use crate::error::{Error, Result, malformed};
use crate::format::{BLOCK_OVERHEAD, FourCC};

fn crc(kind: &FourCC, size: u64, payload: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(kind);
    h.update(&size.to_le_bytes());
    h.update(payload);
    h.finalize()
}

/// ブロックを書き込み、書いたバイト数を返す。
pub fn write_block<W: Write>(w: &mut W, kind: FourCC, payload: &[u8]) -> Result<u64> {
    let size = payload.len() as u64;
    w.write_all(&kind)?;
    w.write_all(&size.to_le_bytes())?;
    w.write_all(payload)?;
    w.write_all(&crc(&kind, size, payload).to_le_bytes())?;
    Ok(size + BLOCK_OVERHEAD)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockHeader {
    pub kind: FourCC,
    /// payload の長さ。
    pub size: u64,
    /// ブロック先頭の位置。
    pub offset: u64,
}

impl BlockHeader {
    pub fn total_size(&self) -> u64 {
        self.size + BLOCK_OVERHEAD
    }
    pub fn end(&self) -> u64 {
        self.offset + self.total_size()
    }
}

pub fn read_block_header<R: Read + Seek>(r: &mut R, offset: u64) -> Result<BlockHeader> {
    r.seek(SeekFrom::Start(offset))?;
    let mut buf = [0u8; 12];
    r.read_exact(&mut buf)?;
    Ok(BlockHeader {
        kind: buf[0..4].try_into().unwrap(),
        size: u64::from_le_bytes(buf[4..12].try_into().unwrap()),
        offset,
    })
}

/// `offset` のブロックを読み、CRC を検証して payload を返す。
pub fn read_block<R: Read + Seek>(
    r: &mut R,
    offset: u64,
    expected: FourCC,
    max_size: u64,
    file_len: u64,
) -> Result<Vec<u8>> {
    let h = read_block_header(r, offset)?;
    if h.kind != expected {
        return malformed(format!(
            "expected {} block at offset {offset}, found {:?}",
            String::from_utf8_lossy(&expected),
            String::from_utf8_lossy(&h.kind)
        ));
    }
    if h.size > max_size || h.end() > file_len {
        return malformed(format!(
            "block at offset {offset} has invalid size {}",
            h.size
        ));
    }
    let mut payload = vec![0u8; h.size as usize];
    r.read_exact(&mut payload)?;
    let mut c = [0u8; 4];
    r.read_exact(&mut c)?;
    if u32::from_le_bytes(c) != crc(&h.kind, h.size, &payload) {
        return Err(Error::Crc { offset });
    }
    Ok(payload)
}

/// ヘッダを読んだ後、payload を読まずに CRC だけ検証する（ストリーミングで計算）。
pub fn verify_block<R: Read + Seek>(r: &mut R, h: &BlockHeader) -> Result<()> {
    r.seek(SeekFrom::Start(h.offset + 12))?;
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&h.kind);
    hasher.update(&h.size.to_le_bytes());
    let mut left = h.size;
    let mut buf = vec![0u8; 1 << 16];
    while left > 0 {
        let n = left.min(buf.len() as u64) as usize;
        r.read_exact(&mut buf[..n])?;
        hasher.update(&buf[..n]);
        left -= n as u64;
    }
    let mut c = [0u8; 4];
    r.read_exact(&mut c)?;
    if u32::from_le_bytes(c) != hasher.finalize() {
        return Err(Error::Crc { offset: h.offset });
    }
    Ok(())
}
