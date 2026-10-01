//! 音声出力: 音源（ワーカースレッド）→ リングバッファ → 出力デバイス。
//!
//! 出力デバイスが消費したフレーム数が再生位置（マスタークロック）になる。
//! デバイスが無い環境（または `FDMV_NO_AUDIO` 指定時）は、実時間で消費するだけのダミー出力を使う。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use libfdmv::format::OPUS_SAMPLE_RATE;

/// 音源のチャンネル数（ステレオ固定）。
pub const CHANNELS: usize = 2;
/// 音源のサンプルレート。
pub const SAMPLE_RATE: u32 = OPUS_SAMPLE_RATE;
/// 先読みしておく長さ（秒）。
const BUFFER_SECONDS: f64 = 0.25;

/// 48 kHz ステレオの PCM を生成する音源。ワーカースレッドで動く。
pub trait PcmSource: Send + 'static {
    /// 位置（48 kHz サンプル）を変更する。
    fn seek(&mut self, sample: i64) -> Result<()>;
    /// `out`（ステレオ・インターリーブ）を埋め、書いたフレーム数を返す。終わりなら 0。
    fn render(&mut self, out: &mut [f32]) -> Result<usize>;
}

type Job<S> = Box<dyn FnOnce(&mut S) -> Result<()> + Send>;

enum Command<S> {
    /// (位置, 世代)
    Seek(i64, u64),
    /// ワーカースレッドで音源を操作する。
    With(Job<S>),
}

/// ワーカーと出力コールバックが共有する状態。
struct Queue {
    /// 出力レートに変換済みのサンプル（ステレオ・インターリーブ）。
    samples: VecDeque<f32>,
    /// `samples` の先頭が鳴るはずだった時点の、タイムライン上の位置（48 kHz サンプル）。
    base: i64,
    /// `base` 以降に出力したフレーム数（出力レート）。
    consumed: u64,
    /// ミックス側がタイムラインの終わりまで書き終えた。
    eof: bool,
    /// シークのたびに増える。ワーカーは自分の世代と一致するときだけ書き込む。
    generation: u64,
}

struct Shared {
    queue: Mutex<Queue>,
    playing: AtomicBool,
    /// マスター音量（f32 のビット列）。
    volume: AtomicU32,
    rate: u32,
    error: Mutex<Option<String>>,
}

impl Shared {
    fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed))
    }

    /// 出力コールバック本体: `out` を `channels` チャンネルで埋める。
    fn fill<T: SizedSample + FromSample<f32>>(&self, out: &mut [T], channels: usize) {
        let frames = out.len() / channels;
        let volume = self.volume();
        let mut written = 0;
        if self.playing.load(Ordering::Relaxed)
            && let Ok(mut q) = self.queue.try_lock()
        {
            let n = frames.min(q.samples.len() / CHANNELS);
            for f in 0..n {
                let l = q.samples.pop_front().unwrap() * volume;
                let r = q.samples.pop_front().unwrap() * volume;
                let frame = &mut out[f * channels..(f + 1) * channels];
                if channels == 1 {
                    frame[0] = T::from_sample(((l + r) * 0.5).clamp(-1.0, 1.0));
                } else {
                    frame[0] = T::from_sample(l.clamp(-1.0, 1.0));
                    frame[1] = T::from_sample(r.clamp(-1.0, 1.0));
                    for s in &mut frame[2..] {
                        *s = T::from_sample(0.0f32);
                    }
                }
            }
            q.consumed += n as u64;
            written = n;
        }
        for s in &mut out[written * channels..] {
            *s = T::from_sample(0.0f32);
        }
    }
}

/// 出力レートへの線形補間リサンプラ（ステレオ）。
struct Resampler {
    /// 入力フレーム / 出力フレーム。
    step: f64,
    /// 次の出力サンプルの位置（入力フレーム単位。-1 は前回の最後のフレーム）。
    t: f64,
    prev: [f32; 2],
    primed: bool,
}

impl Resampler {
    fn new(out_rate: u32) -> Self {
        Resampler {
            step: OPUS_SAMPLE_RATE as f64 / out_rate as f64,
            t: 0.0,
            prev: [0.0; 2],
            primed: false,
        }
    }

    fn reset(&mut self) {
        self.t = 0.0;
        self.primed = false;
    }

    fn process(&mut self, input: &[f32], out: &mut VecDeque<f32>) {
        if self.step == 1.0 {
            out.extend(input);
            return;
        }
        let n = input.len() / 2;
        if n == 0 {
            return;
        }
        if !self.primed {
            self.prev = [input[0], input[1]];
            self.primed = true;
        }
        let frame = |i: isize| -> [f32; 2] {
            if i < 0 {
                self.prev
            } else {
                [input[i as usize * 2], input[i as usize * 2 + 1]]
            }
        };
        while self.t <= (n - 1) as f64 {
            let i = self.t.floor();
            let frac = (self.t - i) as f32;
            let (a, b) = (frame(i as isize), frame(i as isize + 1));
            out.push_back(a[0] + (b[0] - a[0]) * frac);
            out.push_back(a[1] + (b[1] - a[1]) * frac);
            self.t += self.step;
        }
        self.t -= n as f64;
        self.prev = frame(n as isize - 1);
    }
}

