//! macOS: ScreenCaptureKit（macOS 13 以降）。映像（ディスプレイ・ウィンドウ）とアプリ単位の音声の両方。
//!
//! - 映像: `SCStream` の画面出力（BGRA）を [`FrameSlot`] に置く。
//! - アプリの音声: アプリ 1 つにつき 1 本の `SCStream` を作り、コンテンツフィルタをそのアプリだけにして
//!   音声出力だけを受け取る（ScreenCaptureKit の音声はフィルタに含まれるアプリの音だけになる）。
//!
//! 初回は「画面収録」の許可が必要（システム設定 → プライバシーとセキュリティ → 画面収録）。

use std::ptr::NonNull;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_audio_types::{AudioBuffer, AudioBufferList};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelFormatType_32BGRA,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCRunningApplication, SCShareableContent, SCStream,
    SCStreamConfiguration, SCStreamOutput, SCStreamOutputType, SCWindow,
};

use super::{Frame, FrameSlot, PixelOrder, TargetInfo, VideoCapture, VideoTarget};
use crate::SAMPLE_RATE;
use crate::audio::{AudioApp, Capture};
use crate::chain::{Chain, Take};

/// Objective-C のオブジェクトをスレッド間で受け渡すための入れ物。
/// ScreenCaptureKit のオブジェクトはどのスレッドから使ってもよい。
struct SendBox<T>(T);
unsafe impl<T> Send for SendBox<T> {}

fn error_text(e: *mut NSError) -> String {
    match unsafe { e.as_ref() } {
        Some(e) => e.localizedDescription().to_string(),
        None => "不明なエラー".into(),
    }
}

fn shareable_content() -> Result<Retained<SCShareableContent>> {
    let (tx, rx) =
        mpsc::channel::<std::result::Result<SendBox<Retained<SCShareableContent>>, String>>();
    let block = RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
        let r = match unsafe { Retained::retain(content) } {
            Some(c) => Ok(SendBox(c)),
            None => Err(error_text(err)),
        };
        let _ = tx.send(r);
    });
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            true, true, &block,
        );
    }
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(c)) => Ok(c.0),
        Ok(Err(e)) => bail!(
            "画面の情報を取得できません（システム設定 → プライバシーとセキュリティ → 画面収録 で許可してください）: {e}"
        ),
        Err(_) => bail!("画面の情報を取得できません（応答がありません）"),
    }
}

/// 録画対象にするウィンドウ（通常のレイヤーでタイトルのあるもの）。
fn capturable_windows(content: &SCShareableContent) -> Vec<Retained<SCWindow>> {
    let me = std::process::id() as i32;
    unsafe { content.windows() }
        .iter()
        .filter(|w| unsafe {
            w.windowLayer() == 0
                && w.title().is_some_and(|t| t.length() > 0)
                && w.owningApplication().is_none_or(|a| a.processID() != me)
        })
        .collect()
}

fn app_key(app: &SCRunningApplication) -> String {
    let id = unsafe { app.bundleIdentifier() }.to_string();
    if id.is_empty() {
        unsafe { app.processID() }.to_string()
    } else {
        id
    }
}

pub fn targets() -> Result<Vec<TargetInfo>> {
    let content = shareable_content()?;
    let mut v = Vec::new();
    for (i, d) in unsafe { content.displays() }.iter().enumerate() {
        let (w, h) = unsafe { (d.width(), d.height()) };
        v.push(TargetInfo {
            target: VideoTarget::MacDisplay {
                id: unsafe { d.displayID() },
            },
            label: format!("ディスプレイ {}（{w}×{h}）", i + 1),
            app_key: None,
        });
    }
    for w in capturable_windows(&content) {
        let title = unsafe { w.title() }
            .map(|t| t.to_string())
            .unwrap_or_default();
        let app = unsafe { w.owningApplication() };
        let app_name = app
            .as_ref()
            .map(|a| unsafe { a.applicationName() }.to_string())
            .unwrap_or_default();
        v.push(TargetInfo {
            target: VideoTarget::MacWindow {
                id: unsafe { w.windowID() },
            },
            label: format!("ウィンドウ: {title}（{app_name}）"),
            app_key: app.as_deref().map(app_key),
        });
    }
    Ok(v)
}

