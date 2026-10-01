//! FDMV ファイルの書き込み（新規作成とチェーンの追記）。

use std::collections::{HashMap, HashSet};
use std::io::{Seek, SeekFrom, Write};

use crate::block::write_block;
use crate::error::{Result, invalid};
use crate::format::{
    BLOCK_CLUSTER, BLOCK_DIRECTORY, BLOCK_FOOTER, Enc, HEADER_SIZE, MAGIC, MAX_PACKET_SIZE,
    PACKET_HEADER_SIZE, Rational, VERSION_MAJOR, VERSION_MINOR, flags,
};
use crate::model::{Directory, IndexEntry, Packet, StreamEntry};

/// クラスタを区切る目安。
#[derive(Clone, Copy, Debug)]
pub struct ClusterPolicy {
    pub max_duration_secs: f64,
    pub max_bytes: usize,
}

impl Default for ClusterPolicy {
    fn default() -> Self {
        ClusterPolicy {
            max_duration_secs: 1.0,
            max_bytes: 8 << 20,
        }
    }
}

/// パケットを受け取り、クラスタ・ディレクトリ・フッタを書き出す。
///
/// パケットは呼び出し側がおおよそ時刻順に渡す。ストリームごとの順序はそのまま保存される。
/// ディレクトリはファイル末尾に書くので、ストリーム情報（duration やセグメント）は
/// [`Muxer::finish`] の前ならいつでも [`Muxer::stream_mut`] で更新できる。
pub struct Muxer<W: Write + Seek> {
    w: W,
    pos: u64,
    dir: Directory,
    policy: ClusterPolicy,
    cluster: Vec<Packet>,
    cluster_bytes: usize,
    cluster_start: Option<(i64, Rational)>,
    cluster_seen: HashSet<u16>,
    /// cluster_offset が未確定のインデックスエントリ。
    pending_index: Vec<IndexEntry>,
    last_pts: HashMap<u16, i64>,
    /// 追記モードで書き込みを許すストリーム（None なら全ストリーム）。
    writable: Option<HashSet<u16>>,
}

impl<W: Write + Seek> Muxer<W> {
    /// 新しいファイルを作る。`w` の先頭からヘッダを書く。
    pub fn new(mut w: W) -> Result<Self> {
        w.seek(SeekFrom::Start(0))?;
        let mut e = Enc::new();
        e.raw(&MAGIC);
        e.u16(VERSION_MAJOR);
        e.u16(VERSION_MINOR);
        e.u32(0);
        w.write_all(&e.buf)?;
        Ok(Self::with_state(w, HEADER_SIZE, Directory::default(), None))
    }

    /// 既存ファイルの `offset` 以降に追記する。`dir` は既存の有効なディレクトリ。
    /// 追記モードでは、このあと [`Muxer::add_stream`] で追加したストリームだけに書き込める。
    pub fn append(mut w: W, dir: Directory, offset: u64) -> Result<Self> {
        w.seek(SeekFrom::Start(offset))?;
        Ok(Self::with_state(w, offset, dir, Some(HashSet::new())))
    }

    fn with_state(w: W, pos: u64, dir: Directory, writable: Option<HashSet<u16>>) -> Self {
        Muxer {
            w,
            pos,
            dir,
            policy: ClusterPolicy::default(),
            cluster: Vec::new(),
            cluster_bytes: 0,
            cluster_start: None,
            cluster_seen: HashSet::new(),
            pending_index: Vec::new(),
            last_pts: HashMap::new(),
            writable,
        }
    }

    pub fn set_cluster_policy(&mut self, policy: ClusterPolicy) {
        self.policy = policy;
    }

    pub fn directory(&self) -> &Directory {
        &self.dir
    }

    pub fn file_meta_mut(&mut self) -> &mut Vec<(String, String)> {
        &mut self.dir.meta
    }

    /// ストリームを追加し、割り当てた stream_id を返す。
    pub fn add_stream(&mut self, mut entry: StreamEntry) -> Result<u16> {
        entry.id = self.dir.next_stream_id()?;
        let id = entry.id;
        self.dir.streams.push(entry);
        if let Some(w) = &mut self.writable {
            w.insert(id);
        }
        Ok(id)
    }

