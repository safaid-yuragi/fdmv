//! ffmpeg に生の BGRA フレームを時刻付きで渡すための、最小限の Matroska 書き出し。
//!
//! 生の映像（rawvideo）をそのままパイプに流すと時刻を伝えられない。Matroska の `V_UNCOMPRESSED`
//! トラック（ColourSpace = `BGRA`）にフレームごとの時刻（ミリ秒）を付けて書く。
//! セグメントの長さは不明（ストリーミング）とし、クラスタ 1 つにフレーム 1 枚を入れる。

use std::io::{self, Write};

/// EBML の要素 ID（先頭のマーカーを含むそのままの値）。
mod id {
    pub const EBML: u32 = 0x1A45_DFA3;
    pub const EBML_VERSION: u32 = 0x4286;
    pub const EBML_READ_VERSION: u32 = 0x42F7;
    pub const EBML_MAX_ID_LENGTH: u32 = 0x42F2;
    pub const EBML_MAX_SIZE_LENGTH: u32 = 0x42F3;
    pub const DOC_TYPE: u32 = 0x4282;
    pub const DOC_TYPE_VERSION: u32 = 0x4287;
    pub const DOC_TYPE_READ_VERSION: u32 = 0x4285;
    pub const SEGMENT: u32 = 0x1853_8067;
    pub const INFO: u32 = 0x1549_A966;
    pub const TIMESTAMP_SCALE: u32 = 0x2A_D7B1;
    pub const MUXING_APP: u32 = 0x4D80;
    pub const WRITING_APP: u32 = 0x5741;
    pub const TRACKS: u32 = 0x1654_AE6B;
    pub const TRACK_ENTRY: u32 = 0xAE;
    pub const TRACK_NUMBER: u32 = 0xD7;
    pub const TRACK_UID: u32 = 0x73C5;
    pub const TRACK_TYPE: u32 = 0x83;
    pub const DEFAULT_DURATION: u32 = 0x23_E383;
    pub const CODEC_ID: u32 = 0x86;
    pub const VIDEO: u32 = 0xE0;
    pub const PIXEL_WIDTH: u32 = 0xB0;
    pub const PIXEL_HEIGHT: u32 = 0xBA;
    pub const COLOUR_SPACE: u32 = 0x2E_B524;
    pub const CLUSTER: u32 = 0x1F43_B675;
    pub const TIMESTAMP: u32 = 0xE7;
    pub const SIMPLE_BLOCK: u32 = 0xA3;
}

fn put_id(out: &mut Vec<u8>, id: u32) {
    let bytes = id.to_be_bytes();
    let skip = bytes.iter().take_while(|&&b| b == 0).count();
    out.extend_from_slice(&bytes[skip..]);
}

/// 要素の大きさ（常に 8 バイトの可変長整数で書く）。
fn put_size(out: &mut Vec<u8>, size: u64) {
    let mut b = size.to_be_bytes();
    b[0] = 0x01;
    out.extend_from_slice(&b);
}

fn element(out: &mut Vec<u8>, id: u32, data: &[u8]) {
    put_id(out, id);
    put_size(out, data.len() as u64);
    out.extend_from_slice(data);
}

fn uint(out: &mut Vec<u8>, id: u32, v: u64) {
    let bytes = v.to_be_bytes();
    let skip = bytes.iter().take_while(|&&b| b == 0).count().min(7);
    element(out, id, &bytes[skip..]);
}

pub struct MkvPipe<W: Write> {
    w: W,
    frame_len: usize,
    head: Vec<u8>,
}

