//! Windows: WASAPI のプロセス単位ループバック（Windows 10 バージョン 2004 以降）。
//!
//! `ActivateAudioInterfaceAsync` に `VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK` と対象のプロセス ID を渡すと、
//! そのプロセス（と子プロセス。ブラウザの音声プロセスなど）が鳴らしている音だけを録れる。
//! アプリの一覧は、見えているウィンドウを持つプロセス。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDIOCLIENT_ACTIVATION_PARAMS,
    AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
    AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, ActivateAudioInterfaceAsync,
    IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler,
    IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient, IAudioClient,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    WAVEFORMATEX,
};
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::{BLOB, COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Threading::{
    CreateEventW, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW, WaitForSingleObject,
};
use windows::Win32::System::Variant::VT_BLOB;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GWL_EXSTYLE, GetWindowLongW, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsWindowVisible, WS_EX_TOOLWINDOW,
};
use windows::core::{BOOL, Interface, implement};

use super::{AudioApp, Capture};
use crate::SAMPLE_RATE;
use crate::chain::Chain;

/// 見えているトップレベルウィンドウ（ハンドル, タイトル, プロセス ID）。
pub(crate) fn visible_windows() -> Vec<(HWND, String, u32)> {
    unsafe extern "system" fn callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let out = unsafe { &mut *(lparam.0 as *mut Vec<(HWND, String, u32)>) };
        unsafe {
            if !IsWindowVisible(hwnd).as_bool() {
                return BOOL(1);
            }
            if GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0 {
                return BOOL(1);
            }
            let len = GetWindowTextLengthW(hwnd);
            if len <= 0 {
                return BOOL(1);
            }
            let mut buf = vec![0u16; len as usize + 1];
            let n = GetWindowTextW(hwnd, &mut buf);
            let title = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid != 0 && pid != std::process::id() {
                out.push((hwnd, title, pid));
            }
        }
        BOOL(1)
    }
    let mut out: Vec<(HWND, String, u32)> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(callback), LPARAM(&mut out as *mut _ as isize));
    }
    out
}

/// プロセスの実行ファイル名（拡張子なし）。
pub(crate) fn process_name(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            h,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(h);
        if !ok {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        std::path::Path::new(&path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
    }
}

pub fn list_apps() -> Result<Vec<AudioApp>> {
    let mut by_pid: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    for (_, title, pid) in visible_windows() {
        by_pid.entry(pid).or_default().push(title);
    }
    let mut apps: Vec<AudioApp> = by_pid
        .into_iter()
        .map(|(pid, titles)| {
            let name = process_name(pid).unwrap_or_else(|| format!("PID {pid}"));
            AudioApp {
                key: pid.to_string(),
                name,
                detail: titles.join(" / "),
                pid: Some(pid),
                playing: true,
            }
        })
        .collect();
    apps.sort_by_key(|a| a.name.to_lowercase());
    Ok(apps)
}

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activation {
    done: Mutex<Option<mpsc::Sender<()>>>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for Activation_Impl {
    fn ActivateCompleted(
        &self,
        _op: windows::core::Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        if let Some(tx) = self.done.lock().unwrap().take() {
            let _ = tx.send(());
        }
        Ok(())
    }
}

/// プロセス `pid`（と子プロセス）の音声を録る IAudioClient を作る。
fn activate(pid: u32) -> Result<IAudioClient> {
    let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    let prop = PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BLOB,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    blob: BLOB {
                        cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                        pBlobData: &mut params as *mut _ as *mut u8,
                    },
                },
            }),
        },
    };
    let (tx, rx) = mpsc::channel();
    let handler: IActivateAudioInterfaceCompletionHandler = Activation {
        done: Mutex::new(Some(tx)),
    }
    .into();
    let op = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&prop),
            &handler,
        )
    }
    .context("プロセスの音声を開けません（Windows 10 2004 以降が必要です）")?;
    rx.recv_timeout(std::time::Duration::from_secs(10))
        .map_err(|_| anyhow!("プロセスの音声を開けません（応答がありません）"))?;
    let mut hr = windows::core::HRESULT(0);
    let mut unk = None;
    unsafe { op.GetActivateResult(&mut hr, &mut unk)? };
    hr.ok().context("プロセスの音声を開けません")?;
    let unk = unk.ok_or_else(|| anyhow!("プロセスの音声を開けません"))?;
    // params は Activate が終わるまで生きている必要がある。
    let _ = &params;
    Ok(unk.cast::<IAudioClient>()?)
}

struct LoopbackCapture {
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    thread: Option<JoinHandle<()>>,
}

impl Capture for LoopbackCapture {
    fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }
}

impl Drop for LoopbackCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub fn capture_app(app: &AudioApp, chain: Arc<Chain>) -> Result<Box<dyn Capture>> {
    let pid = app
        .pid
        .or_else(|| app.key.parse().ok())
        .ok_or_else(|| anyhow!("プロセス ID が分かりません"))?;
    let stop = Arc::new(AtomicBool::new(false));
    let error: Arc<Mutex<Option<String>>> = Arc::default();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
    let thread = {
        let (stop, error) = (stop.clone(), error.clone());
        std::thread::Builder::new()
            .name("fdmv-wasapi".into())
            .spawn(move || {
                unsafe {
                    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                }
                if let Err(e) = run(pid, chain, &stop, &ready_tx) {
                    let msg = format!("{e:#}");
                    if ready_tx.send(Err(anyhow!(msg.clone()))).is_err() {
                        *error.lock().unwrap() = Some(msg);
                    }
                }
                unsafe { CoUninitialize() };
            })?
    };
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(Box::new(LoopbackCapture {
            stop,
            error,
            thread: Some(thread),
        })),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            bail!("プロセスの音声を開けません")
        }
    }
}

fn run(
    pid: u32,
    chain: Arc<Chain>,
    stop: &AtomicBool,
    ready: &mpsc::Sender<Result<()>>,
) -> Result<()> {
    let client = activate(pid)?;
    // プロセスループバックは GetMixFormat が使えないので、こちらで 48 kHz の float ステレオを指定する。
    let format = WAVEFORMATEX {
        wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
        nChannels: 2,
        nSamplesPerSec: SAMPLE_RATE,
        nAvgBytesPerSec: SAMPLE_RATE * 8,
        nBlockAlign: 8,
        wBitsPerSample: 32,
        cbSize: 0,
    };
    unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK
                | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            2_000_000, // 200 ms（100 ns 単位）
            0,
            &format,
            None,
        )?;
    }
    let event = unsafe { CreateEventW(None, false, false, None)? };
    let capture: IAudioCaptureClient = unsafe {
        client.SetEventHandle(event)?;
        client.GetService()?
    };
    unsafe { client.Start()? };
    let _ = ready.send(Ok(()));

    let mut take = chain.take(SAMPLE_RATE, 2);
    let mut buf: Vec<f32> = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        if unsafe { WaitForSingleObject(event, 100) } != WAIT_OBJECT_0 {
            continue;
        }
        loop {
            let packet = unsafe { capture.GetNextPacketSize()? };
            if packet == 0 {
                break;
            }
            let mut data = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)? };
            let now = Instant::now();
            let n = frames as usize * 2;
            buf.clear();
            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                buf.resize(n, 0.0);
            } else {
                let samples = unsafe { std::slice::from_raw_parts(data as *const f32, n) };
                buf.extend_from_slice(samples);
            }
            unsafe { capture.ReleaseBuffer(frames)? };
            take.push_at(&buf, now);
        }
    }
    unsafe {
        let _ = client.Stop();
        let _ = CloseHandle(event);
    }
    Ok(())
}
