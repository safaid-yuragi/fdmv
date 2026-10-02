//! 映像のエンコード: [`FrameSlot`] のフレームを時刻付きで ffmpeg に送る。
//!
//! - 録画開始から n / fps 秒ごとに画面を見て、変化していればそのフレームを「n / fps 秒の画面」として送る
//!   （可変フレームレート。変化が無い間は送らないので、静止画面ではほとんど負荷がかからない）。
//! - フレームは Matroska に包んで時刻と一緒に渡す（[`crate::mkvpipe`]）。エンコードが追いつかないときは
//!   送るのを諦めて間引く。時刻は保たれるので、映像と音声がずれたり、映像が早送りになったりしない。
//! - ffmpeg の優先度は下げる（ゲームなど録画対象のアプリの動きを妨げないように）。

use std::ffi::OsString;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use libfdmv::ffmpeg::{Ffmpeg, Quality};

use crate::codecs::LiveEncoder;
use crate::mkvpipe::MkvPipe;
use crate::video::{Frame, FrameSlot, fit};

/// 送る前に溜めておけるフレームのバイト数の上限（これを超えたら間引く）。
const QUEUE_BYTES: usize = 256 << 20;

#[derive(Default)]
pub struct EncoderStats {
    sent: AtomicU64,
    dropped: AtomicU64,
    error: Mutex<Option<String>>,
}

impl EncoderStats {
    /// ffmpeg に送ったフレーム数。
    pub fn frames(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    /// エンコードが追いつかずに間引いたフレーム数。
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }
}

/// ffmpeg に渡す 1 枚（`frame` が None なら黒）。
struct Item {
    tick: u64,
    frame: Option<Arc<Frame>>,
}

pub struct VideoEncoder {
    child: Child,
    sampler: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
    /// この番号のフレームの手前で止める（u64::MAX = 録画中）。
    stop_at: Arc<AtomicU64>,
    abort: Arc<AtomicBool>,
    stats: Arc<EncoderStats>,
    stderr: Arc<Mutex<Vec<String>>>,
    stderr_thread: Option<JoinHandle<()>>,
    pub encoder: LiveEncoder,
    pub fps: u32,
}