impl<W: Write> MkvPipe<W> {
    /// ヘッダ（EBML・セグメントの開始・トラック）を書く。`fps` はフレームレートの目安
    /// （ffmpeg がエンコーダに伝えるフレームレートになる。実際の時刻はフレームごとに書く）。
    pub fn new(mut w: W, width: u32, height: u32, fps: u32) -> io::Result<Self> {
        let mut out = Vec::new();
        let mut ebml = Vec::new();
        uint(&mut ebml, id::EBML_VERSION, 1);
        uint(&mut ebml, id::EBML_READ_VERSION, 1);
        uint(&mut ebml, id::EBML_MAX_ID_LENGTH, 4);
        uint(&mut ebml, id::EBML_MAX_SIZE_LENGTH, 8);
        element(&mut ebml, id::DOC_TYPE, b"matroska");
        uint(&mut ebml, id::DOC_TYPE_VERSION, 4);
        uint(&mut ebml, id::DOC_TYPE_READ_VERSION, 2);
        element(&mut out, id::EBML, &ebml);

        // セグメント（長さ不明）
        put_id(&mut out, id::SEGMENT);
        out.extend_from_slice(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);

        let mut info = Vec::new();
        uint(&mut info, id::TIMESTAMP_SCALE, 1_000_000); // ミリ秒
        element(&mut info, id::MUXING_APP, b"fdmv-capture");
        element(&mut info, id::WRITING_APP, b"fdmv-capture");
        element(&mut out, id::INFO, &info);

        let mut video = Vec::new();
        uint(&mut video, id::PIXEL_WIDTH, width as u64);
        uint(&mut video, id::PIXEL_HEIGHT, height as u64);
        element(&mut video, id::COLOUR_SPACE, b"BGRA");
        let mut track = Vec::new();
        uint(&mut track, id::TRACK_NUMBER, 1);
        uint(&mut track, id::TRACK_UID, 1);
        uint(&mut track, id::TRACK_TYPE, 1); // 映像
        uint(
            &mut track,
            id::DEFAULT_DURATION,
            1_000_000_000 / fps.max(1) as u64,
        );
        element(&mut track, id::CODEC_ID, b"V_UNCOMPRESSED");
        element(&mut track, id::VIDEO, &video);
        let mut entry = Vec::new();
        element(&mut entry, id::TRACK_ENTRY, &track);
        element(&mut out, id::TRACKS, &entry);
        w.write_all(&out)?;
        Ok(MkvPipe {
            w,
            frame_len: width as usize * height as usize * 4,
            head: Vec::with_capacity(64),
        })
    }

    /// `ms` = 表示時刻（ミリ秒）。`bgra` は幅×高さ×4 バイト。
    pub fn write_frame(&mut self, ms: u64, bgra: &[u8]) -> io::Result<()> {
        debug_assert_eq!(bgra.len(), self.frame_len);
        let mut ts = Vec::new();
        uint(&mut ts, id::TIMESTAMP, ms);
        // SimpleBlock: トラック番号 1、相対時刻 0、キーフレーム
        let block_header = [0x81, 0x00, 0x00, 0x80];
        let block_len = block_header.len() + bgra.len();
        let h = &mut self.head;
        h.clear();
        put_id(h, id::CLUSTER);
        // Timestamp 要素 + SimpleBlock の ID（1 バイト）と大きさ（8 バイト）+ 中身
        put_size(h, (ts.len() + 1 + 8 + block_len) as u64);
        h.extend_from_slice(&ts);
        put_id(h, id::SIMPLE_BLOCK);
        put_size(h, block_len as u64);
        h.extend_from_slice(&block_header);
        self.w.write_all(h)?;
        self.w.write_all(bgra)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_elements() {
        let mut v = Vec::new();
        uint(&mut v, id::TIMESTAMP, 0);
        assert_eq!(v, [0xE7, 0x01, 0, 0, 0, 0, 0, 0, 1, 0]);
        let mut v = Vec::new();
        uint(&mut v, id::TIMESTAMP_SCALE, 1_000_000);
        assert_eq!(&v[..3], &[0x2A, 0xD7, 0xB1]);
        assert_eq!(&v[11..], &[0x0F, 0x42, 0x40]);
    }

    #[test]
    fn cluster_size_matches() {
        let mut p = MkvPipe::new(Vec::new(), 2, 1, 30).unwrap();
        let start = p.w.len();
        p.write_frame(1234, &[0; 8]).unwrap();
        let c = &p.w[start..];
        assert_eq!(&c[..4], &[0x1F, 0x43, 0xB6, 0x75]);
        let size = u64::from_be_bytes([0, c[5], c[6], c[7], c[8], c[9], c[10], c[11]]);
        assert_eq!(size as usize, c.len() - 12);
    }
}
