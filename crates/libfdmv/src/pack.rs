//! 素材ファイルから FDMV を作る（pack）／既存ファイルにチェーンを追加する（add-chain）。
//!
//! エンコードは外部コマンドの ffmpeg に任せ、出力された IVF / Ogg Opus を解析して格納する。

use std::cmp::Ordering;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};

use crate::av1::Av1Analyzer;
use crate::error::{Result, invalid};
use crate::ffmpeg::{AudioEncodeOptions, Ffmpeg, ProbeInfo, VideoEncodeOptions};
use crate::format::{MAX_CHAIN_CHANNELS, Rational, flags, gcd};
use crate::ivf::IvfReader;
use crate::model::{
    ChainParams, ChainRole, Metadata, Packet, Segment, StreamEntry, VideoParams, is_valid_meta_key,
    meta_set,
};
use crate::opus::{OggOpusReader, OpusHead};
use crate::reader::FdmvReader;
use crate::writer::Muxer;

/// チェーンの 1 セグメントになる音声素材。
#[derive(Clone, Debug)]
pub struct SegmentSource {
    pub path: PathBuf,
    /// タイムライン上の開始位置（秒）。
    pub start: f64,
    /// 素材の何番目の音声ストリームを使うか。
    pub audio_index: usize,
    /// 素材の先頭から切り落とす秒数。
    pub trim_start: f64,
}

impl SegmentSource {
    pub fn new(path: impl Into<PathBuf>, start: f64) -> Self {
        SegmentSource {
            path: path.into(),
            start,
            audio_index: 0,
            trim_start: 0.0,
        }
    }

    /// 映像ファイル自身の音声を、映像と同期する位置に置くセグメント。
    /// 音声と映像の開始時刻のずれ（コンテナ上の start_time）を補正する。
    pub fn from_video_audio(path: impl Into<PathBuf>, probe: &ProbeInfo) -> Self {
        let offset = probe.audio_start.unwrap_or(0.0) - probe.video_start.unwrap_or(0.0);
        let mut s = SegmentSource::new(path, 0.0);
        if offset >= 0.0 {
            s.start = offset;
        } else {
            s.trim_start = -offset;
        }
        s
    }
}

#[derive(Clone, Debug)]
pub struct ChainSpec {
    pub name: String,
    pub role: ChainRole,
    pub gain_db: f32,
    pub meta: Metadata,
    pub segments: Vec<SegmentSource>,
    /// None なら全体のオプションを使う。
    pub audio: Option<AudioEncodeOptions>,
}

