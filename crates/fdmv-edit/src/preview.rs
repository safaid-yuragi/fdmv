//! タイムラインの音声ミックスと映像（プロキシを使う）。プレビュー再生と書き出しで共通に使う。

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::BufReader;

use anyhow::Result;
use fdmv_gui::{Frame, FrameSource, PcmSource};
use libfdmv::FdmvReader;
use libfdmv::decode::VideoStream;
use libfdmv::format::Rational;
use libfdmv::model::db_to_linear;

use crate::project::{Chain, ChainRole, Id, Project, Source, VideoClip};
use crate::proxy::ProxyStore;
use crate::wavfile::WavFile;

pub const RATE: f64 = 48_000.0;

fn samples(secs: f64) -> i64 {
    (secs * RATE).round() as i64
}

/// ミックスする 1 つの音声区間（単位は 48 kHz サンプル）。
#[derive(Clone, Debug, PartialEq)]
pub struct AudioItem {
    pub source: Id,
    pub start: i64,
    pub src_in: i64,
    pub len: i64,
    pub gain: f32,
}

impl AudioItem {
    pub fn end(&self) -> i64 {
        self.start + self.len
    }
}

/// チェーンに含まれる音声区間。`with_chain_gain` が true ならチェーンのゲインも掛ける。
pub fn chain_items(project: &Project, chain: &Chain, with_chain_gain: bool) -> Vec<AudioItem> {
    let cg = if with_chain_gain {
        db_to_linear(chain.gain_db)
    } else {
        1.0
    };
    let mut items = Vec::new();
    if chain.role == ChainRole::Default && chain.video_audio {
        for (start, c) in project.video_layout() {
            if project.source(c.source).is_some_and(|s| s.has_audio) {
                items.push(AudioItem {
                    source: c.source,
                    start: samples(start),
                    src_in: samples(c.src_in),
                    len: samples(start + c.duration()) - samples(start),
                    gain: cg,
                });
            }
        }
    }
    for c in &chain.clips {
        items.push(AudioItem {
            source: c.source,
            start: samples(c.start),
            src_in: samples(c.src_in),
            len: samples(c.end()) - samples(c.start),
            gain: cg * db_to_linear(c.gain_db),
        });
    }
    items
}