pub fn list_apps() -> Result<Vec<AudioApp>> {
    let content = shareable_content()?;
    let windows = capturable_windows(&content);
    let me = std::process::id() as i32;
    let mut apps = Vec::new();
    for a in unsafe { content.applications() }.iter() {
        let pid = unsafe { a.processID() };
        let name = unsafe { a.applicationName() }.to_string();
        if pid == me || name.is_empty() {
            continue;
        }
        let titles: Vec<String> = windows
            .iter()
            .filter(|w| {
                unsafe { w.owningApplication() }.is_some_and(|o| unsafe { o.processID() } == pid)
            })
            .filter_map(|w| unsafe { w.title() }.map(|t| t.to_string()))
            .collect();
        // ウィンドウを持たない常駐アプリは除く（一覧が長くなりすぎるため）。
        if titles.is_empty() {
            continue;
        }
        apps.push(AudioApp {
            key: app_key(&a),
            name,
            detail: titles.join(" / "),
            pid: u32::try_from(pid).ok(),
            playing: true,
        });
    }
    apps.sort_by_key(|a| a.name.to_lowercase());
    Ok(apps)
}

// ---------------------------------------------------------------------------
// SCStream の出力を受け取るオブジェクト

struct OutputIvars {
    slot: Option<Arc<FrameSlot>>,
    take: Option<Mutex<Take>>,
}

define_class!(
    // SAFETY: NSObject にはサブクラス化の要件が無く、Drop も実装していない。
    #[unsafe(super(NSObject))]
    #[ivars = OutputIvars]
    struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output_sample_buffer(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            r#type: SCStreamOutputType,
        ) {
            if r#type == SCStreamOutputType::Screen {
                if let Some(slot) = &self.ivars().slot {
                    on_video(slot, sample_buffer);
                }
            } else if r#type == SCStreamOutputType::Audio
                && let Some(take) = &self.ivars().take
            {
                on_audio(&mut take.lock().unwrap(), sample_buffer);
            }
        }
    }
);

impl StreamOutput {
    fn new(ivars: OutputIvars) -> Retained<Self> {
        let this = Self::alloc().set_ivars(ivars);
        unsafe { msg_send![super(this), init] }
    }
}

fn on_video(slot: &FrameSlot, sample: &CMSampleBuffer) {
    let Some(image) = (unsafe { sample.image_buffer() }) else {
        return;
    };
    if unsafe { CVPixelBufferLockBaseAddress(&image, CVPixelBufferLockFlags::ReadOnly) } != 0 {
        return;
    }
    let (w, h) = (
        CVPixelBufferGetWidth(&image),
        CVPixelBufferGetHeight(&image),
    );
    let stride = CVPixelBufferGetBytesPerRow(&image);
    let base = CVPixelBufferGetBaseAddress(&image);
    if !base.is_null() && w > 0 && h > 0 {
        let data = unsafe { std::slice::from_raw_parts(base as *const u8, stride * h) };
        if let Some(f) = Frame::from_strided(w as u32, h as u32, stride, data, PixelOrder::Bgra) {
            slot.put(f);
        }
    }
    unsafe { CVPixelBufferUnlockBaseAddress(&image, CVPixelBufferLockFlags::ReadOnly) };
}