impl VideoEncoder {
    /// `size` = 送るフレームの大きさ（偶数）。`max_height` を超える場合は ffmpeg で縮小する。
    /// `intermediate` なら、停止後に変換する前提の高画質で録る。
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        ff: &Ffmpeg,
        slot: Arc<FrameSlot>,
        size: (u32, u32),
        fps: u32,
        max_height: Option<u32>,
        encoder: &LiveEncoder,
        quality: Quality,
        intermediate: bool,
        output: &Path,
        epoch: Instant,
    ) -> Result<Self> {
        let (w, h) = size;
        if w < 2 || h < 2 || w % 2 != 0 || h % 2 != 0 {
            bail!("映像の大きさ {w}x{h} はエンコードできません");
        }
        let fps = fps.clamp(1, 240);
        let mut args: Vec<OsString> = Vec::new();
        for a in ["-hide_banner", "-loglevel", "error", "-nostats", "-y"] {
            args.push(a.into());
        }
        args.extend(encoder.input_args());
        for a in ["-f", "matroska", "-i", "pipe:0"] {
            args.push(a.into());
        }
        // 色は BT.709 の限定範囲に変換して、そう記録する（プレイヤーが正しい行列で戻せるように）。
        let mut vf = String::new();
        if let Some(mh) = max_height.filter(|&mh| h > mh) {
            vf.push_str(&format!("scale=-2:{mh}:flags=bicubic:"));
        } else {
            vf.push_str("scale=");
        }
        vf.push_str("out_color_matrix=bt709:out_range=tv,format=yuv420p");
        vf.push_str(encoder.filter_suffix());
        for a in [
            "-vf",
            &vf,
            // 時刻はそのまま使い、1/fps 秒の目盛りに載せる。
            "-fps_mode",
            "vfr",
            "-enc_time_base",
            &format!("1:{fps}"),
            "-g",
            &(fps * 5).to_string(),
            "-colorspace",
            "bt709",
            "-color_primaries",
            "bt709",
            "-color_trc",
            "bt709",
            "-color_range",
            "tv",
        ] {
            args.push(a.into());
        }
        args.extend(encoder.output_args(ff, quality, intermediate)?);
        args.push("-f".into());
        args.push(encoder.container().into());
        args.push(output.as_os_str().to_owned());

        let mut cmd = Command::new(&ff.ffmpeg);
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        lower_priority(&mut cmd);
        let mut child = cmd.spawn().context("ffmpeg を起動できません")?;
        let stdin = child.stdin.take().unwrap();
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let stderr_thread = {
            let pipe = child.stderr.take().unwrap();
            let lines = stderr.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(pipe).lines().map_while(|l| l.ok()) {
                    let mut l = lines.lock().unwrap();
                    l.push(line);
                    if l.len() > 20 {
                        l.remove(0);
                    }
                }
            })
        };

        let stats = Arc::new(EncoderStats::default());
        let stop_at = Arc::new(AtomicU64::new(u64::MAX));
        let abort = Arc::new(AtomicBool::new(false));
        let frame_bytes = w as usize * h as usize * 4;
        let depth = (QUEUE_BYTES / frame_bytes).clamp(2, (fps as usize / 2).max(2));
        let (tx, rx) = sync_channel::<Item>(depth);
        let sampler = {
            let (stats, stop_at, abort) = (stats.clone(), stop_at.clone(), abort.clone());
            std::thread::Builder::new()
                .name("fdmv-video-sample".into())
                .spawn(move || sample(&slot, fps, epoch, tx, &stats, &stop_at, &abort))?
        };
        let writer = {
            let (stats, abort) = (stats.clone(), abort.clone());
            std::thread::Builder::new()
                .name("fdmv-video-encode".into())
                .spawn(move || {
                    if let Err(e) = write(stdin, rx, (w, h), fps, &stats)
                        && !abort.load(Ordering::Relaxed)
                    {
                        *stats.error.lock().unwrap() = Some(format!("{e:#}"));
                    }
                })?
        };
        Ok(VideoEncoder {
            child,
            sampler: Some(sampler),
            writer: Some(writer),
            stop_at,
            abort,
            stats,
            stderr,
            stderr_thread: Some(stderr_thread),
            encoder: encoder.clone(),
            fps,
        })
    }

    pub fn stats(&self) -> &Arc<EncoderStats> {
        &self.stats
    }

    /// 録画開始から `duration` で終わるよう指示する（待たない）。
    pub fn stop_at(&self, duration: Duration) {
        self.stop_at
            .fetch_min(ticks(duration, self.fps), Ordering::Relaxed);
    }

    /// 終わるのを待つ。映像の長さ（秒）を返す。
    pub fn finish(mut self, duration: Duration) -> Result<f64> {
        self.stop_at(duration);
        let end = self.stop_at.load(Ordering::Relaxed);
        if let Some(t) = self.sampler.take() {
            let _ = t.join();
        }
        if let Some(t) = self.writer.take() {
            let _ = t.join();
        }
        let status = self.child.wait()?;
        if let Some(t) = self.stderr_thread.take() {
            let _ = t.join();
        }
        if !status.success() {
            return Err(anyhow!(
                "映像のエンコードに失敗しました（ffmpeg: {status}）: {}",
                self.stderr.lock().unwrap().join("\n")
            ));
        }
        if let Some(e) = self.stats.error() {
            bail!("映像のエンコードに失敗しました: {e}");
        }
        Ok(end as f64 / self.fps as f64)
    }
}

