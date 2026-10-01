//! 映像: ワーカースレッドで先読みデコードし、RGBA に変換してキューに入れる。

use std::collections::VecDeque;
use std::path::Path;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use eframe::egui;
use libfdmv::FdmvReader;
use libfdmv::decode::VideoStream;
use libfdmv::format::Rational;

/// 先読みするフレーム数。
const MAX_QUEUED: usize = 6;

pub struct Frame {
    pub pts: f64,
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

struct Queue {
    frames: VecDeque<Frame>,
    generation: u64,
    error: Option<String>,
}

pub struct VideoEngine {
    queue: Arc<Mutex<Queue>>,
    tx: Option<Sender<(i64, u64)>>,
    worker: Option<JoinHandle<()>>,
    timebase: Rational,
}

impl VideoEngine {
    pub fn start(path: &Path, ctx: egui::Context) -> Result<Self> {
        let reader = FdmvReader::open(path).context("opening file for video")?;
        let dir = reader.directory().clone();
        let timebase = dir
            .video()
            .map(|v| v.timebase)
            .unwrap_or(Rational::new(1, 1));
        let stream = VideoStream::new(&dir, 0)?;
        let queue = Arc::new(Mutex::new(Queue {
            frames: VecDeque::new(),
            generation: 0,
            error: None,
        }));
        let (tx, rx) = channel();
        let q = queue.clone();
        let worker = std::thread::Builder::new()
            .name("fdmv-video".into())
            .spawn(move || {
                if let Err(e) = worker(reader, stream, q.clone(), rx, ctx.clone(), timebase) {
                    q.lock().unwrap().error = Some(format!("{e:#}"));
                    ctx.request_repaint();
                }
            })?;
        Ok(VideoEngine {
            queue,
            tx: Some(tx),
            worker: Some(worker),
            timebase,
        })
    }

    pub fn seek(&self, seconds: f64) {
        let generation = {
            let mut q = self.queue.lock().unwrap();
            q.frames.clear();
            q.generation += 1;
            q.generation
        };
        if let Some(tx) = &self.tx {
            let _ = tx.send((self.timebase.from_seconds(seconds), generation));
        }
    }

    /// 時刻 `t` に表示すべきフレームがあれば取り出す（それより古いフレームは捨てる）。
    pub fn take_frame(&self, t: f64) -> Option<Frame> {
        // 映像の 1 tick の半分だけ余裕を見る（浮動小数点の誤差対策）。
        let eps = self.timebase.as_f64() * 0.5;
        let mut q = self.queue.lock().unwrap();
        let mut out = None;
        while q.frames.front().is_some_and(|f| f.pts <= t + eps) {
            out = q.frames.pop_front();
        }
        out
    }

    /// 次のフレームの時刻（再描画のタイミング用）。
    pub fn next_pts(&self) -> Option<f64> {
        self.queue.lock().unwrap().frames.front().map(|f| f.pts)
    }

    pub fn error(&self) -> Option<String> {
        self.queue.lock().unwrap().error.take()
    }
}

impl Drop for VideoEngine {
    fn drop(&mut self) {
        self.tx = None;
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

fn worker(
    mut reader: FdmvReader<std::io::BufReader<std::fs::File>>,
    mut stream: VideoStream,
    queue: Arc<Mutex<Queue>>,
    rx: Receiver<(i64, u64)>,
    ctx: egui::Context,
    timebase: Rational,
) -> Result<()> {
    let mut generation = 0;
    let mut eof = false;
    let mut rgba = Vec::new();
    loop {
        let full = queue.lock().unwrap().frames.len() >= MAX_QUEUED;
        let wait = if eof || full {
            Duration::from_millis(5)
        } else {
            Duration::ZERO
        };
        match rx.recv_timeout(wait) {
            Ok((pts, g)) => {
                // 連続したシーク要求は最後のものだけ処理する。
                let (mut pts, mut g) = (pts, g);
                while let Ok((p, gg)) = rx.try_recv() {
                    (pts, g) = (p, gg);
                }
                stream.seek(&mut reader, pts)?;
                generation = g;
                eof = false;
                continue;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
        if eof || full {
            continue;
        }
        let frame = stream.next_frame(&mut reader)?.map(|f| {
            f.write_rgba8(&mut rgba);
            Frame {
                pts: timebase.to_seconds(f.pts()),
                width: f.width() as usize,
                height: f.height() as usize,
                rgba: std::mem::take(&mut rgba),
            }
        });
        let mut q = queue.lock().unwrap();
        if q.generation != generation {
            // 処理待ちのシークがある。
            continue;
        }
        match frame {
            Some(f) => q.frames.push_back(f),
            None => eof = true,
        }
        drop(q);
        ctx.request_repaint();
    }
}