fn on_audio(take: &mut Take, sample: &CMSampleBuffer) {
    let now = Instant::now();
    // AudioBufferList は可変長。ステレオの非インターリーブ（バッファ 2 つ）まで入る大きさを用意する。
    #[repr(C)]
    struct List {
        list: AudioBufferList,
        more: [AudioBuffer; 7],
    }
    let mut list: List = unsafe { std::mem::zeroed() };
    let mut block = std::ptr::null_mut();
    let status = unsafe {
        sample.audio_buffer_list_with_retained_block_buffer(
            std::ptr::null_mut(),
            &mut list.list,
            std::mem::size_of::<List>(),
            None,
            None,
            1, // kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment
            &mut block,
        )
    };
    if status != 0 {
        return;
    }
    let n = list.list.mNumberBuffers as usize;
    let buffers: &[AudioBuffer] =
        unsafe { std::slice::from_raw_parts(list.list.mBuffers.as_ptr(), n.min(8)) };
    let mut out: Vec<f32> = Vec::new();
    // ScreenCaptureKit の音声は 32 bit float。
    let as_f32 = |b: &AudioBuffer| -> &[f32] {
        if b.mData.is_null() {
            return &[];
        }
        unsafe { std::slice::from_raw_parts(b.mData as *const f32, b.mDataByteSize as usize / 4) }
    };
    match buffers {
        [one] if one.mNumberChannels == 2 => out.extend_from_slice(as_f32(one)),
        [one] => {
            for &s in as_f32(one) {
                out.extend_from_slice(&[s, s]);
            }
        }
        [l, r, ..] => {
            for (&a, &b) in as_f32(l).iter().zip(as_f32(r)) {
                out.extend_from_slice(&[a, b]);
            }
        }
        [] => {}
    }
    if !block.is_null() {
        // retained で受け取ったブロックバッファを解放する。
        unsafe { objc2_core_foundation::CFRetained::from_raw(NonNull::new_unchecked(block)) };
    }
    take.push_at(&out, now);
}

// ---------------------------------------------------------------------------
// ストリーム

struct Stream {
    stream: Retained<SCStream>,
    _output: Retained<StreamOutput>,
    _queue: DispatchRetained<DispatchQueue>,
}

unsafe impl Send for Stream {}

impl Stream {
    fn start(
        filter: &SCContentFilter,
        config: &SCStreamConfiguration,
        output: Retained<StreamOutput>,
        kind: SCStreamOutputType,
    ) -> Result<Stream> {
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), filter, config, None)
        };
        let queue = DispatchQueue::new("fdmv-capture", None);
        unsafe {
            stream.addStreamOutput_type_sampleHandlerQueue_error(
                ProtocolObject::from_ref(&*output),
                kind,
                Some(&queue),
            )
        }
        .map_err(|e| anyhow!("{}", e.localizedDescription()))?;
        let (tx, rx) = mpsc::channel::<Option<String>>();
        let block = RcBlock::new(move |err: *mut NSError| {
            let _ = tx.send((!err.is_null()).then(|| error_text(err)));
        });
        unsafe { stream.startCaptureWithCompletionHandler(Some(&block)) };
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(None) => {}
            Ok(Some(e)) => bail!("取り込みを開始できません: {e}"),
            Err(_) => bail!("取り込みを開始できません（応答がありません）"),
        }
        Ok(Stream {
            stream,
            _output: output,
            _queue: queue,
        })
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let (tx, rx) = mpsc::channel::<()>();
        let block = RcBlock::new(move |_: *mut NSError| {
            let _ = tx.send(());
        });
        unsafe { self.stream.stopCaptureWithCompletionHandler(Some(&block)) };
        let _ = rx.recv_timeout(Duration::from_secs(3));
        // 書きかけの音声のパートを閉じる（出力オブジェクトがいつ解放されるかは OS 次第なので）。
        if let Some(t) = &self._output.ivars().take {
            t.lock().unwrap().close();
        }
    }
}

fn config(width: usize, height: usize, fps: u32) -> Retained<SCStreamConfiguration> {
    let c = unsafe { SCStreamConfiguration::new() };
    unsafe {
        c.setWidth(width.max(2));
        c.setHeight(height.max(2));
        c.setMinimumFrameInterval(CMTime::new(1, fps.max(1) as i32));
        c.setPixelFormat(kCVPixelFormatType_32BGRA);
        c.setShowsCursor(true);
        c.setQueueDepth(5);
    }
    c
}

