//! FDMV ファイルの読み込み。

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use crate::block::{BlockHeader, read_block, read_block_header, verify_block};
use crate::error::{Error, Result, malformed};
use crate::format::{
    BLOCK_CLUSTER, BLOCK_DIRECTORY, BLOCK_FOOTER, BLOCK_OVERHEAD, Dec, FOOTER_SIZE, HEADER_SIZE,
    MAGIC, MAX_CLUSTER_SIZE, MAX_DIRECTORY_SIZE, PACKET_HEADER_SIZE, VERSION_MAJOR,
};
use crate::model::{Directory, IndexEntry, Packet};

const CLUSTER_CACHE: usize = 4;

pub struct FdmvReader<R: Read + Seek> {
    r: R,
    file_len: u64,
    version: (u16, u16),
    dir: Directory,
    dir_offset: u64,
    /// 有効なフッタの直後の位置（通常はファイル長）。
    valid_end: u64,
    recovered: bool,
    cache: VecDeque<(u64, Arc<Vec<u8>>)>,
}

impl FdmvReader<BufReader<File>> {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::new(BufReader::new(File::open(path)?))
    }
}

impl<R: Read + Seek> FdmvReader<R> {
    pub fn new(mut r: R) -> Result<Self> {
        let file_len = r.seek(SeekFrom::End(0))?;
        if file_len < HEADER_SIZE {
            return Err(Error::NotFdmv);
        }
        r.seek(SeekFrom::Start(0))?;
        let mut h = [0u8; HEADER_SIZE as usize];
        r.read_exact(&mut h)?;
        if h[0..8] != MAGIC {
            return Err(Error::NotFdmv);
        }
        let major = u16::from_le_bytes([h[8], h[9]]);
        let minor = u16::from_le_bytes([h[10], h[11]]);
        if major != VERSION_MAJOR {
            return Err(Error::UnsupportedVersion { major, minor });
        }

        let (dir, dir_offset, valid_end, recovered) = match load_directory_at_footer(
            &mut r,
            file_len - FOOTER_SIZE.min(file_len),
            file_len,
        ) {
            Ok((dir, off)) => (dir, off, file_len, false),
            Err(first_err) => match recover_directory(&mut r, file_len)? {
                Some((dir, off, end)) => (dir, off, end, true),
                None => return Err(first_err),
            },
        };
        dir.validate()?;
        Ok(FdmvReader {
            r,
            file_len,
            version: (major, minor),
            dir,
            dir_offset,
            valid_end,
            recovered,
            cache: VecDeque::new(),
        })
    }

    pub fn directory(&self) -> &Directory {
        &self.dir
    }
    pub fn version(&self) -> (u16, u16) {
        self.version
    }
    pub fn file_len(&self) -> u64 {
        self.file_len
    }
    /// 有効なディレクトリ（DIRC ブロック）の位置。
    pub fn directory_offset(&self) -> u64 {
        self.dir_offset
    }
    /// 有効なフッタの終わり。末尾のフッタが壊れていてリカバリした場合はファイル長より小さい。
    pub fn valid_end(&self) -> u64 {
        self.valid_end
    }
    /// 末尾のフッタが壊れており、それより前のディレクトリで開いた場合に true。
    pub fn was_recovered(&self) -> bool {
        self.recovered
    }
    pub fn into_inner(self) -> R {
        self.r
    }

    fn cluster_payload(&mut self, offset: u64) -> Result<Arc<Vec<u8>>> {
        if let Some(i) = self.cache.iter().position(|(o, _)| *o == offset) {
            let entry = self.cache.remove(i).unwrap();
            let data = entry.1.clone();
            self.cache.push_front(entry);
            return Ok(data);
        }
        let data = Arc::new(read_block(
            &mut self.r,
            offset,
            BLOCK_CLUSTER,
            MAX_CLUSTER_SIZE,
            self.file_len,
        )?);
        self.cache.push_front((offset, data.clone()));
        self.cache.truncate(CLUSTER_CACHE);
        Ok(data)
    }

