//! マイク（cpal）。Windows / macOS で使う。
//!
//! デバイスの形式（サンプルレート・チャンネル数）のまま書き、48 kHz ステレオへの変換は停止後に ffmpeg で行う。

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};

use super::{Capture, MicDevice};
use crate::chain::{Chain, Take};

pub fn list_mics() -> Result<Vec<MicDevice>> {
    let host = cpal::default_host();
    let mut v = Vec::new();
    for d in host.input_devices()? {
        let (Ok(id), Ok(desc)) = (d.id(), d.description()) else {
            continue;
        };
        v.push(MicDevice {
            id: Some(id.to_string()),
            name: desc.name().to_owned(),
        });
    }
    Ok(v)
}

struct MicCapture {
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    thread: Option<JoinHandle<()>>,
}

impl Capture for MicCapture {
    fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }
}

impl Drop for MicCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

pub fn capture_mic(device: &MicDevice, chain: Arc<Chain>) -> Result<Box<dyn Capture>> {
    let id = device.id.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let error: Arc<Mutex<Option<String>>> = Arc::default();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
    // cpal のストリームはスレッドをまたげない OS があるので、専用スレッドで持つ。
    let thread = {
        let (stop, error) = (stop.clone(), error.clone());
        std::thread::Builder::new()
            .name("fdmv-mic".into())
            .spawn(move || {
                let stream = match open(id.as_deref(), chain, error.clone()) {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(()));
                while !stop.load(Ordering::Relaxed) {
                    std::thread::park_timeout(Duration::from_millis(200));
                }
                drop(stream);
            })?
    };
    ready_rx
        .recv()
        .map_err(|_| anyhow!("マイクを開けません"))??;
    Ok(Box::new(MicCapture {
        stop,
        error,
        thread: Some(thread),
    }))
}

fn open(
    id: Option<&str>,
    chain: Arc<Chain>,
    error: Arc<Mutex<Option<String>>>,
) -> Result<cpal::Stream> {
    let host = cpal::default_host();
    let device = match id {
        None => host.default_input_device().context("マイクがありません")?,
        Some(id) => {
            let id = id.parse().map_err(|_| anyhow!("マイクの指定が不正です"))?;
            host.device_by_id(&id).context("マイクが見つかりません")?
        }
    };
    let config = device.default_input_config()?;
    let rate = config.sample_rate();
    let channels = config.channels();
    let take = chain.take(rate, channels);
    let stream = match config.sample_format() {
        SampleFormat::F32 => build::<f32>(&device, &config, take, error)?,
        SampleFormat::I16 => build::<i16>(&device, &config, take, error)?,
        SampleFormat::U16 => build::<u16>(&device, &config, take, error)?,
        SampleFormat::I32 => build::<i32>(&device, &config, take, error)?,
        SampleFormat::F64 => build::<f64>(&device, &config, take, error)?,
        f => bail!("マイクの形式 {f:?} には対応していません"),
    };
    stream.play()?;
    Ok(stream)
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    mut take: Take,
    error: Arc<Mutex<Option<String>>>,
) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let mut buf: Vec<f32> = Vec::new();
    Ok(device.build_input_stream(
        config.config(),
        move |data: &[T], _| {
            let now = Instant::now();
            buf.clear();
            buf.extend(data.iter().map(|&s| s.to_sample::<f32>()));
            take.push_at(&buf, now);
        },
        move |e| *error.lock().unwrap() = Some(format!("マイク: {e}")),
        None,
    )?)
}