    pub fn stream_mut(&mut self, id: u16) -> Option<&mut StreamEntry> {
        self.dir.stream_mut(id)
    }

    pub fn write_packet(&mut self, pkt: Packet) -> Result<()> {
        let Some(stream) = self.dir.stream(pkt.stream_id) else {
            return invalid(format!("unknown stream id {}", pkt.stream_id));
        };
        if let Some(w) = &self.writable
            && !w.contains(&pkt.stream_id)
        {
            return invalid(format!(
                "stream {} is read-only in append mode",
                pkt.stream_id
            ));
        }
        if pkt.data.len() as u64 > MAX_PACKET_SIZE {
            return invalid(format!("packet of {} bytes is too large", pkt.data.len()));
        }
        // セグメントの先頭パケットは、前のセグメントの末尾パケットより前の pts を持ちうる
        // （pre_skip のため）。それ以外は pts が厳密に増加しなければならない。
        if let Some(&last) = self.last_pts.get(&pkt.stream_id)
            && pkt.pts <= last
            && !pkt.is_segment_start()
        {
            return invalid(format!(
                "stream {}: pts {} is not after previous pts {last}",
                pkt.stream_id, pkt.pts
            ));
        }
        let tb = stream.timebase;

        let size = PACKET_HEADER_SIZE + pkt.data.len();
        if let Some((t0, tb0)) = self.cluster_start {
            let span = tb.to_seconds(pkt.pts) - tb0.to_seconds(t0);
            if span >= self.policy.max_duration_secs
                || self.cluster_bytes + size > self.policy.max_bytes
            {
                self.flush_cluster()?;
            }
        }
        if self.cluster_start.is_none() {
            self.cluster_start = Some((pkt.pts, tb));
        }

        // 各クラスタでのストリームの最初のパケット、キーフレーム、セグメント先頭を索引に載せる。
        let index_flags = pkt.flags & (flags::KEYFRAME | flags::SEGMENT_START);
        let first_in_cluster = self.cluster_seen.insert(pkt.stream_id);
        if first_in_cluster || index_flags != 0 {
            self.pending_index.push(IndexEntry {
                stream_id: pkt.stream_id,
                flags: index_flags,
                packet_index: self.cluster.len() as u32,
                pts: pkt.pts,
                cluster_offset: 0,
            });
        }
        self.last_pts.insert(pkt.stream_id, pkt.pts);
        self.cluster_bytes += size;
        self.cluster.push(pkt);
        Ok(())
    }

    fn flush_cluster(&mut self) -> Result<()> {
        if self.cluster.is_empty() {
            return Ok(());
        }
        let mut e = Enc::new();
        e.buf.reserve(self.cluster_bytes + 4);
        e.u32(self.cluster.len() as u32);
        for p in &self.cluster {
            e.u16(p.stream_id);
            e.u8(p.flags);
            e.u8(0);
            e.i64(p.pts);
            e.u32(p.duration);
            e.u32(p.data.len() as u32);
            e.raw(&p.data);
        }
        let offset = self.pos;
        self.pos += write_block(&mut self.w, BLOCK_CLUSTER, &e.buf)?;
        for mut entry in self.pending_index.drain(..) {
            entry.cluster_offset = offset;
            self.dir.index.push(entry);
        }
        self.cluster.clear();
        self.cluster_bytes = 0;
        self.cluster_start = None;
        self.cluster_seen.clear();
        Ok(())
    }

    /// 残りのクラスタ、ディレクトリ、フッタを書き、ライタを返す。
    pub fn finish(mut self) -> Result<W> {
        self.flush_cluster()?;
        self.dir.sort_index();
        self.dir.validate()?;
        let dir_offset = self.pos;
        self.pos += write_block(&mut self.w, BLOCK_DIRECTORY, &self.dir.encode())?;
        self.pos += write_block(&mut self.w, BLOCK_FOOTER, &dir_offset.to_le_bytes())?;
        self.w.flush()?;
        Ok(self.w)
    }
}