    /// クラスタを読み、`(クラスタ内の番号, パケット)` を返す。`stream` を指定するとそのストリームのみ。
    pub fn read_cluster(&mut self, offset: u64, stream: Option<u16>) -> Result<Vec<(u32, Packet)>> {
        let payload = self.cluster_payload(offset)?;
        parse_cluster(&payload, stream)
    }

    pub fn cursor(&self, stream_id: u16) -> StreamCursor {
        StreamCursor::new(&self.dir, stream_id)
    }

    /// ヘッダ直後からブロックを順に走査し、すべての CRC を検証する。
    pub fn scan_blocks(&mut self) -> Result<Vec<BlockHeader>> {
        let mut out = Vec::new();
        let mut pos = HEADER_SIZE;
        while pos < self.file_len {
            if self.file_len - pos < BLOCK_OVERHEAD {
                return malformed(format!("trailing garbage at offset {pos}"));
            }
            let h = read_block_header(&mut self.r, pos)?;
            if h.end() > self.file_len {
                return malformed(format!("block at offset {pos} extends past end of file"));
            }
            verify_block(&mut self.r, &h)?;
            pos = h.end();
            out.push(h);
        }
        Ok(out)
    }
}

fn load_directory_at_footer<R: Read + Seek>(
    r: &mut R,
    footer_offset: u64,
    file_len: u64,
) -> Result<(Directory, u64)> {
    let foot = read_block(r, footer_offset, BLOCK_FOOTER, 8, file_len)?;
    let dir_offset = u64::from_le_bytes(foot[..8].try_into().unwrap());
    if dir_offset < HEADER_SIZE || dir_offset >= footer_offset {
        return malformed("footer points outside the file");
    }
    let payload = read_block(r, dir_offset, BLOCK_DIRECTORY, MAX_DIRECTORY_SIZE, file_len)?;
    Ok((Directory::decode(&payload)?, dir_offset))
}

/// 末尾から後ろ向きに走査し、有効なフッタとディレクトリを探す。
fn recover_directory<R: Read + Seek>(
    r: &mut R,
    file_len: u64,
) -> Result<Option<(Directory, u64, u64)>> {
    const CHUNK: u64 = 1 << 20;
    let pattern: [u8; 12] = {
        let mut p = [0u8; 12];
        p[..4].copy_from_slice(&BLOCK_FOOTER);
        p[4..].copy_from_slice(&8u64.to_le_bytes());
        p
    };
    let mut end = file_len;
    let mut buf = Vec::new();
    while end > HEADER_SIZE {
        let start = end.saturating_sub(CHUNK).max(HEADER_SIZE);
        // チャンク境界をまたぐ一致を拾うため、少し重ねて読む。
        let read_end = (end + pattern.len() as u64 - 1).min(file_len);
        buf.resize((read_end - start) as usize, 0);
        r.seek(SeekFrom::Start(start))?;
        r.read_exact(&mut buf)?;
        for i in (0..buf.len().saturating_sub(pattern.len() - 1)).rev() {
            if buf[i..i + pattern.len()] != pattern {
                continue;
            }
            let off = start + i as u64;
            if off + FOOTER_SIZE > file_len {
                continue;
            }
            if let Ok((dir, dir_off)) = load_directory_at_footer(r, off, file_len) {
                return Ok(Some((dir, dir_off, off + FOOTER_SIZE)));
            }
        }
        end = start;
    }
    Ok(None)
}

pub fn parse_cluster(payload: &[u8], stream: Option<u16>) -> Result<Vec<(u32, Packet)>> {
    let mut d = Dec::new(payload);
    let n = d.u32()?;
    if n as usize > d.remaining() / PACKET_HEADER_SIZE {
        return malformed("cluster packet count exceeds cluster size");
    }
    let mut out = Vec::new();
    for i in 0..n {
        let stream_id = d.u16()?;
        let flags = d.u8()?;
        d.u8()?;
        let pts = d.i64()?;
        let duration = d.u32()?;
        let size = d.u32()? as usize;
        let data = d.take(size)?;
        if stream.is_none_or(|s| s == stream_id) {
            out.push((
                i,
                Packet {
                    stream_id,
                    flags,
                    pts,
                    duration,
                    data: data.to_vec(),
                },
            ));
        }
    }
    Ok(out)
}