impl Drop for VideoEncoder {
    fn drop(&mut self) {
        if self.sampler.is_some() || self.writer.is_some() {
            self.abort.store(true, Ordering::Relaxed);
            let _ = self.child.kill();
            for t in [self.sampler.take(), self.writer.take()]
                .into_iter()
                .flatten()
            {
                let _ = t.join();
            }
            let _ = self.child.wait();
        }
    }
}

fn ticks(d: Duration, fps: u32) -> u64 {
    ((d.as_secs_f64() * fps as f64).ceil() as u64).max(1)
}

/// 決まった時刻ごとに画面を見て、変化していれば送る。
fn sample(
    slot: &FrameSlot,
    fps: u32,
    epoch: Instant,
    tx: SyncSender<Item>,
    stats: &EncoderStats,
    stop_at: &AtomicU64,
    abort: &AtomicBool,
) {
    let mut last_generation = None;
    let mut last_sent: Option<u64> = None;
    let mut n = 0u64;
    let send_blocking = |item: Item, last_sent: &mut Option<u64>| {
        let tick = item.tick;
        if tx.send(item).is_ok() {
            *last_sent = Some(tick);
        }
    };
    loop {
        if abort.load(Ordering::Relaxed) {
            return;
        }
        if n >= stop_at.load(Ordering::Relaxed) {
            break;
        }
        let due = epoch + Duration::from_secs_f64(n as f64 / fps as f64);
        let now = Instant::now();
        if due > now {
            // 停止の指示にすぐ気づけるよう、長く寝すぎない。
            std::thread::sleep((due - now).min(Duration::from_millis(50)));
            continue;
        }
        let (generation, frame) = slot.latest();
        if n == 0 {
            // 先頭のフレームは必ず 0 秒に置く（無ければ黒）。
            send_blocking(Item { tick: 0, frame }, &mut last_sent);
            last_generation = Some(generation);
        } else if last_generation != Some(generation) {
            match tx.try_send(Item { tick: n, frame }) {
                Ok(()) => {
                    last_sent = Some(n);
                    last_generation = Some(generation);
                }
                Err(TrySendError::Full(_)) => {
                    stats.dropped.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
        n += 1;
    }
    // 終わりの 2 フレームを置いて、映像の長さを録画の長さに合わせる
    // （最後のフレームの長さは直前の間隔から決まるため）。
    for tick in [n.saturating_sub(2), n.saturating_sub(1)] {
        if last_sent.is_none_or(|l| tick > l) {
            let (_, frame) = slot.latest();
            send_blocking(Item { tick, frame }, &mut last_sent);
        }
    }
}

/// 溜まったフレームを ffmpeg に書く。
fn write(
    stdin: ChildStdin,
    rx: Receiver<Item>,
    (w, h): (u32, u32),
    fps: u32,
    stats: &EncoderStats,
) -> Result<()> {
    let mut pipe = MkvPipe::new(stdin, w, h, fps).context("ffmpeg に映像を送れません")?;
    let mut black = vec![0u8; w as usize * h as usize * 4];
    for px in black.chunks_exact_mut(4) {
        px[3] = 255;
    }
    let mut buf = Vec::new();
    for item in rx {
        let ms = (item.tick as f64 * 1000.0 / fps as f64).round() as u64;
        let data: &[u8] = match &item.frame {
            None => &black,
            Some(f) if f.width == w && f.height == h => &f.bgra,
            Some(f) => {
                fit(f, w, h, &mut buf);
                &buf
            }
        };
        pipe.write_frame(ms, data)
            .context("ffmpeg に映像を送れません")?;
        stats.sent.fetch_add(1, Ordering::Relaxed);
    }
    Ok(())
}

/// 子プロセスの優先度を下げる。
fn lower_priority(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: fork 後・exec 前に async-signal-safe な setpriority だけを呼ぶ。
        unsafe {
            cmd.pre_exec(|| {
                libc::setpriority(libc::PRIO_PROCESS, 0, 10);
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(BELOW_NORMAL_PRIORITY_CLASS | CREATE_NO_WINDOW);
    }
}
