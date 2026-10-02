//! アプリごとの音声とマイクの取り込み。
//!
//! | OS | アプリの音声 | マイク |
//! |---|---|---|
//! | Linux | PulseAudio / PipeWire のアプリのストリームを横から録音（`pactl` / `parec`） | `parec` |
//! | Windows | WASAPI のプロセス単位ループバック（Windows 10 2004 以降） | cpal |
//! | macOS | ScreenCaptureKit のアプリ単位の音声（macOS 13 以降） | cpal |
//!
//! どれも、普段どおりスピーカーから鳴っている音をそのまま写し取るだけで、再生の経路は変えない。

use std::sync::Arc;

use anyhow::Result;

use crate::chain::Chain;

#[cfg(not(target_os = "linux"))]
mod mic;
#[cfg(target_os = "linux")]
mod pulse;
#[cfg(windows)]
pub(crate) mod wasapi;

/// 音声を録れるアプリ。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioApp {
    /// アプリを見分けるキー（Linux: 実行ファイル名など、Windows: プロセス ID、macOS: バンドル ID）。
    pub key: String,
    /// 表示名。チェーン名の初期値にもなる。
    pub name: String,
    /// 補足（ウィンドウのタイトル、再生中のメディアの名前など）。
    pub detail: String,
    pub pid: Option<u32>,
    /// いま音を出しているか（分かる場合）。
    pub playing: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MicDevice {
    /// None = OS の既定のマイク。
    pub id: Option<String>,
    pub name: String,
}

/// 動いている取り込み。drop すると止まる。
pub trait Capture: Send {
    /// これ以上取り込めなくなったときの理由。
    fn error(&self) -> Option<String>;
}

/// 音声を録れるアプリの一覧。
pub fn list_apps() -> Result<Vec<AudioApp>> {
    #[cfg(target_os = "linux")]
    {
        pulse::list_apps()
    }
    #[cfg(windows)]
    {
        wasapi::list_apps()
    }
    #[cfg(target_os = "macos")]
    {
        crate::video::macos::list_apps()
    }
    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    {
        Ok(Vec::new())
    }
}

/// `app` の音声を `chain` に録り始める。
pub fn capture_app(app: &AudioApp, chain: Arc<Chain>) -> Result<Box<dyn Capture>> {
    #[cfg(target_os = "linux")]
    {
        pulse::capture_app(app, chain)
    }
    #[cfg(windows)]
    {
        wasapi::capture_app(app, chain)
    }
    #[cfg(target_os = "macos")]
    {
        crate::video::macos::capture_app(app, chain)
    }
    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    {
        let _ = (app, chain);
        anyhow::bail!("この OS ではアプリの音声を録れません")
    }
}

pub fn list_mics() -> Result<Vec<MicDevice>> {
    let mut v = vec![MicDevice {
        id: None,
        name: "既定のマイク".into(),
    }];
    #[cfg(target_os = "linux")]
    v.extend(pulse::list_mics()?);
    #[cfg(not(target_os = "linux"))]
    v.extend(mic::list_mics()?);
    Ok(v)
}

pub fn capture_mic(device: &MicDevice, chain: Arc<Chain>) -> Result<Box<dyn Capture>> {
    #[cfg(target_os = "linux")]
    {
        pulse::capture_mic(device, chain)
    }
    #[cfg(not(target_os = "linux"))]
    {
        mic::capture_mic(device, chain)
    }
}
