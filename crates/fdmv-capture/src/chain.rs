//! チェーン 1 本分の録音。
//!
//! 音声の取り込み（[`Take`]）は届いたサンプルを生の f32 のまま「パート」ファイルに書く。
//! パートの開始位置は、サンプルが届いた時刻から逆算した録画開始からの秒数。
//! 音声が途切れたら（アプリが再生を止めた、ストリームが作り直された など）新しいパートにする。
//! 同じアプリが複数のストリームを出すと、パートは時間的に重なる。停止後にミックスする。

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use libfdmv::ChainRole;

/// 前のパートの終わりより、これ以上遅れてサンプルが届いたら新しいパートにする（秒）。
const GAP_SECONDS: f64 = 0.3;
/// 前のパートの終わりより、これ以上進んだサンプルは捨てる（時計のずれ・一時的な詰まりの吸収）。
const AHEAD_SECONDS: f64 = 0.5;

/// 書き終えたパート。
#[derive(Clone, Debug)]
pub struct Part {
    pub path: PathBuf,
    /// 録画開始からの位置（秒）。負なら録画開始より前から始まっている。
    pub start: f64,
    pub rate: u32,
    pub channels: u16,
    pub frames: u64,
}

impl Part {
    pub fn duration(&self) -> f64 {
        self.frames as f64 / self.rate as f64
    }
}

pub struct Chain {
    pub name: String,
    pub role: ChainRole,
    /// FDMV のチェーンのメタデータにそのまま入れる。
    pub meta: Vec<(String, String)>,
    dir: PathBuf,
    epoch: Instant,
    parts: Mutex<Vec<Part>>,
    next_part: AtomicU32,
    /// 前回読み出してからの最大振幅（f32 のビット列。正の数なので整数として比較できる）。
    peak: AtomicU32,
    /// いま音声を受け取っている取り込みの数。
    open_takes: AtomicUsize,
    /// 取り込みが続けられなくなったときのエラー。
    error: Mutex<Option<String>>,
    stopped: AtomicBool,
}

impl Chain {
    pub fn new(
        name: impl Into<String>,
        role: ChainRole,
        meta: Vec<(String, String)>,
        dir: PathBuf,
        epoch: Instant,
    ) -> io::Result<Arc<Self>> {
        std::fs::create_dir_all(&dir)?;
        Ok(Arc::new(Chain {
            name: name.into(),
            role,
            meta,
            dir,
            epoch,
            parts: Mutex::default(),
            next_part: AtomicU32::new(0),
            peak: AtomicU32::new(0),
            open_takes: AtomicUsize::new(0),
            error: Mutex::default(),
            stopped: AtomicBool::new(false),
        }))
    }

    /// 録画開始の時刻（映像の 0 秒）。
    pub fn epoch(&self) -> Instant {
        self.epoch
    }

    /// 新しい取り込みを始める。`rate` / `channels` は届くサンプルの形式。
    pub fn take(self: &Arc<Self>, rate: u32, channels: u16) -> Take {
        Take {
            chain: self.clone(),
            rate,
            channels: channels.max(1),
            current: None,
            buf: Vec::new(),
        }
    }

    pub fn parts(&self) -> Vec<Part> {
        self.parts.lock().unwrap().clone()
    }

    /// 前回呼んでからの最大振幅（0.0–）。
    pub fn take_peak(&self) -> f32 {
        f32::from_bits(self.peak.swap(0, Ordering::Relaxed))
    }

    /// いま音声が届いているか。
    pub fn is_receiving(&self) -> bool {
        self.open_takes.load(Ordering::Relaxed) > 0
    }

    pub fn set_error(&self, e: impl Into<String>) {
        *self.error.lock().unwrap() = Some(e.into());
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }

    /// 以後に届いた音声を捨てる（停止処理の途中で届いたもの）。
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
    }

    fn record_peak(&self, data: &[f32]) {
        let p = data.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        if p.is_finite() {
            self.peak.fetch_max(p.to_bits(), Ordering::Relaxed);
        }
    }
}

struct OpenPart {
    w: BufWriter<File>,
    path: PathBuf,
    start: f64,
    frames: u64,
}

/// 1 つの音声ストリームの取り込み。drop すると書きかけのパートを閉じる。
pub struct Take {
    chain: Arc<Chain>,
    rate: u32,
    channels: u16,
    current: Option<OpenPart>,
    buf: Vec<u8>,
}

impl Take {
    /// インターリーブの f32 サンプルを書く。いま取り込んだばかり（遅延なし）とみなす。
    pub fn push(&mut self, data: &[f32]) {
        self.push_at(data, Instant::now());
    }