/// フィルタの内容の点→画素の倍率（macOS 14 以降。それより前は 1）。
fn pixel_scale(filter: &SCContentFilter) -> f64 {
    if objc2::available!(macos = 14.0) {
        let info = unsafe { SCShareableContent::infoForFilter(filter) };
        (unsafe { info.pointPixelScale() } as f64).max(1.0)
    } else {
        1.0
    }
}

struct MacVideo {
    _stream: Stream,
    slot: Arc<FrameSlot>,
}

impl VideoCapture for MacVideo {
    fn slot(&self) -> Arc<FrameSlot> {
        self.slot.clone()
    }

    fn error(&self) -> Option<String> {
        None
    }
}

pub fn start_video(target: &VideoTarget, fps: u32) -> Result<Box<dyn VideoCapture>> {
    let content = shareable_content()?;
    let displays = unsafe { content.displays() };
    let (filter, w, h) = match target {
        VideoTarget::MacDisplay { id } => {
            let d: Retained<SCDisplay> = displays
                .iter()
                .find(|d| unsafe { d.displayID() } == *id)
                .ok_or_else(|| anyhow!("ディスプレイが見つかりません"))?;
            let filter = unsafe {
                SCContentFilter::initWithDisplay_excludingWindows(
                    SCContentFilter::alloc(),
                    &d,
                    &NSArray::new(),
                )
            };
            let (w, h) = unsafe { (d.width() as f64, d.height() as f64) };
            (filter, w, h)
        }
        VideoTarget::MacWindow { id } => {
            let win = unsafe { content.windows() }
                .iter()
                .find(|w| unsafe { w.windowID() } == *id)
                .ok_or_else(|| {
                    anyhow!("ウィンドウが見つかりません（閉じられた可能性があります）")
                })?;
            let filter = unsafe {
                SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &win)
            };
            let frame = unsafe { win.frame() };
            (filter, frame.size.width, frame.size.height)
        }
        _ => bail!("この録画対象は macOS では使えません"),
    };
    let scale = pixel_scale(&filter);
    let (w, h) = ((w * scale).round() as usize, (h * scale).round() as usize);
    let slot = Arc::new(FrameSlot::default());
    let output = StreamOutput::new(OutputIvars {
        slot: Some(slot.clone()),
        take: None,
    });
    let stream = Stream::start(
        &filter,
        &config(w, h, fps),
        output,
        SCStreamOutputType::Screen,
    )?;
    Ok(Box::new(MacVideo {
        _stream: stream,
        slot,
    }))
}

struct MacAudio {
    _stream: Stream,
}

impl Capture for MacAudio {
    fn error(&self) -> Option<String> {
        None
    }
}

pub fn capture_app(app: &AudioApp, chain: Arc<Chain>) -> Result<Box<dyn Capture>> {
    let content = shareable_content()?;
    let target = unsafe { content.applications() }
        .iter()
        .find(|a| app_key(a) == app.key)
        .ok_or_else(|| anyhow!("{} が見つかりません（終了した可能性があります）", app.name))?;
    let display = unsafe { content.displays() }
        .iter()
        .next()
        .ok_or_else(|| anyhow!("ディスプレイがありません"))?;
    let filter = unsafe {
        SCContentFilter::initWithDisplay_includingApplications_exceptingWindows(
            SCContentFilter::alloc(),
            &display,
            &NSArray::from_retained_slice(&[target]),
            &NSArray::new(),
        )
    };
    // 映像は使わないので最小にする。
    let config = config(2, 2, 1);
    unsafe {
        config.setCapturesAudio(true);
        config.setSampleRate(SAMPLE_RATE as isize);
        config.setChannelCount(2);
        config.setExcludesCurrentProcessAudio(true);
    }
    let output = StreamOutput::new(OutputIvars {
        slot: None,
        take: Some(Mutex::new(chain.take(SAMPLE_RATE, 2))),
    });
    let stream = Stream::start(&filter, &config, output, SCStreamOutputType::Audio)?;
    Ok(Box::new(MacAudio { _stream: stream }))
}