/// 出力先。ドロップすると止まる。
enum Output {
    Device {
        _stream: cpal::Stream,
        name: String,
    },
    Null {
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    },
}

impl Drop for Output {
    fn drop(&mut self) {
        if let Output::Null { stop, thread } = self {
            stop.store(true, Ordering::Relaxed);
            if let Some(t) = thread.take() {
                let _ = t.join();
            }
        }
    }
}

pub struct AudioOutput<S: PcmSource> {
    shared: Arc<Shared>,
    tx: Option<Sender<Command<S>>>,
    worker: Option<JoinHandle<()>>,
    _output: Output,
    device_name: Option<String>,
}

impl<S: PcmSource> AudioOutput<S> {
    /// 出力を開き、`source` を位置 0 から鳴らす準備をする（最初は一時停止）。
    pub fn start(source: S) -> Result<Self> {
        let (config, device) = match open_device() {
            Ok((config, device)) => (Some(config), Some(device)),
            Err(e) => {
                eprintln!("audio: {e:#}; playing without sound");
                (None, None)
            }
        };
        let rate = config
            .as_ref()
            .map(|c| c.sample_rate())
            .unwrap_or(SAMPLE_RATE);
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue {
                samples: VecDeque::new(),
                base: 0,
                consumed: 0,
                eof: false,
                generation: 0,
            }),
            playing: AtomicBool::new(false),
            volume: AtomicU32::new(1.0f32.to_bits()),
            rate,
            error: Mutex::new(None),
        });

        let output = match (config, device) {
            (Some(config), Some(device)) => match build_stream(&device, &config, shared.clone()) {
                Ok(stream) => {
                    let name = device
                        .description()
                        .map(|d| d.to_string())
                        .unwrap_or_else(|_| "default".into());
                    Output::Device {
                        _stream: stream,
                        name,
                    }
                }
                Err(e) => {
                    eprintln!("audio: {e:#}; playing without sound");
                    null_output(shared.clone())
                }
            },
            _ => null_output(shared.clone()),
        };
        let device_name = match &output {
            Output::Device { name, .. } => Some(format!("{name} ({} Hz)", shared.rate)),
            Output::Null { .. } => None,
        };

        let (tx, rx) = channel();
        let worker_shared = shared.clone();
        let worker = std::thread::Builder::new()
            .name("fdmv-audio".into())
            .spawn(move || worker(source, worker_shared, rx))?;
        Ok(AudioOutput {
            shared,
            tx: Some(tx),
            worker: Some(worker),
            _output: output,
            device_name,
        })
    }

    /// 出力デバイス名。ダミー出力なら None。
    pub fn device_name(&self) -> Option<&str> {
        self.device_name.as_deref()
    }

    /// ワーカースレッドで音源を操作する（チェーンの追加や内容の差し替えなど）。
    /// 既に用意済みのサンプルには反映されないので、すぐに反映したい場合はこの後に [`Self::seek`] する。
    pub fn with(&self, f: impl FnOnce(&mut S) -> Result<()> + Send + 'static) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Command::With(Box::new(f)));
        }
    }

    /// 再生位置をすぐに反映するため、キューもここで空にする（ワーカーは後から追いつく）。
    pub fn seek(&self, sample: i64) {
        let generation = {
            let mut q = self.shared.queue.lock().unwrap();
            q.samples.clear();
            q.base = sample;
            q.consumed = 0;
            q.eof = false;
            q.generation += 1;
            q.generation
        };
        if let Some(tx) = &self.tx {
            let _ = tx.send(Command::Seek(sample, generation));
        }
    }

    pub fn set_playing(&self, playing: bool) {
        self.shared.playing.store(playing, Ordering::Relaxed);
    }

    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }

    pub fn set_volume(&self, v: f32) {
        self.shared.volume.store(v.to_bits(), Ordering::Relaxed);
    }

    /// 再生位置（48 kHz サンプル）。
    pub fn position(&self) -> i64 {
        let q = self.shared.queue.lock().unwrap();
        q.base + (q.consumed as f64 * SAMPLE_RATE as f64 / self.shared.rate as f64) as i64
    }

    /// 最後まで再生し終えた。
    pub fn finished(&self) -> bool {
        let q = self.shared.queue.lock().unwrap();
        q.eof && q.samples.is_empty()
    }

    /// 音源で起きたエラー（取り出すと消える）。
    pub fn take_error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().take()
    }
}

