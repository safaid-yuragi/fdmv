//! テストパターン（動作確認用）。環境変数 `FDMV_TEST_PATTERN` があると録画対象の一覧に出る。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::{Frame, FrameSlot, VideoCapture};

struct Pattern {
    slot: Arc<FrameSlot>,
    stop: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl VideoCapture for Pattern {
    fn slot(&self) -> Arc<FrameSlot> {
        self.slot.clone()
    }

    fn error(&self) -> Option<String> {
        None
    }
}

impl Drop for Pattern {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.lock().unwrap().take() {
            let _ = t.join();
        }
    }
}

/// 横に流れる市松模様と、時計の秒が変わってから 50 ms のあいだ白くなる帯（音ズレの確認用）。
pub fn start(width: u32, height: u32, fps: u32) -> Box<dyn VideoCapture> {
    let slot = Arc::new(FrameSlot::default());
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let (slot, stop) = (slot.clone(), stop.clone());
        std::thread::spawn(move || {
            let start = Instant::now();
            let period = Duration::from_secs_f64(1.0 / fps.max(1) as f64);
            let mut n = 0u32;
            while !stop.load(Ordering::Relaxed) {
                let t = start.elapsed().as_secs_f64();
                slot.put(frame(width, height, t));
                n += 1;
                let next = start + period * n;
                if let Some(d) = next.checked_duration_since(Instant::now()) {
                    std::thread::sleep(d);
                }
            }
        })
    };
    Box::new(Pattern {
        slot,
        stop,
        thread: Mutex::new(Some(thread)),
    })
}

fn frame(w: u32, h: u32, t: f64) -> Frame {
    let shift = (t * 200.0) as u32;
    let flash = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .is_ok_and(|d| d.subsec_millis() < 50);
    let mut bgra = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            let px = if y < h / 8 {
                if flash {
                    [255, 255, 255, 255]
                } else {
                    [0, 0, 0, 255]
                }
            } else {
                let v = if ((x + shift) / 64 + y / 64) % 2 == 0 {
                    230
                } else {
                    30
                };
                [v, (y * 255 / h) as u8, 255 - v, 255]
            };
            bgra[i..i + 4].copy_from_slice(&px);
        }
    }
    Frame {
        width: w,
        height: h,
        bgra,
    }
}