    /// `end` = `data` の最後のサンプルが鳴った（取り込まれた）時刻。
    pub fn push_at(&mut self, data: &[f32], end: Instant) {
        if data.is_empty() || self.chain.stopped.load(Ordering::Relaxed) {
            return;
        }
        if let Err(e) = self.write(data, end) {
            self.chain
                .set_error(format!("音声の書き込みに失敗しました: {e}"));
            self.close();
        }
    }

    fn write(&mut self, data: &[f32], end: Instant) -> io::Result<()> {
        let ch = self.channels as usize;
        let frames = (data.len() / ch) as u64;
        if frames == 0 {
            return Ok(());
        }
        let data = &data[..frames as usize * ch];
        let end_secs = signed_secs(end, self.chain.epoch);
        let start = end_secs - frames as f64 / self.rate as f64;
        if let Some(cur) = &self.current {
            let expected = cur.start + cur.frames as f64 / self.rate as f64;
            if start - expected > GAP_SECONDS {
                self.close();
            } else if expected - start > AHEAD_SECONDS {
                return Ok(());
            }
        }
        if self.current.is_none() {
            let n = self.chain.next_part.fetch_add(1, Ordering::Relaxed);
            let path = self.chain.dir.join(format!("part{n:05}.f32"));
            let w = BufWriter::with_capacity(1 << 18, File::create(&path)?);
            self.current = Some(OpenPart {
                w,
                path,
                start,
                frames: 0,
            });
            self.chain.open_takes.fetch_add(1, Ordering::Relaxed);
        }
        let cur = self.current.as_mut().unwrap();
        self.buf.clear();
        self.buf.reserve(data.len() * 4);
        for s in data {
            self.buf.extend_from_slice(&s.to_le_bytes());
        }
        cur.w.write_all(&self.buf)?;
        cur.frames += frames;
        self.chain.record_peak(data);
        Ok(())
    }

    /// 書きかけのパートを閉じる。次に届いたサンプルからは新しいパートになる。
    pub fn close(&mut self) {
        let Some(mut cur) = self.current.take() else {
            return;
        };
        self.chain.open_takes.fetch_sub(1, Ordering::Relaxed);
        if let Err(e) = cur.w.flush() {
            self.chain
                .set_error(format!("音声の書き込みに失敗しました: {e}"));
        }
        if cur.frames > 0 {
            self.chain.parts.lock().unwrap().push(Part {
                path: cur.path,
                start: cur.start,
                rate: self.rate,
                channels: self.channels,
                frames: cur.frames,
            });
        }
    }
}

impl Drop for Take {
    fn drop(&mut self) {
        self.close();
    }
}

/// `t - epoch` を符号付きの秒で。
pub fn signed_secs(t: Instant, epoch: Instant) -> f64 {
    match t.checked_duration_since(epoch) {
        Some(d) => d.as_secs_f64(),
        None => -(epoch - t).as_secs_f64(),
    }
}

/// `now` から `d` 前の時刻（引けなければ `now`）。
pub fn before(now: Instant, d: Duration) -> Instant {
    now.checked_sub(d).unwrap_or(now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_gap_and_drops_ahead() {
        let dir = tempfile::tempdir().unwrap();
        let epoch = Instant::now();
        let chain = Chain::new("t", ChainRole::Sub, Vec::new(), dir.path().into(), epoch).unwrap();
        let mut take = chain.take(48000, 2);
        let block = vec![0.5f32; 960]; // 480 フレーム = 10 ms
        let at = |ms: u64| epoch + Duration::from_millis(ms);
        take.push_at(&block, at(110)); // 100–110 ms
        take.push_at(&block, at(120));
        assert!(chain.is_receiving());
        // 1 秒先の音（途切れた後の再開）
        take.push_at(&block, at(1130));
        // 同じ時刻に届き続ける（時計より速い）→ 0.5 秒先まで受け取り、その先は捨てる
        for _ in 0..80 {
            take.push_at(&block, at(1130));
        }
        drop(take);
        assert!(!chain.is_receiving());
        let parts = chain.parts();
        assert_eq!(parts.len(), 2);
        assert!((parts[0].start - 0.1).abs() < 1e-9);
        assert_eq!(parts[0].frames, 960); // 2 回分
        assert!((parts[1].start - 1.12).abs() < 1e-9);
        assert_eq!(parts[1].frames, 480 * 51);
        assert!((chain.take_peak() - 0.5).abs() < 1e-6);
        assert_eq!(chain.take_peak(), 0.0);
    }
}