impl<S: PcmSource> Drop for AudioOutput<S> {
    fn drop(&mut self) {
        // チャンネルを閉じるとワーカーが終了する。
        self.tx = None;
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

fn worker<S: PcmSource>(mut source: S, shared: Arc<Shared>, rx: Receiver<Command<S>>) {
    let report = |e: anyhow::Error| {
        eprintln!("audio: {e:#}");
        *shared.error.lock().unwrap() = Some(format!("{e:#}"));
    };
    let mut resampler = Resampler::new(shared.rate);
    let target = (shared.rate as f64 * BUFFER_SECONDS) as usize * CHANNELS;
    let mut mix = vec![0f32; 1024 * CHANNELS];
    let mut converted = VecDeque::new();
    let mut eof = false;
    let mut generation = 0u64;
    loop {
        let queued = shared.queue.lock().unwrap().samples.len();
        let wait = if eof || queued >= target {
            Duration::from_millis(10)
        } else {
            Duration::ZERO
        };
        let cmd = match rx.recv_timeout(wait) {
            Ok(c) => Some(c),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if let Some(cmd) = cmd {
            match cmd {
                Command::Seek(sample, g) => {
                    if let Err(e) = source.seek(sample) {
                        report(e);
                    }
                    resampler.reset();
                    converted.clear();
                    eof = false;
                    generation = g;
                }
                Command::With(f) => {
                    if let Err(e) = f(&mut source) {
                        report(e);
                    }
                }
            }
            continue;
        }
        if eof || queued >= target {
            continue;
        }
        let n = match source.render(&mut mix) {
            Ok(n) => n,
            Err(e) => {
                report(e);
                0
            }
        };
        let mut q = shared.queue.lock().unwrap();
        if q.generation != generation {
            // まだ処理していないシークがある。古い位置のサンプルは捨てる。
            continue;
        }
        if n == 0 {
            eof = true;
            q.eof = true;
            continue;
        }
        resampler.process(&mix[..n * CHANNELS], &mut converted);
        q.samples.extend(converted.drain(..));
    }
}

fn open_device() -> Result<(cpal::SupportedStreamConfig, cpal::Device)> {
    if std::env::var_os("FDMV_NO_AUDIO").is_some() {
        bail!("disabled by FDMV_NO_AUDIO");
    }
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow!("no default output device"))?;
    let default = device.default_output_config()?;
    // 48 kHz が使えるなら変換せずに済む。
    let preferred = device.supported_output_configs()?.find_map(|c| {
        (c.sample_format() == default.sample_format()
            && c.channels() == default.channels()
            && c.min_sample_rate() <= OPUS_SAMPLE_RATE
            && c.max_sample_rate() >= OPUS_SAMPLE_RATE)
            .then(|| c.with_sample_rate(OPUS_SAMPLE_RATE))
    });
    Ok((preferred.unwrap_or(default), device))
}

fn build_stream(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    shared: Arc<Shared>,
) -> Result<cpal::Stream> {
    let stream = match config.sample_format() {
        SampleFormat::F32 => build::<f32>(device, config, shared)?,
        SampleFormat::I16 => build::<i16>(device, config, shared)?,
        SampleFormat::U16 => build::<u16>(device, config, shared)?,
        SampleFormat::I32 => build::<i32>(device, config, shared)?,
        SampleFormat::F64 => build::<f64>(device, config, shared)?,
        f => bail!("unsupported sample format {f:?}"),
    };
    stream.play()?;
    Ok(stream)
}

fn build<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    shared: Arc<Shared>,
) -> Result<cpal::Stream> {
    let channels = config.channels() as usize;
    Ok(device.build_output_stream(
        config.config(),
        move |out: &mut [T], _| shared.fill(out, channels),
        |e| eprintln!("audio stream error: {e}"),
        None,
    )?)
}

/// デバイスが無いときに、実時間でキューを消費する。
fn null_output(shared: Arc<Shared>) -> Output {
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let thread = std::thread::spawn(move || {
        let mut last = Instant::now();
        let mut carry = 0.0f64;
        let mut scratch = Vec::<f32>::new();
        while !stop2.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(10));
            let now = Instant::now();
            let due = now.duration_since(last).as_secs_f64() * shared.rate as f64 + carry;
            last = now;
            let frames = due.floor() as usize;
            carry = due - frames as f64;
            if shared.playing.load(Ordering::Relaxed) {
                scratch.resize(frames * CHANNELS, 0.0);
                shared.fill(&mut scratch, CHANNELS);
            }
        }
    });
    Output::Null {
        stop,
        thread: Some(thread),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_keeps_rate() {
        let mut r = Resampler::new(44_100);
        let mut out = VecDeque::new();
        let input: Vec<f32> = (0..48_000 * 2).map(|i| (i / 2) as f32).collect();
        for chunk in input.chunks(1024 * 2) {
            r.process(chunk, &mut out);
        }
        let frames = out.len() / 2;
        assert!((frames as i64 - 44_100).abs() <= 2, "{frames}");
        // 単調増加の入力は単調増加の出力になる（補間の連続性）
        let left: Vec<f32> = out.iter().step_by(2).copied().collect();
        assert!(left.windows(2).all(|w| w[1] >= w[0]));
    }
}