impl ChainSpec {
    pub fn new(name: impl Into<String>, role: ChainRole) -> Self {
        ChainSpec {
            name: name.into(),
            role,
            gain_db: 0.0,
            meta: Vec::new(),
            segments: Vec::new(),
            audio: None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct PackOptions {
    pub video: VideoEncodeOptions,
    pub audio: AudioEncodeOptions,
    pub file_meta: Metadata,
}

#[derive(Debug, Default)]
pub struct PackReport {
    pub encoder: String,
    pub file_size: u64,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug)]
pub enum AppendMode {
    /// 元のファイルの末尾に追記する。途中で中断されても追記前の状態として開ける。
    InPlace,
    /// 元のファイルをコピーした新しいファイルに書く。
    CopyTo(PathBuf),
}

/// 映像 `input` と `chains` から FDMV ファイル `output` を作る。
pub fn pack(
    ff: &Ffmpeg,
    input: &Path,
    chains: &[ChainSpec],
    opts: &PackOptions,
    output: &Path,
) -> Result<PackReport> {
    check_chain_specs(chains, &[], false)?;
    for (k, _) in &opts.file_meta {
        if !is_valid_meta_key(k) {
            return invalid(format!("invalid metadata key {k:?}"));
        }
    }
    let tmp = tempfile::tempdir()?;
    let mut report = PackReport::default();

    let ivf_path = tmp.path().join("video.ivf");
    report.encoder = ff.encode_video(input, &ivf_path, &opts.video)?;
    let mut sources = Vec::new();
    for (ci, spec) in chains.iter().enumerate() {
        sources.push(EncodedChain::encode(ff, spec, &opts.audio, tmp.path(), ci)?);
    }

    let partial = partial_path(output);
    let result = (|| -> Result<()> {
        let file = BufWriter::new(File::create(&partial)?);
        let mut mux = Muxer::new(file)?;
        let mut video = VideoSource::open(&ivf_path)?;
        let video_id = mux.add_stream(video.entry())?;
        video.stream_id = video_id;

        let mut chain_sources = Vec::new();
        for (spec, enc) in chains.iter().zip(sources) {
            let id = mux.add_stream(enc.entry(spec))?;
            chain_sources.push(ChainSource::new(id, enc)?);
        }

        merge_and_write(&mut mux, Some(&mut video), &mut chain_sources)?;

        let v = video.finish()?;
        let timeline = v.duration;
        let vtb = v.timebase;
        *mux.stream_mut(video_id).unwrap() = v;
        for c in &chain_sources {
            c.apply(mux.stream_mut(c.stream_id).unwrap());
            report
                .warnings
                .extend(c.warnings(timeline, vtb, mux.directory()));
        }

        let meta = mux.file_meta_mut();
        meta.extend(opts.file_meta.iter().cloned());
        meta_set(
            meta,
            "encoder",
            &format!(
                "libfdmv {} (ffmpeg {})",
                env!("CARGO_PKG_VERSION"),
                report.encoder
            ),
        );
        if crate::model::meta_get(meta, "created_at").is_none() {
            meta_set(meta, "created_at", &now_rfc3339());
        }
        mux.finish()?
            .into_inner()
            .map_err(|e| e.into_error())?
            .sync_all()?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&partial);
        return Err(e);
    }
    fs::rename(&partial, output)?;
    report.file_size = fs::metadata(output)?.len();
    Ok(report)
}

/// 既存の FDMV ファイルにチェーンを追加する。
pub fn add_chain(
    ff: &Ffmpeg,
    file: &Path,
    spec: &ChainSpec,
    audio: &AudioEncodeOptions,
    mode: &AppendMode,
) -> Result<PackReport> {
    let reader = FdmvReader::open(file)?;
    let dir = reader.directory().clone();
    let dir_offset = reader.directory_offset();
    let valid_end = reader.valid_end();
    drop(reader);

    let existing: Vec<&StreamEntry> = dir.chains().collect();
    check_chain_specs(
        std::slice::from_ref(spec),
        &existing,
        dir.default_chain().is_some(),
    )?;

    let tmp = tempfile::tempdir()?;
    let enc = EncodedChain::encode(ff, spec, audio, tmp.path(), 0)?;
    let mut report = PackReport::default();
    let (timeline, vtb) = dir.timeline().unwrap();

    let write = |w: File, offset: u64, report: &mut PackReport| -> Result<File> {
        let mut mux = Muxer::append(BufWriter::new(w), dir.clone(), offset)?;
        let id = mux.add_stream(enc.entry(spec))?;
        let mut source = ChainSource::new(id, enc.reopen()?)?;
        merge_and_write(&mut mux, None, std::slice::from_mut(&mut source))?;
        source.apply(mux.stream_mut(id).unwrap());
        report
            .warnings
            .extend(source.warnings(timeline, vtb, mux.directory()));
        let w = mux.finish()?.into_inner().map_err(|e| e.into_error())?;
        w.sync_all()?;
        Ok(w)
    };

    match mode {
        AppendMode::InPlace => {
            let f = OpenOptions::new().read(true).write(true).open(file)?;
            // 壊れた追記の残骸があれば取り除く。
            if f.metadata()?.len() != valid_end {
                f.set_len(valid_end)?;
            }
            write(f, valid_end, &mut report)?;
            report.file_size = fs::metadata(file)?.len();
        }
        AppendMode::CopyTo(out) => {
            let partial = partial_path(out);
            let result = (|| -> Result<()> {
                let mut src = File::open(file)?;
                let mut dst = File::create(&partial)?;
                std::io::copy(&mut (&mut src).take(dir_offset), &mut dst)?;
                write(dst, dir_offset, &mut report)?;
                Ok(())
            })();
            if let Err(e) = result {
                let _ = fs::remove_file(&partial);
                return Err(e);
            }
            fs::rename(&partial, out)?;
            report.file_size = fs::metadata(out)?.len();
        }
    }
    Ok(report)
}

fn partial_path(output: &Path) -> PathBuf {
    let mut name = output.file_name().unwrap_or_default().to_os_string();
    name.push(".partial");
    output.with_file_name(name)
}

fn check_chain_specs(
    specs: &[ChainSpec],
    existing: &[&StreamEntry],
    has_default: bool,
) -> Result<()> {
    let mut names: Vec<&str> = existing.iter().map(|s| s.name.as_str()).collect();
    let mut defaults = has_default as usize;
    for s in specs {
        if s.name.is_empty() || s.name.len() > crate::format::MAX_NAME_LEN {
            return invalid(format!("chain name {:?} must be 1..=255 bytes", s.name));
        }
        if names.contains(&s.name.as_str()) {
            return invalid(format!("chain {:?} already exists", s.name));
        }
        names.push(&s.name);
        if s.role == ChainRole::Default {
            defaults += 1;
        }
        if s.segments.is_empty() {
            return invalid(format!("chain {:?} has no audio source", s.name));
        }
        if !s.gain_db.is_finite() {
            return invalid(format!("chain {:?} gain must be finite", s.name));
        }
        for (k, _) in &s.meta {
            if !is_valid_meta_key(k) {
                return invalid(format!("invalid metadata key {k:?}"));
            }
        }
        if let Some(a) = &s.audio
            && (a.channels == 0 || a.channels > MAX_CHAIN_CHANNELS)
        {
            return invalid("chain channels must be 1 or 2");
        }
    }
    if defaults > 1 {
        return invalid("only one default chain is allowed");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 映像

struct VideoSource {
    stream_id: u16,
    reader: IvfReader<BufReader<File>>,
    analyzer: Av1Analyzer,
    width: u32,
    height: u32,
    timebase: Rational,
    first_pts: Option<i64>,
    /// 次に出すフレーム（duration を決めるため 1 枚先読みする）。
    current: Option<(i64, Vec<u8>, bool)>,
    peeked: Option<Packet>,
    seq_header: Option<Vec<u8>>,
    frames: u64,
    last_end: i64,
    last_duration: u32,
}

impl VideoSource {
    fn open(path: &Path) -> Result<Self> {
        let reader = IvfReader::new(BufReader::new(File::open(path)?))?;
        if &reader.header.fourcc != b"AV01" {
            return invalid("encoded video is not AV1");
        }
        let h = &reader.header;
        Ok(VideoSource {
            stream_id: 0,
            width: h.width as u32,
            height: h.height as u32,
            timebase: h.timebase,
            reader,
            analyzer: Av1Analyzer::default(),
            first_pts: None,
            current: None,
            peeked: None,
            seq_header: None,
            frames: 0,
            last_end: 0,
            last_duration: 1,
        })
    }

    fn entry(&self) -> StreamEntry {
        StreamEntry::new_video(
            VideoParams {
                width: self.width,
                height: self.height,
                frame_rate: Rational::new(0, 1),
            },
            self.timebase,
        )
    }

    fn read_frame(&mut self) -> Result<Option<(i64, Vec<u8>, bool)>> {
        let Some(f) = self.reader.next_frame()? else {
            return Ok(None);
        };
        let first = *self.first_pts.get_or_insert(f.pts);
        let info = self.analyzer.analyze(&f.data)?;
        if self.seq_header.is_none() {
            self.seq_header = info.sequence_header;
        }
        Ok(Some((f.pts - first, f.data, info.is_keyframe)))
    }

    fn peek(&mut self) -> Result<Option<&Packet>> {
        if self.peeked.is_none() {
            if self.current.is_none() {
                self.current = self.read_frame()?;
                if let Some((_, _, key)) = &self.current
                    && !key
                {
                    return invalid("the first video frame is not a keyframe");
                }
            }
            if let Some((pts, data, key)) = self.current.take() {
                self.current = self.read_frame()?;
                let duration = match &self.current {
                    Some((next, _, _)) if *next > pts => (*next - pts) as u32,
                    Some(_) => return invalid("video timestamps are not increasing"),
                    None => self.last_duration,
                };
                self.last_duration = duration;
                self.last_end = pts + duration as i64;
                self.frames += 1;
                self.peeked = Some(Packet {
                    stream_id: self.stream_id,
                    flags: if key { flags::KEYFRAME } else { 0 },
                    pts,
                    duration,
                    data,
                });
            }
        }
        Ok(self.peeked.as_ref())
    }

    fn finish(self) -> Result<StreamEntry> {
        if self.frames == 0 {
            return invalid("the video has no frames");
        }
        let mut e = self.entry();
        e.id = self.stream_id;
        e.duration = self.last_end;
        e.codec_private = self.seq_header.unwrap_or_default();
        // 平均フレームレート = フレーム数 / 長さ
        let num = self.frames * self.timebase.den as u64;
        let den = self.last_end.max(1) as u64 * self.timebase.num as u64;
        let g = gcd(num, den).max(1);
        let (mut n, mut d) = (num / g, den / g);
        while n > u32::MAX as u64 || d > u32::MAX as u64 {
            n /= 2;
            d = (d / 2).max(1);
        }
        if let crate::model::StreamParams::Video(v) = &mut e.params {
            v.frame_rate = Rational::new(n as u32, d as u32);
        }
        Ok(e)
    }
}

// ---------------------------------------------------------------------------
// チェーン

struct EncodedSegment {
    path: PathBuf,
    start: i64,
    length: i64,
    head: OpusHead,
}

struct EncodedChain {
    channels: u8,
    segments: Vec<EncodedSegment>,
}

impl EncodedChain {
    fn encode(
        ff: &Ffmpeg,
        spec: &ChainSpec,
        default_audio: &AudioEncodeOptions,
        tmp: &Path,
        chain_index: usize,
    ) -> Result<Self> {
        let audio = spec.audio.as_ref().unwrap_or(default_audio);
        let mut segments = Vec::new();
        for (si, src) in spec.segments.iter().enumerate() {
            if !src.start.is_finite() || src.start < 0.0 {
                return invalid(format!("chain {:?}: segment start must be >= 0", spec.name));
            }
            let path = tmp.join(format!("chain{chain_index}_seg{si}.opus"));
            ff.encode_audio(&src.path, src.audio_index, src.trim_start, &path, audio)?;
            let mut r = OggOpusReader::new(BufReader::new(File::open(&path)?))?;
            let mut packets = 0;
            while r.next_packet()?.is_some() {
                packets += 1;
            }
            let length = r.pcm_length();
            if packets == 0 || length == 0 {
                return invalid(format!("{}: no audio", src.path.display()));
            }
            let head = r.head.clone();
            if head.mapping_family != 0 || head.channels == 0 || head.channels > MAX_CHAIN_CHANNELS
            {
                return invalid(format!(
                    "{}: only mono/stereo Opus is supported",
                    src.path.display()
                ));
            }
            segments.push(EncodedSegment {
                path,
                start: Rational::OPUS.from_seconds(src.start),
                length,
                head,
            });
        }
        segments.sort_by_key(|s| s.start);
        for w in segments.windows(2) {
            if w[0].start + w[0].length > w[1].start {
                return invalid(format!(
                    "chain {:?}: segments at {:.3}s and {:.3}s overlap",
                    spec.name,
                    Rational::OPUS.to_seconds(w[0].start),
                    Rational::OPUS.to_seconds(w[1].start)
                ));
            }
        }
        let first = &segments[0].head;
        if segments
            .iter()
            .any(|s| s.head.channels != first.channels || s.head.output_gain != first.output_gain)
        {
            return invalid(format!(
                "chain {:?}: segments have different channel layouts",
                spec.name
            ));
        }
        Ok(EncodedChain {
            channels: first.channels,
            segments,
        })
    }

    fn reopen(&self) -> Result<Self> {
        Ok(EncodedChain {
            channels: self.channels,
            segments: self
                .segments
                .iter()
                .map(|s| EncodedSegment {
                    path: s.path.clone(),
                    start: s.start,
                    length: s.length,
                    head: s.head.clone(),
                })
                .collect(),
        })
    }

    fn entry(&self, spec: &ChainSpec) -> StreamEntry {
        let mut e = StreamEntry::new_chain(
            spec.name.clone(),
            ChainParams {
                role: spec.role,
                channels: self.channels,
                gain_db: spec.gain_db,
                segments: self.segment_table(),
            },
        );
        e.codec_private = self.segments[0].head.raw.clone();
        e.meta = spec.meta.clone();
        e.duration = self
            .segments
            .last()
            .map(|s| s.start + s.length)
            .unwrap_or(0);
        e
    }

    fn segment_table(&self) -> Vec<Segment> {
        self.segments
            .iter()
            .map(|s| Segment {
                start: s.start,
                length: s.length,
                pre_skip: s.head.pre_skip,
            })
            .collect()
    }
}

struct ChainSource {
    stream_id: u16,
    chain: EncodedChain,
    current: usize,
    reader: Option<OggOpusReader<BufReader<File>>>,
    peeked: Option<Packet>,
}

impl ChainSource {
    fn new(stream_id: u16, chain: EncodedChain) -> Result<Self> {
        Ok(ChainSource {
            stream_id,
            chain,
            current: 0,
            reader: None,
            peeked: None,
        })
    }

    fn peek(&mut self) -> Result<Option<&Packet>> {
        while self.peeked.is_none() {
            let Some(seg) = self.chain.segments.get(self.current) else {
                break;
            };
            let first = self.reader.is_none();
            if first {
                self.reader = Some(OggOpusReader::new(BufReader::new(File::open(&seg.path)?))?);
            }
            match self.reader.as_mut().unwrap().next_packet()? {
                Some(p) => {
                    self.peeked = Some(Packet {
                        stream_id: self.stream_id,
                        flags: if first { flags::SEGMENT_START } else { 0 },
                        pts: seg.start + p.pcm_pos,
                        duration: p.duration,
                        data: p.data,
                    });
                }
                None => {
                    self.reader = None;
                    self.current += 1;
                }
            }
        }
        Ok(self.peeked.as_ref())
    }

    fn apply(&self, entry: &mut StreamEntry) {
        if let Some(c) = entry.chain_mut() {
            c.segments = self.chain.segment_table();
        }
        entry.duration = self
            .chain
            .segments
            .last()
            .map(|s| s.start + s.length)
            .unwrap_or(0);
    }

    fn warnings(&self, timeline: i64, vtb: Rational, dir: &crate::model::Directory) -> Vec<String> {
        let name = dir
            .stream(self.stream_id)
            .map(|s| s.name.as_str())
            .unwrap_or("?");
        let end = vtb.rescale(timeline, Rational::OPUS);
        self.chain
            .segments
            .iter()
            .filter(|s| s.start + s.length > end)
            .map(|s| {
                format!(
                    "chain {name:?}: segment at {:.3}s extends past the end of the video and will be cut off",
                    Rational::OPUS.to_seconds(s.start)
                )
            })
            .collect()
    }
}

/// 映像とチェーンのパケットを時刻順に並べて書き込む。
fn merge_and_write<W: Write + Seek>(
    mux: &mut Muxer<W>,
    mut video: Option<&mut VideoSource>,
    chains: &mut [ChainSource],
) -> Result<()> {
    loop {
        let mut best: Option<(usize, i64, Rational)> = None;
        if let Some(v) = video.as_deref_mut()
            && let Some(p) = v.peek()?
        {
            best = Some((usize::MAX, p.pts, v.timebase));
        }
        for (i, c) in chains.iter_mut().enumerate() {
            if let Some(p) = c.peek()? {
                let better = match best {
                    None => true,
                    Some((_, t, tb)) => {
                        Rational::cmp_time(p.pts, Rational::OPUS, t, tb) == Ordering::Less
                    }
                };
                if better {
                    best = Some((i, p.pts, Rational::OPUS));
                }
            }
        }
        let Some((i, _, _)) = best else { break };
        let pkt = if i == usize::MAX {
            video.as_deref_mut().unwrap().peeked.take()
        } else {
            chains[i].peeked.take()
        };
        mux.write_packet(pkt.unwrap())?;
    }
    Ok(())
}

fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Howard Hinnant の civil_from_days
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}