/// 区間の和集合。`gap` サンプル未満の隙間はつなげる。
pub fn merge_ranges(items: &[AudioItem], gap: i64, limit: i64) -> Vec<(i64, i64)> {
    let mut r: Vec<(i64, i64)> = items
        .iter()
        .map(|i| (i.start.max(0), i.end().min(limit)))
        .filter(|(a, b)| a < b)
        .collect();
    r.sort();
    let mut out: Vec<(i64, i64)> = Vec::new();
    for (a, b) in r {
        match out.last_mut() {
            Some(last) if a <= last.1 + gap => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

/// 音声プロキシを開いて区間をミックスする。
pub struct Mixer {
    proxies: ProxyStore,
    sources: HashMap<Id, Source>,
    files: HashMap<Id, WavFile>,
}

impl Mixer {
    pub fn new(proxies: ProxyStore, project: &Project) -> Self {
        let mut m = Mixer {
            proxies,
            sources: HashMap::new(),
            files: HashMap::new(),
        };
        m.set_sources(project);
        m
    }

    pub fn set_sources(&mut self, project: &Project) {
        self.sources = project.sources.iter().map(|s| (s.id, s.clone())).collect();
        self.files.retain(|id, _| project.source(*id).is_some());
    }

    fn file(&mut self, source: Id) -> Result<Option<&mut WavFile>> {
        if !self.files.contains_key(&source) {
            let Some(path) = self
                .sources
                .get(&source)
                .and_then(|s| self.proxies.audio_path(s))
            else {
                return Ok(None);
            };
            if !path.exists() {
                // プロキシ作成中。できるまで無音。
                return Ok(None);
            }
            self.files.insert(source, WavFile::open(&path)?);
        }
        Ok(self.files.get_mut(&source))
    }

    /// `out`（ステレオ）に、`pos` からの区間の音を加算する。
    pub fn mix(&mut self, items: &[AudioItem], pos: i64, out: &mut [f32]) -> Result<()> {
        let n = (out.len() / 2) as i64;
        for it in items {
            let (a, b) = (pos.max(it.start), (pos + n).min(it.end()));
            if a >= b {
                continue;
            }
            let Some(f) = self.file(it.source)? else {
                continue;
            };
            let dst = &mut out[((a - pos) * 2) as usize..((b - pos) * 2) as usize];
            f.mix_into(it.src_in + (a - it.start), dst, it.gain)?;
        }
        Ok(())
    }
}

/// プレビュー用の音源。有効なチェーンをすべてミックスする。
pub struct TimelineAudio {
    mixer: Mixer,
    items: Vec<AudioItem>,
    pos: i64,
    end: i64,
}

impl TimelineAudio {
    pub fn new(proxies: ProxyStore, project: &Project, muted: &HashSet<Id>) -> Self {
        let mut t = TimelineAudio {
            mixer: Mixer::new(proxies, project),
            items: Vec::new(),
            pos: 0,
            end: 0,
        };
        t.set_project(project, muted);
        t
    }

    /// 編集内容を反映する。`muted` のチェーンは鳴らさない。
    pub fn set_project(&mut self, project: &Project, muted: &HashSet<Id>) {
        self.mixer.set_sources(project);
        self.items = project
            .chains
            .iter()
            .filter(|c| !muted.contains(&c.id))
            .flat_map(|c| chain_items(project, c, true))
            .collect();
        self.end = samples(project.duration());
    }
}

impl PcmSource for TimelineAudio {
    fn seek(&mut self, sample: i64) -> Result<()> {
        self.pos = sample.clamp(0, self.end);
        Ok(())
    }

    fn render(&mut self, out: &mut [f32]) -> Result<usize> {
        let n = ((out.len() / 2) as i64).min(self.end - self.pos).max(0) as usize;
        let out = &mut out[..n * 2];
        out.fill(0.0);
        self.mixer.mix(&self.items, self.pos, out)?;
        self.pos += n as i64;
        Ok(n)
    }
}

type ProxyDecoder = (FdmvReader<BufReader<File>>, VideoStream, Rational);

/// プレビュー用の映像源。映像クリップをプロキシからデコードしてつなげる。
pub struct TimelineVideo {
    proxies: ProxyStore,
    clips: Vec<(f64, VideoClip, Source)>,
    decoders: HashMap<Id, Option<ProxyDecoder>>,
    current: Option<usize>,
    /// プロキシがまだ無いクリップでは、灰色のフレームを 1 枚だけ出す。
    placeholder_sent: bool,
    buf: Vec<u8>,
}

impl TimelineVideo {
    pub fn new(proxies: ProxyStore, project: &Project) -> Self {
        let mut v = TimelineVideo {
            proxies,
            clips: Vec::new(),
            decoders: HashMap::new(),
            current: None,
            placeholder_sent: false,
            buf: Vec::new(),
        };
        v.set_project(project);
        v
    }

    pub fn set_project(&mut self, project: &Project) {
        self.clips = project
            .video_layout()
            .into_iter()
            .filter_map(|(s, c)| Some((s, c.clone(), project.source(c.source)?.clone())))
            .collect();
        // プロキシができていなかった素材は、次に使うときに開き直す。
        self.decoders.retain(|_, d| d.is_some());
        self.current = None;
    }

    fn decoder(&mut self, i: usize) -> Result<Option<&mut ProxyDecoder>> {
        let src = &self.clips[i].2;
        if !self.decoders.contains_key(&src.id) {
            let d = match self.proxies.video_path(src) {
                Some(p) if p.exists() => {
                    let reader = FdmvReader::open(&p)?;
                    let dir = reader.directory().clone();
                    let tb = dir
                        .video()
                        .map(|v| v.timebase)
                        .unwrap_or(Rational::new(1, 1));
                    Some((reader, VideoStream::new(&dir, 0)?, tb))
                }
                _ => None,
            };
            if d.is_none() {
                return Ok(None);
            }
            self.decoders.insert(src.id, d);
        }
        Ok(self.decoders.get_mut(&src.id).and_then(|d| d.as_mut()))
    }

    fn enter(&mut self, i: usize, src_t: f64) -> Result<()> {
        self.current = Some(i);
        self.placeholder_sent = false;
        if let Some((reader, stream, tb)) = self.decoder(i)? {
            let pts = tb.floor_seconds(src_t);
            stream.seek(reader, pts)?;
        }
        Ok(())
    }
}

impl FrameSource for TimelineVideo {
    fn seek(&mut self, seconds: f64) -> Result<()> {
        let mut start = 0.0;
        self.current = None;
        for i in 0..self.clips.len() {
            let c = &self.clips[i].1;
            let end = start + c.duration();
            // 終端ちょうどなら最後のフレームを見せる。
            if seconds < end || i + 1 == self.clips.len() {
                let local = (seconds - start).clamp(0.0, (c.duration() - 1e-3).max(0.0));
                let src_in = c.src_in;
                return self.enter(i, src_in + local);
            }
            start = end;
        }
        Ok(())
    }

    fn next_frame(&mut self) -> Result<Option<Frame>> {
        loop {
            let Some(i) = self.current else {
                return Ok(None);
            };
            let (start, clip) = (self.clips[i].0, self.clips[i].1.clone());
            let frame = match self.decoder(i)? {
                Some((reader, stream, tb)) => {
                    let tb = *tb;
                    stream
                        .next_frame(reader)?
                        .map(|f| (tb.to_seconds(f.pts()), f))
                }
                None => {
                    if self.placeholder_sent {
                        None
                    } else {
                        self.placeholder_sent = true;
                        let (w, h) = (16, 9);
                        return Ok(Some(Frame {
                            pts: start,
                            width: w,
                            height: h,
                            rgba: [48u8, 48, 48, 255].repeat(w * h),
                        }));
                    }
                }
            };
            match frame {
                Some((src_t, f)) if src_t < clip.src_out - 1e-4 => {
                    let pts = start + (src_t - clip.src_in).max(0.0);
                    return Ok(Some(fdmv_gui::frame_from_decoded(&f, pts, &mut self.buf)));
                }
                _ => {
                    // このクリップの終わり。次のクリップへ。
                    if i + 1 < self.clips.len() {
                        let next_in = self.clips[i + 1].1.src_in;
                        self.enter(i + 1, next_in)?;
                    } else {
                        self.current = None;
                    }
                }
            }
        }
    }
}
