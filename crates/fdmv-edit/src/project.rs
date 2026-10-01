//! 編集プロジェクト（`.fdmvproj`、JSON）のデータ構造と編集操作。
//!
//! 時間はすべて秒（f64）。映像トラックはクリップを隙間なく順に並べたもので、
//! タイムライン上の位置はクリップの長さの累積で決まる。チェーンのクリップは絶対位置に置き、重なってもよい（ミックスされる）。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub type Id = u64;

pub const PROJECT_VERSION: u32 = 2;
/// クリップの最短の長さ（秒）。
pub const MIN_CLIP: f64 = 0.01;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Project {
    pub version: u32,
    pub settings: Settings,
    pub sources: Vec<Source>,
    pub video: Vec<VideoClip>,
    pub chains: Vec<Chain>,
    pub next_id: Id,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Settings {
    pub title: String,
    /// true なら解像度とフレームレートを最初の映像クリップの素材に合わせる。
    pub auto_format: bool,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub crf: u32,
    pub preset: Option<u32>,
    pub ten_bit: bool,
    pub audio_bitrate: String,
}

impl Default for Settings {
    fn default() -> Self {
        let (crf, preset) = libfdmv::ffmpeg::Quality::High.crf_preset();
        Settings {
            title: String::new(),
            auto_format: true,
            width: 1920,
            height: 1080,
            fps_num: 30,
            fps_den: 1,
            crf,
            preset: Some(preset),
            ten_bit: false,
            audio_bitrate: "128k".into(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SourceVideo {
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
}

/// 素材ファイル。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Source {
    pub id: Id,
    pub path: PathBuf,
    /// プロジェクトファイルからの相対パス（素材を見つけられないときに使う）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relative_path: Option<PathBuf>,
    pub duration: f64,
    pub video: Option<SourceVideo>,
    pub has_audio: bool,
    /// 映像の先頭（素材上の時刻 0）に対する音声の先頭のずれ（秒）。
    pub audio_offset: f64,
    /// ffmpeg の `-ss` の基準（ファイルの開始時刻）から映像の先頭までのずれ（秒）。
    pub video_ss_offset: f64,
}

impl Source {
    pub fn name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct VideoClip {
    pub id: Id,
    pub source: Id,
    pub src_in: f64,
    pub src_out: f64,
}

impl VideoClip {
    pub fn duration(&self) -> f64 {
        self.src_out - self.src_in
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ChainRole {
    Default,
    Sub,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Chain {
    pub id: Id,
    pub name: String,
    pub role: ChainRole,
    pub gain_db: f32,
    #[serde(default)]
    pub language: String,
    #[serde(default)]
    pub description: String,
    /// デフォルトチェーンに映像クリップの音声を含める。
    pub video_audio: bool,
    pub clips: Vec<AudioClip>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AudioClip {
    pub id: Id,
    pub source: Id,
    /// タイムライン上の開始位置。
    pub start: f64,
    pub src_in: f64,
    pub src_out: f64,
    pub gain_db: f32,
}

impl AudioClip {
    pub fn duration(&self) -> f64 {
        self.src_out - self.src_in
    }
    pub fn end(&self) -> f64 {
        self.start + self.duration()
    }
}

impl Default for Project {
    fn default() -> Self {
        Self::new()
    }
}

impl Project {
    pub fn new() -> Self {
        Project {
            version: PROJECT_VERSION,
            settings: Settings::default(),
            sources: Vec::new(),
            video: Vec::new(),
            chains: vec![Chain {
                id: 1,
                name: "main".into(),
                role: ChainRole::Default,
                gain_db: 0.0,
                language: String::new(),
                description: String::new(),
                video_audio: true,
                clips: Vec::new(),
            }],
            next_id: 2,
        }
    }

    // ------------------------------------------------------------------
    // 保存と読み込み

    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut p: Project =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if p.version > PROJECT_VERSION {
            bail!(
                "project version {} is newer than this editor supports",
                p.version
            );
        }
        if p.version < 2 {
            // v1 の既定画質（CRF 32、preset 未指定）は劣化が目立ったため、新しい既定値に置き換える。
            if p.settings.crf == 32 && p.settings.preset.is_none() {
                let d = Settings::default();
                p.settings.crf = d.crf;
                p.settings.preset = d.preset;
            }
            p.version = PROJECT_VERSION;
        }
        // 素材が移動していたら、プロジェクトファイルからの相対パスで探す。
        let base = path.parent().unwrap_or(Path::new("."));
        for s in &mut p.sources {
            if !s.path.exists()
                && let Some(rel) = &s.relative_path
                && base.join(rel).exists()
            {
                s.path = base.join(rel);
            }
        }
        Ok(p)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let mut p = self.clone();
        let base = path
            .parent()
            .map(|b| b.canonicalize().unwrap_or_else(|_| b.to_path_buf()));
        for s in &mut p.sources {
            s.relative_path = base
                .as_ref()
                .and_then(|b| s.path.strip_prefix(b).ok())
                .map(Path::to_path_buf);
        }
        let text = serde_json::to_string_pretty(&p)?;
        let tmp = path.with_extension("fdmvproj.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // 参照

    pub fn alloc_id(&mut self) -> Id {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn source(&self, id: Id) -> Option<&Source> {
        self.sources.iter().find(|s| s.id == id)
    }

    pub fn chain(&self, id: Id) -> Option<&Chain> {
        self.chains.iter().find(|c| c.id == id)
    }

    pub fn chain_mut(&mut self, id: Id) -> Option<&mut Chain> {
        self.chains.iter_mut().find(|c| c.id == id)
    }

    pub fn default_chain(&self) -> Option<&Chain> {
        self.chains.iter().find(|c| c.role == ChainRole::Default)
    }

    /// タイムライン全体の長さ（映像トラックの長さ）。
    pub fn duration(&self) -> f64 {
        self.video.iter().map(VideoClip::duration).sum()
    }

    /// 映像クリップと、そのタイムライン上の開始位置。
    pub fn video_layout(&self) -> Vec<(f64, &VideoClip)> {
        let mut t = 0.0;
        self.video
            .iter()
            .map(|c| {
                let s = t;
                t += c.duration();
                (s, c)
            })
            .collect()
    }

    /// 時刻 `t` を含む映像クリップの番号と開始位置。
    pub fn video_clip_at(&self, t: f64) -> Option<(usize, f64)> {
        let mut start = 0.0;
        for (i, c) in self.video.iter().enumerate() {
            let end = start + c.duration();
            if t >= start && t < end {
                return Some((i, start));
            }
            start = end;
        }
        None
    }

    /// 書き出しの解像度とフレームレート（幅, 高さ, fps 分子, fps 分母）。
    pub fn format(&self) -> (u32, u32, u32, u32) {
        let s = &self.settings;
        if s.auto_format
            && let Some(v) = self
                .video
                .first()
                .and_then(|c| self.source(c.source))
                .and_then(|s| s.video.as_ref())
        {
            // AV1 / yuv420p のため幅と高さは偶数にする。
            return (v.width & !1, v.height & !1, v.fps_num, v.fps_den);
        }
        (
            s.width & !1,
            s.height & !1,
            s.fps_num.max(1),
            s.fps_den.max(1),
        )
    }

    /// 1 フレームの長さ（秒）。
    pub fn frame_duration(&self) -> f64 {
        let (_, _, n, d) = self.format();
        d as f64 / n as f64
    }

    /// フレームの境界に丸める。
    pub fn snap(&self, t: f64) -> f64 {
        let fd = self.frame_duration();
        (t / fd).round() * fd
    }

    // ------------------------------------------------------------------
    // 素材

    pub fn add_source(&mut self, mut source: Source) -> Id {
        source.id = self.alloc_id();
        let id = source.id;
        self.sources.push(source);
        id
    }

    /// 素材と、それを使うクリップをすべて削除する。
    pub fn remove_source(&mut self, id: Id) {
        self.sources.retain(|s| s.id != id);
        self.video.retain(|c| c.source != id);
        for ch in &mut self.chains {
            ch.clips.retain(|c| c.source != id);
        }
    }

    // ------------------------------------------------------------------
    // 映像トラック

    /// 素材全体を映像クリップとして `index` 番目に挿入する（None なら末尾）。
    pub fn insert_video(&mut self, source: Id, index: Option<usize>) -> Result<Id> {
        let Some(s) = self.source(source) else {
            bail!("unknown source")
        };
        if s.video.is_none() {
            bail!("{} has no video", s.name());
        }
        let src_out = s.duration;
        let id = self.alloc_id();
        let clip = VideoClip {
            id,
            source,
            src_in: 0.0,
            src_out,
        };
        let index = index.unwrap_or(self.video.len()).min(self.video.len());
        self.video.insert(index, clip);
        Ok(id)
    }

    /// タイムライン上の時刻 `t` で映像クリップを分割する。分割したら新しいクリップの ID を返す。
    pub fn split_video(&mut self, t: f64) -> Option<Id> {
        let (i, start) = self.video_clip_at(t)?;
        let c = &self.video[i];
        let cut = c.src_in + (t - start);
        if cut - c.src_in < MIN_CLIP || c.src_out - cut < MIN_CLIP {
            return None;
        }
        let second = VideoClip {
            id: 0,
            source: c.source,
            src_in: cut,
            src_out: c.src_out,
        };
        self.video[i].src_out = cut;
        let id = self.alloc_id();
        self.video.insert(i + 1, VideoClip { id, ..second });
        Some(id)
    }

    /// 映像クリップを削除し、後ろを詰める。
    pub fn remove_video(&mut self, clip: Id) -> bool {
        let before = self.video.len();
        self.video.retain(|c| c.id != clip);
        self.video.len() != before
    }

    /// 映像クリップの使用範囲を変える。
    pub fn trim_video(&mut self, clip: Id, src_in: f64, src_out: f64) {
        let Some(i) = self.video.iter().position(|c| c.id == clip) else {
            return;
        };
        let dur = self
            .source(self.video[i].source)
            .map(|s| s.duration)
            .unwrap_or(f64::MAX);
        let c = &mut self.video[i];
        c.src_in = src_in.clamp(0.0, (dur - MIN_CLIP).max(0.0));
        c.src_out = src_out.clamp(c.src_in + MIN_CLIP, dur.max(c.src_in + MIN_CLIP));
    }

    /// 映像クリップを `to` 番目に移動する。
    pub fn move_video(&mut self, clip: Id, to: usize) {
        let Some(i) = self.video.iter().position(|c| c.id == clip) else {
            return;
        };
        let c = self.video.remove(i);
        let to = if to > i { to - 1 } else { to }.min(self.video.len());
        self.video.insert(to, c);
    }

    // ------------------------------------------------------------------
    // チェーン

    pub fn add_chain(&mut self, name: &str) -> Id {
        let id = self.alloc_id();
        let role = if self.default_chain().is_none() {
            ChainRole::Default
        } else {
            ChainRole::Sub
        };
        self.chains.push(Chain {
            id,
            name: name.to_owned(),
            role,
            gain_db: 0.0,
            language: String::new(),
            description: String::new(),
            video_audio: role == ChainRole::Default,
            clips: Vec::new(),
        });
        id
    }

    pub fn remove_chain(&mut self, id: Id) {
        self.chains.retain(|c| c.id != id);
    }

    /// `id` をデフォルトチェーンにする（ほかはサブになる）。
    pub fn set_default_chain(&mut self, id: Id) {
        for c in &mut self.chains {
            c.role = if c.id == id {
                ChainRole::Default
            } else {
                ChainRole::Sub
            };
        }
    }

    /// 使われていない名前を作る（例: "チェーン 2"）。
    pub fn unique_chain_name(&self, base: &str) -> String {
        (1..)
            .map(|i| {
                if i == 1 {
                    base.to_owned()
                } else {
                    format!("{base} {i}")
                }
            })
            .find(|n| !self.chains.iter().any(|c| &c.name == n))
            .unwrap()
    }

    /// 素材全体の音声をチェーンの `start` の位置に置く。
    pub fn add_audio(&mut self, chain: Id, source: Id, start: f64) -> Result<Id> {
        let Some(s) = self.source(source) else {
            bail!("unknown source")
        };
        if !s.has_audio {
            bail!("{} has no audio", s.name());
        }
        let dur = s.duration;
        let id = self.alloc_id();
        let Some(ch) = self.chain_mut(chain) else {
            bail!("unknown chain")
        };
        ch.clips.push(AudioClip {
            id,
            source,
            start: start.max(0.0),
            src_in: 0.0,
            src_out: dur,
            gain_db: 0.0,
        });
        ch.clips.sort_by(|a, b| a.start.total_cmp(&b.start));
        Ok(id)
    }

    pub fn audio_clip(&self, chain: Id, clip: Id) -> Option<&AudioClip> {
        self.chain(chain)?.clips.iter().find(|c| c.id == clip)
    }

    pub fn audio_clip_mut(&mut self, chain: Id, clip: Id) -> Option<&mut AudioClip> {
        self.chain_mut(chain)?
            .clips
            .iter_mut()
            .find(|c| c.id == clip)
    }

    pub fn remove_audio(&mut self, chain: Id, clip: Id) -> bool {
        let Some(ch) = self.chain_mut(chain) else {
            return false;
        };
        let before = ch.clips.len();
        ch.clips.retain(|c| c.id != clip);
        ch.clips.len() != before
    }

    pub fn move_audio(&mut self, chain: Id, clip: Id, start: f64) {
        if let Some(ch) = self.chain_mut(chain) {
            if let Some(c) = ch.clips.iter_mut().find(|c| c.id == clip) {
                c.start = start.max(0.0);
            }
            ch.clips.sort_by(|a, b| a.start.total_cmp(&b.start));
        }
    }

    /// 音声クリップの左端（`start` と `src_in`）または右端（`src_out`）を動かす。
    pub fn trim_audio(
        &mut self,
        chain: Id,
        clip: Id,
        new_start: Option<f64>,
        new_end: Option<f64>,
    ) {
        let durations: Vec<(Id, f64)> = self.sources.iter().map(|s| (s.id, s.duration)).collect();
        let Some(c) = self.audio_clip_mut(chain, clip) else {
            return;
        };
        let src_dur = durations
            .iter()
            .find(|(id, _)| *id == c.source)
            .map(|(_, d)| *d)
            .unwrap_or(f64::MAX);
        if let Some(s) = new_start {
            // 左端: 素材の先頭より前には伸ばせない。
            let delta = (s - c.start).max(-c.src_in).min(c.duration() - MIN_CLIP);
            c.start += delta;
            c.src_in += delta;
        }
        if let Some(e) = new_end {
            let out = c.src_in + (e - c.start);
            c.src_out = out.clamp(c.src_in + MIN_CLIP, src_dur);
        }
    }

    /// 音声クリップを時刻 `t` で分割する。
    pub fn split_audio(&mut self, chain: Id, clip: Id, t: f64) -> Option<Id> {
        let c = self.audio_clip(chain, clip)?.clone();
        if t - c.start < MIN_CLIP || c.end() - t < MIN_CLIP {
            return None;
        }
        let cut = c.src_in + (t - c.start);
        let id = self.alloc_id();
        let ch = self.chain_mut(chain)?;
        let first = ch.clips.iter_mut().find(|x| x.id == clip)?;
        first.src_out = cut;
        ch.clips.push(AudioClip {
            id,
            start: t,
            src_in: cut,
            ..c
        });
        ch.clips.sort_by(|a, b| a.start.total_cmp(&b.start));
        Some(id)
    }

    // ------------------------------------------------------------------
    // 範囲削除

    /// タイムラインの `[a, b)` をすべてのトラックから削除し、後ろを詰める。
    pub fn delete_range(&mut self, a: f64, b: f64) {
        let (a, b) = (a.min(b).max(0.0), a.max(b));
        let len = b - a;
        if len <= 0.0 {
            return;
        }
        // 映像トラック
        let mut out = Vec::new();
        let mut t = 0.0;
        for c in std::mem::take(&mut self.video) {
            let (s, e) = (t, t + c.duration());
            t = e;
            if e <= a || s >= b {
                out.push(c);
                continue;
            }
            if s < a && a - s >= MIN_CLIP {
                out.push(VideoClip {
                    src_out: c.src_in + (a - s),
                    ..c.clone()
                });
            }
            if e > b && e - b >= MIN_CLIP {
                let tail = VideoClip {
                    id: 0,
                    src_in: c.src_in + (b - s),
                    ..c.clone()
                };
                // 前半も残っている場合、後半は新しい ID にする。
                let id = if s < a { self.alloc_id() } else { c.id };
                out.push(VideoClip { id, ..tail });
            }
        }
        self.video = out;

        // チェーン
        let mut next_id = self.next_id;
        for ch in &mut self.chains {
            let mut out = Vec::new();
            for c in std::mem::take(&mut ch.clips) {
                let (s, e) = (c.start, c.end());
                if e <= a {
                    out.push(c);
                } else if s >= b {
                    out.push(AudioClip {
                        start: s - len,
                        ..c
                    });
                } else {
                    let head = s < a && a - s >= MIN_CLIP;
                    if head {
                        out.push(AudioClip {
                            src_out: c.src_in + (a - s),
                            ..c.clone()
                        });
                    }
                    if e > b && e - b >= MIN_CLIP {
                        // 前半も残っている場合、後半は新しい ID にする。
                        let id = if head {
                            next_id += 1;
                            next_id - 1
                        } else {
                            c.id
                        };
                        out.push(AudioClip {
                            id,
                            start: a,
                            src_in: c.src_in + (b - s),
                            ..c.clone()
                        });
                    }
                }
            }
            out.sort_by(|x, y| x.start.total_cmp(&y.start));
            ch.clips = out;
        }
        self.next_id = next_id;
    }

    /// 書き出しできる状態か確認する。
    pub fn check_exportable(&self) -> Result<()> {
        if self.video.is_empty() {
            bail!("映像トラックが空です");
        }
        for c in &self.video {
            let Some(s) = self.source(c.source) else {
                bail!("映像クリップの素材が見つかりません")
            };
            if !s.path.exists() {
                bail!("素材が見つかりません: {}", s.path.display());
            }
        }
        let mut names = std::collections::HashSet::new();
        for ch in &self.chains {
            if ch.name.trim().is_empty() {
                bail!("名前が空のチェーンがあります");
            }
            if !names.insert(ch.name.as_str()) {
                bail!("チェーン名 {:?} が重複しています", ch.name);
            }
            for c in &ch.clips {
                let Some(s) = self.source(c.source) else {
                    bail!("音声クリップの素材が見つかりません")
                };
                if !s.path.exists() {
                    bail!("素材が見つかりません: {}", s.path.display());
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(p: &mut Project, dur: f64, video: bool) -> Id {
        p.add_source(Source {
            id: 0,
            path: PathBuf::from("/x"),
            relative_path: None,
            duration: dur,
            video: video.then_some(SourceVideo {
                width: 640,
                height: 360,
                fps_num: 10,
                fps_den: 1,
            }),
            has_audio: true,
            audio_offset: 0.0,
            video_ss_offset: 0.0,
        })
    }

    fn spans(p: &Project) -> Vec<(f64, f64)> {
        p.video.iter().map(|c| (c.src_in, c.src_out)).collect()
    }

    #[test]
    fn split_and_remove_video() {
        let mut p = Project::new();
        let a = source(&mut p, 10.0, true);
        let b = source(&mut p, 5.0, true);
        p.insert_video(a, None).unwrap();
        p.insert_video(b, None).unwrap();
        assert_eq!(p.duration(), 15.0);
        let new = p.split_video(4.0).unwrap();
        assert_eq!(spans(&p), vec![(0.0, 4.0), (4.0, 10.0), (0.0, 5.0)]);
        assert!(
            p.split_video(4.0).is_none(),
            "splitting at a boundary does nothing"
        );
        assert!(p.remove_video(new));
        assert_eq!(p.duration(), 9.0);
        assert_eq!(p.video_clip_at(4.5), Some((1, 4.0)));
        let first = p.video[0].id;
        p.move_video(first, 2);
        assert_eq!(spans(&p), vec![(0.0, 5.0), (0.0, 4.0)]);
        let audio_only = source(&mut p, 1.0, false);
        assert!(p.insert_video(audio_only, None).is_err());
    }

    #[test]
    fn delete_range_ripples_all_tracks() {
        let mut p = Project::new();
        let a = source(&mut p, 10.0, true);
        let voice = source(&mut p, 4.0, false);
        p.insert_video(a, None).unwrap();
        let sub = p.add_chain("解説");
        p.add_audio(sub, voice, 1.0).unwrap(); // [1, 5)
        p.add_audio(sub, voice, 6.0).unwrap(); // [6, 10) → 後ろへ詰める
        p.delete_range(3.0, 5.0);
        assert_eq!(spans(&p), vec![(0.0, 3.0), (5.0, 10.0)]);
        assert_ne!(p.video[0].id, p.video[1].id);
        let clips = &p.chain(sub).unwrap().clips;
        let got: Vec<(f64, f64, f64)> = clips
            .iter()
            .map(|c| (c.start, c.src_in, c.src_out))
            .collect();
        assert_eq!(got, vec![(1.0, 0.0, 2.0), (4.0, 0.0, 4.0)]);
        // 範囲がクリップの中にある場合は前後に分かれる
        p.delete_range(5.0, 6.0);
        let clips = &p.chain(sub).unwrap().clips;
        let got: Vec<(f64, f64, f64)> = clips
            .iter()
            .map(|c| (c.start, c.src_in, c.src_out))
            .collect();
        assert_eq!(got, vec![(1.0, 0.0, 2.0), (4.0, 0.0, 1.0), (5.0, 2.0, 4.0)]);
        let ids: std::collections::HashSet<Id> = clips.iter().map(|c| c.id).collect();
        assert_eq!(ids.len(), 3);
        assert_eq!(p.duration(), 7.0);
    }

    #[test]
    fn audio_clip_editing() {
        let mut p = Project::new();
        let v = source(&mut p, 4.0, false);
        let main = p.chains[0].id;
        let c = p.add_audio(main, v, 2.0).unwrap();
        // 左端を素材の先頭より前には伸ばせない
        p.trim_audio(main, c, Some(1.0), None);
        assert_eq!(p.audio_clip(main, c).unwrap().start, 2.0);
        p.trim_audio(main, c, Some(3.0), Some(5.0));
        let clip = p.audio_clip(main, c).unwrap();
        assert_eq!((clip.start, clip.src_in, clip.src_out), (3.0, 1.0, 3.0));
        let d = p.split_audio(main, c, 4.0).unwrap();
        assert_eq!(p.audio_clip(main, d).unwrap().src_in, 2.0);
        p.move_audio(main, d, 0.5);
        assert_eq!(p.chain(main).unwrap().clips[0].id, d);
        assert!(p.remove_audio(main, c));
    }

    #[test]
    fn chains_and_roundtrip() {
        let mut p = Project::new();
        let s = p.add_chain("解説");
        assert_eq!(p.chain(s).unwrap().role, ChainRole::Sub);
        assert_eq!(p.unique_chain_name("解説"), "解説 2");
        p.set_default_chain(s);
        assert_eq!(p.default_chain().unwrap().id, s);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.fdmvproj");
        p.save(&path).unwrap();
        assert_eq!(Project::load(&path).unwrap(), p);
    }

    #[test]
    fn v1_default_quality_is_migrated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.fdmvproj");
        let mut p = Project::new();
        p.version = 1;
        p.settings.crf = 32;
        p.settings.preset = None;
        std::fs::write(&path, serde_json::to_string(&p).unwrap()).unwrap();
        let q = Project::load(&path).unwrap();
        assert_eq!(q.version, PROJECT_VERSION);
        assert_eq!((q.settings.crf, q.settings.preset), (23, Some(6)));
        // 利用者が変えた値はそのまま
        p.settings.crf = 40;
        std::fs::write(&path, serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(Project::load(&path).unwrap().settings.crf, 40);
    }
}