/// 1 つのストリームのパケットを順に読むカーソル。
///
/// 複数のカーソルが 1 つの [`FdmvReader`] を共有できるよう、読み込みのたびにリーダを受け取る。
#[derive(Clone, Debug)]
pub struct StreamCursor {
    stream_id: u16,
    /// このストリームのインデックスエントリ（ファイル内の順序）。
    entries: Vec<IndexEntry>,
    /// このストリームを含むクラスタの位置（ファイル内の順序）。
    clusters: Vec<u64>,
    next_cluster: usize,
    buffer: VecDeque<Packet>,
}

impl StreamCursor {
    pub fn new(dir: &Directory, stream_id: u16) -> Self {
        let entries = dir.index_of(stream_id).to_vec();
        let mut clusters: Vec<u64> = entries.iter().map(|e| e.cluster_offset).collect();
        clusters.dedup();
        StreamCursor {
            stream_id,
            entries,
            clusters,
            next_cluster: 0,
            buffer: VecDeque::new(),
        }
    }

    pub fn stream_id(&self) -> u16 {
        self.stream_id
    }

    pub fn entries(&self) -> &[IndexEntry] {
        &self.entries
    }

    pub fn rewind(&mut self) {
        self.next_cluster = 0;
        self.buffer.clear();
    }

    /// 指定したインデックスエントリのパケットから読み始める。
    pub fn seek_to_entry<R: Read + Seek>(
        &mut self,
        reader: &mut FdmvReader<R>,
        entry: &IndexEntry,
    ) -> Result<()> {
        let Ok(ci) = self.clusters.binary_search(&entry.cluster_offset) else {
            return malformed("index entry refers to a cluster not indexed for this stream");
        };
        self.buffer = reader
            .read_cluster(entry.cluster_offset, Some(self.stream_id))?
            .into_iter()
            .filter(|(i, _)| *i >= entry.packet_index)
            .map(|(_, p)| p)
            .collect();
        self.next_cluster = ci + 1;
        Ok(())
    }

    /// `pred` を満たす最後のエントリから読み始め、そのエントリを返す。
    /// 該当するエントリがなければ先頭に戻して None を返す。
    pub fn seek_last_where<R: Read + Seek>(
        &mut self,
        reader: &mut FdmvReader<R>,
        pred: impl Fn(&IndexEntry) -> bool,
    ) -> Result<Option<IndexEntry>> {
        match self.entries.iter().rev().find(|e| pred(e)).copied() {
            Some(e) => {
                self.seek_to_entry(reader, &e)?;
                Ok(Some(e))
            }
            None => {
                self.rewind();
                Ok(None)
            }
        }
    }

    fn fill<R: Read + Seek>(&mut self, reader: &mut FdmvReader<R>) -> Result<bool> {
        while self.buffer.is_empty() {
            let Some(&off) = self.clusters.get(self.next_cluster) else {
                return Ok(false);
            };
            self.next_cluster += 1;
            self.buffer = reader
                .read_cluster(off, Some(self.stream_id))?
                .into_iter()
                .map(|(_, p)| p)
                .collect();
        }
        Ok(true)
    }

    pub fn peek<R: Read + Seek>(&mut self, reader: &mut FdmvReader<R>) -> Result<Option<&Packet>> {
        Ok(if self.fill(reader)? {
            self.buffer.front()
        } else {
            None
        })
    }

    pub fn next_packet<R: Read + Seek>(
        &mut self,
        reader: &mut FdmvReader<R>,
    ) -> Result<Option<Packet>> {
        Ok(if self.fill(reader)? {
            self.buffer.pop_front()
        } else {
            None
        })
    }
}
