//! パケット、ストリーム定義、ディレクトリ。

use std::collections::HashSet;

use crate::error::{Result, invalid, malformed};
use crate::format::{
    CODEC_AV1, CODEC_OPUS, Dec, Enc, INDEX_ENTRY_SIZE, KIND_CHAIN, KIND_VIDEO, MAX_CHAIN_CHANNELS,
    MAX_NAME_LEN, Rational, SEGMENT_SIZE, flags,
};

pub type Metadata = Vec<(String, String)>;

#[derive(Clone, Debug, PartialEq)]
pub struct Packet {
    pub stream_id: u16,
    pub flags: u8,
    pub pts: i64,
    pub duration: u32,
    pub data: Vec<u8>,
}

impl Packet {
    pub fn is_keyframe(&self) -> bool {
        self.flags & flags::KEYFRAME != 0
    }
    pub fn is_segment_start(&self) -> bool {
        self.flags & flags::SEGMENT_START != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainRole {
    Default,
    Sub,
}

impl ChainRole {
    fn from_u8(v: u8) -> Result<Self> {
        match v {
            0 => Ok(ChainRole::Default),
            1 => Ok(ChainRole::Sub),
            _ => malformed(format!("unknown chain role {v}")),
        }
    }
    fn to_u8(self) -> u8 {
        match self {
            ChainRole::Default => 0,
            ChainRole::Sub => 1,
        }
    }
}

/// チェーン内で音声が存在する区間。単位はチェーンのタイムベース（1/48000 秒）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    /// タイムライン上の、最初に鳴るサンプルの位置。
    pub start: i64,
    /// 鳴るサンプル数。
    pub length: i64,
    pub pre_skip: u16,
}

impl Segment {
    pub fn end(&self) -> i64 {
        self.start + self.length
    }
    pub fn contains(&self, t: i64) -> bool {
        t >= self.start && t < self.end()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct VideoParams {
    pub width: u32,
    pub height: u32,
    pub frame_rate: Rational,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChainParams {
    pub role: ChainRole,
    pub channels: u8,
    pub gain_db: f32,
    pub segments: Vec<Segment>,
}

impl ChainParams {
    pub fn gain_linear(&self) -> f32 {
        db_to_linear(self.gain_db)
    }

    /// `t` を含むセグメント、なければ `t` より後の最初のセグメントの番号。
    pub fn segment_at_or_after(&self, t: i64) -> Option<usize> {
        self.segments.iter().position(|s| s.end() > t)
    }
}

pub fn db_to_linear(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

#[derive(Clone, Debug, PartialEq)]
pub enum StreamParams {
    Video(VideoParams),
    Chain(ChainParams),
    /// このバージョンが知らない kind。書き戻しのために生データを保持する。
    Other(Vec<u8>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct StreamEntry {
    pub id: u16,
    pub kind: u8,
    pub codec: u8,
    pub timebase: Rational,
    pub duration: i64,
    pub codec_private: Vec<u8>,
    pub name: String,
    pub meta: Metadata,
    pub params: StreamParams,
    /// 末尾の未知フィールド（新しい minor バージョンで追加されたもの）。
    pub extra: Vec<u8>,
}

impl StreamEntry {
    pub fn new_video(params: VideoParams, timebase: Rational) -> Self {
        StreamEntry {
            id: 0,
            kind: KIND_VIDEO,
            codec: CODEC_AV1,
            timebase,
            duration: 0,
            codec_private: Vec::new(),
            name: String::new(),
            meta: Vec::new(),
            params: StreamParams::Video(params),
            extra: Vec::new(),
        }
    }

    pub fn new_chain(name: impl Into<String>, params: ChainParams) -> Self {
        StreamEntry {
            id: 0,
            kind: KIND_CHAIN,
            codec: CODEC_OPUS,
            timebase: Rational::OPUS,
            duration: 0,
            codec_private: Vec::new(),
            name: name.into(),
            meta: Vec::new(),
            params: StreamParams::Chain(params),
            extra: Vec::new(),
        }
    }

    pub fn is_video(&self) -> bool {
        self.kind == KIND_VIDEO
    }
    pub fn is_chain(&self) -> bool {
        self.kind == KIND_CHAIN
    }
    pub fn video(&self) -> Option<&VideoParams> {
        match &self.params {
            StreamParams::Video(v) => Some(v),
            _ => None,
        }
    }
    pub fn chain(&self) -> Option<&ChainParams> {
        match &self.params {
            StreamParams::Chain(c) => Some(c),
            _ => None,
        }
    }
    pub fn chain_mut(&mut self) -> Option<&mut ChainParams> {
        match &mut self.params {
            StreamParams::Chain(c) => Some(c),
            _ => None,
        }
    }
    pub fn meta(&self, key: &str) -> Option<&str> {
        meta_get(&self.meta, key)
    }
    pub fn duration_seconds(&self) -> f64 {
        self.timebase.to_seconds(self.duration)
    }

    fn encode(&self, e: &mut Enc) {
        e.u16(self.id);
        e.u8(self.kind);
        e.u8(self.codec);
        e.u32(self.timebase.num);
        e.u32(self.timebase.den);
        e.i64(self.duration);
        e.bytes(&self.codec_private);
        e.string(&self.name);
        encode_meta(e, &self.meta);
        match &self.params {
            StreamParams::Video(v) => {
                e.u32(v.width);
                e.u32(v.height);
                e.u32(v.frame_rate.num);
                e.u32(v.frame_rate.den);
            }
            StreamParams::Chain(c) => {
                e.u8(c.role.to_u8());
                e.u8(c.channels);
                e.u16(0);
                e.f32(c.gain_db);
                e.u32(c.segments.len() as u32);
                for s in &c.segments {
                    e.i64(s.start);
                    e.i64(s.length);
                    e.u16(s.pre_skip);
                    e.u16(0);
                }
            }
            StreamParams::Other(raw) => e.raw(raw),
        }
        e.raw(&self.extra);
    }

    fn decode(d: &mut Dec) -> Result<Self> {
        let id = d.u16()?;
        let kind = d.u8()?;
        let codec = d.u8()?;
        let timebase = Rational::new(d.u32()?, d.u32()?);
        let duration = d.i64()?;
        let codec_private = d.bytes()?.to_vec();
        let name = d.string()?;
        let meta = decode_meta(d)?;
        let params = match kind {
            KIND_VIDEO => StreamParams::Video(VideoParams {
                width: d.u32()?,
                height: d.u32()?,
                frame_rate: Rational::new(d.u32()?, d.u32()?),
            }),
            KIND_CHAIN => {
                let role = ChainRole::from_u8(d.u8()?)?;
                let channels = d.u8()?;
                d.u16()?;
                let gain_db = d.f32()?;
                let n = d.u32()? as usize;
                if n > d.remaining() / SEGMENT_SIZE {
                    return malformed("segment count exceeds record size");
                }
                let mut segments = Vec::with_capacity(n);
                for _ in 0..n {
                    let start = d.i64()?;
                    let length = d.i64()?;
                    let pre_skip = d.u16()?;
                    d.u16()?;
                    segments.push(Segment {
                        start,
                        length,
                        pre_skip,
                    });
                }
                StreamParams::Chain(ChainParams {
                    role,
                    channels,
                    gain_db,
                    segments,
                })
            }
            _ => StreamParams::Other(d.rest().to_vec()),
        };
        let extra = d.rest().to_vec();
        Ok(StreamEntry {
            id,
            kind,
            codec,
            timebase,
            duration,
            codec_private,
            name,
            meta,
            params,
            extra,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    pub stream_id: u16,
    pub flags: u8,
    pub packet_index: u32,
    pub pts: i64,
    pub cluster_offset: u64,
}

impl IndexEntry {
    pub fn is_keyframe(&self) -> bool {
        self.flags & flags::KEYFRAME != 0
    }
    pub fn is_segment_start(&self) -> bool {
        self.flags & flags::SEGMENT_START != 0
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Directory {
    pub streams: Vec<StreamEntry>,
    pub meta: Metadata,
    /// `(stream_id, cluster_offset, packet_index)` の昇順（＝ストリームごとのファイル内の順序）。
    pub index: Vec<IndexEntry>,
}

impl Directory {
    pub fn stream(&self, id: u16) -> Option<&StreamEntry> {
        self.streams.iter().find(|s| s.id == id)
    }
    pub fn stream_mut(&mut self, id: u16) -> Option<&mut StreamEntry> {
        self.streams.iter_mut().find(|s| s.id == id)
    }
    pub fn video(&self) -> Option<&StreamEntry> {
        self.streams.iter().find(|s| s.is_video())
    }
    pub fn chains(&self) -> impl Iterator<Item = &StreamEntry> {
        self.streams.iter().filter(|s| s.is_chain())
    }
    pub fn chain_by_name(&self, name: &str) -> Option<&StreamEntry> {
        self.chains().find(|s| s.name == name)
    }
    pub fn default_chain(&self) -> Option<&StreamEntry> {
        self.chains()
            .find(|s| s.chain().is_some_and(|c| c.role == ChainRole::Default))
    }
    pub fn meta(&self, key: &str) -> Option<&str> {
        meta_get(&self.meta, key)
    }
    /// タイムラインの長さ（映像の長さ）とそのタイムベース。
    pub fn timeline(&self) -> Option<(i64, Rational)> {
        self.video().map(|v| (v.duration, v.timebase))
    }
    pub fn duration_seconds(&self) -> f64 {
        self.timeline()
            .map(|(d, tb)| tb.to_seconds(d))
            .unwrap_or(0.0)
    }
    /// 指定ストリームのインデックスエントリ（ファイル内の順序）。
    pub fn index_of(&self, stream_id: u16) -> &[IndexEntry] {
        let lo = self.index.partition_point(|e| e.stream_id < stream_id);
        let hi = self.index.partition_point(|e| e.stream_id <= stream_id);
        &self.index[lo..hi]
    }
    pub fn next_stream_id(&self) -> Result<u16> {
        match self.streams.iter().map(|s| s.id).max() {
            None => Ok(0),
            Some(u16::MAX) => invalid("too many streams"),
            Some(m) => Ok(m + 1),
        }
    }

    pub fn sort_index(&mut self) {
        self.index
            .sort_by_key(|e| (e.stream_id, e.cluster_offset, e.packet_index));
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::new();
        e.u32(self.streams.len() as u32);
        for s in &self.streams {
            let mut rec = Enc::new();
            s.encode(&mut rec);
            e.bytes(&rec.buf);
        }
        encode_meta(&mut e, &self.meta);
        e.u32(self.index.len() as u32);
        for x in &self.index {
            e.u16(x.stream_id);
            e.u8(x.flags);
            e.u8(0);
            e.u32(x.packet_index);
            e.i64(x.pts);
            e.u64(x.cluster_offset);
        }
        e.buf
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let mut d = Dec::new(data);
        let n = d.u32()? as usize;
        if n > d.remaining() / 4 {
            return malformed("stream count exceeds directory size");
        }
        let mut streams = Vec::with_capacity(n);
        for _ in 0..n {
            let rec = d.bytes()?;
            streams.push(StreamEntry::decode(&mut Dec::new(rec))?);
        }
        let meta = decode_meta(&mut d)?;
        let n = d.u32()? as usize;
        if n > d.remaining() / INDEX_ENTRY_SIZE {
            return malformed("index count exceeds directory size");
        }
        let mut index = Vec::with_capacity(n);
        for _ in 0..n {
            let stream_id = d.u16()?;
            let flags = d.u8()?;
            d.u8()?;
            index.push(IndexEntry {
                stream_id,
                flags,
                packet_index: d.u32()?,
                pts: d.i64()?,
                cluster_offset: d.u64()?,
            });
        }
        let mut dir = Directory {
            streams,
            meta,
            index,
        };
        dir.sort_index();
        Ok(dir)
    }

    /// 仕様上の制約をチェックする。
    pub fn validate(&self) -> Result<()> {
        let mut ids = HashSet::new();
        let mut names = HashSet::new();
        let mut videos = 0;
        let mut defaults = 0;
        for s in &self.streams {
            if !ids.insert(s.id) {
                return malformed(format!("duplicate stream id {}", s.id));
            }
            if !s.timebase.is_valid() {
                return malformed(format!("stream {} has invalid timebase", s.id));
            }
            validate_meta(&s.meta)?;
            match &s.params {
                StreamParams::Video(_) => videos += 1,
                StreamParams::Chain(c) => {
                    if s.name.is_empty() || s.name.len() > MAX_NAME_LEN {
                        return malformed(format!(
                            "chain {} name must be 1..={MAX_NAME_LEN} bytes",
                            s.id
                        ));
                    }
                    if !names.insert(s.name.as_str()) {
                        return malformed(format!("duplicate chain name {:?}", s.name));
                    }
                    if s.timebase != Rational::OPUS {
                        return malformed(format!("chain {:?} timebase must be 1/48000", s.name));
                    }
                    if c.channels == 0 || c.channels > MAX_CHAIN_CHANNELS {
                        return malformed(format!(
                            "chain {:?} has unsupported channel count {}",
                            s.name, c.channels
                        ));
                    }
                    if !c.gain_db.is_finite() {
                        return malformed(format!("chain {:?} gain is not finite", s.name));
                    }
                    if c.role == ChainRole::Default {
                        defaults += 1;
                    }
                    for (i, seg) in c.segments.iter().enumerate() {
                        if seg.length <= 0 {
                            return malformed(format!(
                                "chain {:?} segment {i} has non-positive length",
                                s.name
                            ));
                        }
                        if i > 0 && c.segments[i - 1].end() > seg.start {
                            return malformed(format!(
                                "chain {:?} segments {} and {i} overlap",
                                s.name,
                                i - 1
                            ));
                        }
                    }
                }
                StreamParams::Other(_) => {}
            }
        }
        if videos != 1 {
            return malformed(format!("expected exactly one video stream, found {videos}"));
        }
        if defaults > 1 {
            return malformed("more than one default chain");
        }
        validate_meta(&self.meta)?;
        for e in &self.index {
            if !ids.contains(&e.stream_id) {
                return malformed(format!("index refers to unknown stream {}", e.stream_id));
            }
        }
        Ok(())
    }
}

pub fn meta_get<'a>(meta: &'a Metadata, key: &str) -> Option<&'a str> {
    meta.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// 同じキーがあれば置き換え、なければ追加する。
pub fn meta_set(meta: &mut Metadata, key: &str, value: &str) {
    match meta.iter_mut().find(|(k, _)| k == key) {
        Some(kv) => kv.1 = value.to_owned(),
        None => meta.push((key.to_owned(), value.to_owned())),
    }
}

pub fn is_valid_meta_key(key: &str) -> bool {
    !key.is_empty()
        && key.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-' | b'.')
        })
}

fn validate_meta(meta: &Metadata) -> Result<()> {
    for (k, _) in meta {
        if !is_valid_meta_key(k) {
            return malformed(format!("invalid metadata key {k:?}"));
        }
    }
    Ok(())
}

fn encode_meta(e: &mut Enc, meta: &Metadata) {
    e.u32(meta.len() as u32);
    for (k, v) in meta {
        e.string(k);
        e.string(v);
    }
}

fn decode_meta(d: &mut Dec) -> Result<Metadata> {
    let n = d.u32()? as usize;
    if n > d.remaining() / 8 {
        return malformed("metadata count exceeds data size");
    }
    let mut meta = Vec::with_capacity(n);
    for _ in 0..n {
        meta.push((d.string()?, d.string()?));
    }
    Ok(meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Directory {
        let mut video = StreamEntry::new_video(
            VideoParams {
                width: 1920,
                height: 1080,
                frame_rate: Rational::new(30000, 1001),
            },
            Rational::new(1001, 30000),
        );
        video.duration = 300;
        video.codec_private = vec![0x0a, 0x0b];
        let mut main = StreamEntry::new_chain(
            "main",
            ChainParams {
                role: ChainRole::Default,
                channels: 2,
                gain_db: 0.0,
                segments: vec![Segment {
                    start: 0,
                    length: 480_000,
                    pre_skip: 312,
                }],
            },
        );
        main.id = 1;
        let mut sub = StreamEntry::new_chain(
            "解説",
            ChainParams {
                role: ChainRole::Sub,
                channels: 1,
                gain_db: -3.0,
                segments: vec![
                    Segment {
                        start: 48_000,
                        length: 96_000,
                        pre_skip: 312,
                    },
                    Segment {
                        start: 200_000,
                        length: 10_000,
                        pre_skip: 312,
                    },
                ],
            },
        );
        sub.id = 2;
        sub.meta.push(("language".into(), "ja".into()));
        Directory {
            streams: vec![video, main, sub],
            meta: vec![("title".into(), "テスト".into())],
            index: vec![
                IndexEntry {
                    stream_id: 0,
                    flags: 1,
                    packet_index: 0,
                    pts: 0,
                    cluster_offset: 16,
                },
                IndexEntry {
                    stream_id: 2,
                    flags: 2,
                    packet_index: 3,
                    pts: 47_688,
                    cluster_offset: 99,
                },
            ],
        }
    }

    #[test]
    fn directory_roundtrip() {
        let dir = sample();
        dir.validate().unwrap();
        let back = Directory::decode(&dir.encode()).unwrap();
        assert_eq!(back, dir);
        assert_eq!(
            back.chain_by_name("解説").unwrap().meta("language"),
            Some("ja")
        );
        assert_eq!(back.default_chain().unwrap().name, "main");
    }

    #[test]
    fn unknown_trailing_fields_are_preserved() {
        let mut dir = sample();
        dir.streams[1].extra = vec![1, 2, 3];
        let back = Directory::decode(&dir.encode()).unwrap();
        assert_eq!(back.streams[1].extra, vec![1, 2, 3]);
    }

    #[test]
    fn overlapping_segments_are_rejected() {
        let mut dir = sample();
        dir.streams[2].chain_mut().unwrap().segments[1].start = 100_000;
        assert!(dir.validate().is_err());
    }
}
