//! 定数、有理数タイムベース、バイナリのエンコード／デコード補助。

use std::cmp::Ordering;

use crate::error::{Result, malformed};

pub const MAGIC: [u8; 8] = [0x89, b'F', b'D', b'M', b'V', 0x0D, 0x0A, 0x1A];
pub const VERSION_MAJOR: u16 = 1;
pub const VERSION_MINOR: u16 = 0;
pub const HEADER_SIZE: u64 = 16;

pub type FourCC = [u8; 4];
pub const BLOCK_CLUSTER: FourCC = *b"CLST";
pub const BLOCK_DIRECTORY: FourCC = *b"DIRC";
pub const BLOCK_FOOTER: FourCC = *b"FOOT";
/// 種別 4 + 長さ 8 + CRC 4。
pub const BLOCK_OVERHEAD: u64 = 16;
pub const FOOTER_SIZE: u64 = BLOCK_OVERHEAD + 8;

pub const PACKET_HEADER_SIZE: usize = 20;
pub const INDEX_ENTRY_SIZE: usize = 24;
pub const SEGMENT_SIZE: usize = 20;

/// パケット／インデックスの flags。
pub mod flags {
    pub const KEYFRAME: u8 = 1 << 0;
    pub const SEGMENT_START: u8 = 1 << 1;
}

pub const KIND_VIDEO: u8 = 0;
pub const KIND_CHAIN: u8 = 1;
pub const CODEC_AV1: u8 = 1;
pub const CODEC_OPUS: u8 = 1;

pub const OPUS_SAMPLE_RATE: u32 = 48_000;
/// シーク時に Opus デコーダを安定させるためのプリロール（80 ms）。
pub const OPUS_PREROLL: i64 = 3840;
/// Opus パケット 1 つの最大サンプル数（120 ms）。
pub const OPUS_MAX_FRAME: usize = 5760;

pub const MAX_PACKET_SIZE: u64 = 256 << 20;
pub const MAX_CLUSTER_SIZE: u64 = 1 << 30;
pub const MAX_DIRECTORY_SIZE: u64 = 1 << 30;
pub const MAX_NAME_LEN: usize = 255;
pub const MAX_CHAIN_CHANNELS: u8 = 2;

/// 1 tick = num/den 秒。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rational {
    pub num: u32,
    pub den: u32,
}

impl Rational {
    pub const OPUS: Rational = Rational {
        num: 1,
        den: OPUS_SAMPLE_RATE,
    };

    pub const fn new(num: u32, den: u32) -> Self {
        Rational { num, den }
    }

    pub fn is_valid(self) -> bool {
        self.num != 0 && self.den != 0
    }

    pub fn reduced(self) -> Self {
        let g = gcd(self.num as u64, self.den as u64).max(1);
        Rational {
            num: (self.num as u64 / g) as u32,
            den: (self.den as u64 / g) as u32,
        }
    }

    pub fn as_f64(self) -> f64 {
        self.num as f64 / self.den as f64
    }

    pub fn to_seconds(self, ticks: i64) -> f64 {
        ticks as f64 * self.num as f64 / self.den as f64
    }

    /// 秒をこのタイムベースの tick に変換する（最も近い値に丸める）。
    pub fn from_seconds(self, secs: f64) -> i64 {
        (secs * self.den as f64 / self.num as f64).round() as i64
    }

    /// 秒を tick に変換する（切り捨て）。「その時刻に表示されているフレーム」を求めるときに使う。
    pub fn floor_seconds(self, secs: f64) -> i64 {
        (secs * self.den as f64 / self.num as f64 + 1e-9).floor() as i64
    }

    /// `ticks`（タイムベース `self`）を `to` に変換する。最も近い値に丸める。
    pub fn rescale(self, ticks: i64, to: Rational) -> i64 {
        let n = ticks as i128 * self.num as i128 * to.den as i128;
        let d = self.den as i128 * to.num as i128;
        (2 * n + d).div_euclid(2 * d) as i64
    }

    /// 異なるタイムベースの時刻を正確に比較する。
    pub fn cmp_time(a: i64, a_tb: Rational, b: i64, b_tb: Rational) -> Ordering {
        let l = a as i128 * a_tb.num as i128 * b_tb.den as i128;
        let r = b as i128 * b_tb.num as i128 * a_tb.den as i128;
        l.cmp(&r)
    }
}

impl std::fmt::Display for Rational {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.num, self.den)
    }
}

pub fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// リトルエンディアンのバイト列を組み立てる。
#[derive(Default)]
pub struct Enc {
    pub buf: Vec<u8>,
}

impl Enc {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn f32(&mut self, v: f32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn raw(&mut self, v: &[u8]) {
        self.buf.extend_from_slice(v);
    }
    pub fn bytes(&mut self, v: &[u8]) {
        self.u32(v.len() as u32);
        self.raw(v);
    }
    pub fn string(&mut self, v: &str) {
        self.bytes(v.as_bytes());
    }
}

/// リトルエンディアンのバイト列を読む。範囲外アクセスは `Malformed` エラーになる。
pub struct Dec<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Dec<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Dec { data, pos: 0 }
    }
    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }
    pub fn position(&self) -> usize {
        self.pos
    }
    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return malformed(format!(
                "unexpected end of data (need {n} bytes at {}, have {})",
                self.pos,
                self.remaining()
            ));
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn arr<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into().unwrap())
    }
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.arr()?))
    }
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.arr()?))
    }
    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.arr()?))
    }
    pub fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.arr()?))
    }
    pub fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_le_bytes(self.arr()?))
    }
    pub fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    pub fn string(&mut self) -> Result<String> {
        let b = self.bytes()?;
        match std::str::from_utf8(b) {
            Ok(s) => Ok(s.to_owned()),
            Err(_) => malformed("string is not valid UTF-8"),
        }
    }
    pub fn rest(&mut self) -> &'a [u8] {
        let s = &self.data[self.pos..];
        self.pos = self.data.len();
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rescale_rounds_to_nearest() {
        let video = Rational::new(1001, 30000);
        assert_eq!(video.rescale(30, Rational::OPUS), 48048);
        assert_eq!(Rational::OPUS.rescale(-1, Rational::new(1, 1000)), 0);
        assert_eq!(Rational::OPUS.rescale(-48, Rational::new(1, 1000)), -1);
    }

    #[test]
    fn cmp_time_is_exact() {
        let a = Rational::new(1, 3);
        let b = Rational::new(1, 6);
        assert_eq!(Rational::cmp_time(1, a, 2, b), Ordering::Equal);
        assert_eq!(Rational::cmp_time(1, a, 3, b), Ordering::Less);
    }
}
