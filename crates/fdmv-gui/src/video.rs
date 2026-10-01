//! 映像出力: 映像源をワーカースレッドで先読みデコードし、RGBA のフレームをキューに入れる。

use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Result;
use eframe::egui;

/// 先読みするフレーム数。
const MAX_QUEUED: usize = 6;

pub struct Frame {
    /// 提示時刻（秒）。
    pub pts: f64,
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

/// 時刻順にフレームを返す映像源。ワーカースレッドで動く。
pub trait FrameSource: Send + 'static {
    /// 次の [`FrameSource::next_frame`] が、`seconds` に表示されるフレームを返すようにする。
    fn seek(&mut self, seconds: f64) -> Result<()>;
    /// 次のフレーム。終わりなら None。
    fn next_frame(&mut self) -> Result<Option<Frame>>;
}

type Job<F> = Box<dyn FnOnce(&mut F) -> Result<()> + Send>;

enum Command<F> {
    Seek(f64, u64),
    With(Job<F>),
}

struct Queue {
    frames: VecDeque<Frame>,
    generation: u64,
    error: Option<String>,
}

pub struct VideoOutput<F: FrameSource> {
    queue: Arc<Mutex<Queue>>,
    tx: Option<Sender<Command<F>>>,
    worker: Option<JoinHandle<()>>,
}

impl<F: FrameSource> VideoOutput<F> {
    /// フレームが用意できるたびに `ctx` に再描画を要求する。
    pub fn start(source: F, ctx: egui::Context) -> Result<Self> {
        let queue = Arc::new(Mutex::new(Queue {
            frames: VecDeque::new(),
            generation: 0,
            error: None,
        }));
        let (tx, rx) = channel();
        let q = queue.clone();
        let worker = std::thread::Builder::new()
            .name("fdmv-video".into())
            .spawn(move || worker(source, q, rx, ctx))?;
        Ok(VideoOutput {
            queue,
            tx: Some(tx),
            worker: Some(worker),
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
            let _ = tx.send(Command::Seek(seconds, generation));
        }
    }

    /// ワーカースレッドで映像源を操作する。反映させるにはこの後に [`Self::seek`] する。
    pub fn with(&self, f: impl FnOnce(&mut F) -> Result<()> + Send + 'static) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Command::With(Box::new(f)));
        }
    }

    /// 時刻 `t` に表示すべきフレームがあれば取り出す（それより古いフレームは捨てる）。
    pub fn take_frame(&self, t: f64) -> Option<Frame> {
        // 浮動小数点の誤差で、ちょうどの時刻のフレームを取りこぼさないようにする。
        const EPS: f64 = 1e-3;
        let mut q = self.queue.lock().unwrap();
        let mut out = None;
        while q.frames.front().is_some_and(|f| f.pts <= t + EPS) {
            out = q.frames.pop_front();
        }
        out
    }

    /// 次のフレームの時刻（再描画のタイミング用）。
    pub fn next_pts(&self) -> Option<f64> {
        self.queue.lock().unwrap().frames.front().map(|f| f.pts)
    }

    /// 映像源で起きたエラー（取り出すと消える）。
    pub fn take_error(&self) -> Option<String> {
        self.queue.lock().unwrap().error.take()
    }
}

impl<F: FrameSource> Drop for VideoOutput<F> {
    fn drop(&mut self) {
        self.tx = None;
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

fn worker<F: FrameSource>(
    mut source: F,
    queue: Arc<Mutex<Queue>>,
    rx: Receiver<Command<F>>,
    ctx: egui::Context,
) {
    let report = |e: anyhow::Error| {
        eprintln!("video: {e:#}");
        queue.lock().unwrap().error = Some(format!("{e:#}"));
        ctx.request_repaint();
    };
    let mut generation = 0;
    let mut eof = false;
    loop {
        let full = queue.lock().unwrap().frames.len() >= MAX_QUEUED;
        let wait = if eof || full {
            Duration::from_millis(5)
        } else {
            Duration::ZERO
        };
        match rx.recv_timeout(wait) {
            Ok(Command::Seek(t, g)) => {
                // 連続したシーク要求は最後のものだけ処理する（間の With は順に実行する）。
                let (mut t, mut g) = (t, g);
                while let Ok(cmd) = rx.try_recv() {
                    match cmd {
                        Command::Seek(tt, gg) => (t, g) = (tt, gg),
                        Command::With(f) => {
                            if let Err(e) = f(&mut source) {
                                report(e);
                            }
                        }
                    }
                }
                eof = false;
                generation = g;
                if let Err(e) = source.seek(t) {
                    report(e);
                    eof = true;
                }
                continue;
            }
            Ok(Command::With(f)) => {
                if let Err(e) = f(&mut source) {
                    report(e);
                }
                continue;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if eof || full {
            continue;
        }
        let frame = match source.next_frame() {
            Ok(f) => f,
            Err(e) => {
                report(e);
                None
            }
        };
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
