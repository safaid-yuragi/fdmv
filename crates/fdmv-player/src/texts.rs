//! UI の文言。日本語フォントが見つからない環境では英語で表示する。

pub struct Texts {
    pub file: &'static str,
    pub open: &'static str,
    pub close: &'static str,
    pub quit: &'static str,
    pub view: &'static str,
    pub info: &'static str,
    pub fullscreen: &'static str,
    pub chains_panel: &'static str,
    pub chains: &'static str,
    pub default_tag: &'static str,
    pub volume: &'static str,
    pub master: &'static str,
    pub mute: &'static str,
    pub segments: &'static str,
    pub no_chains: &'static str,
    pub drop_hint: &'static str,
    pub keys_hint: &'static str,
    pub error: &'static str,
    pub ok: &'static str,
    pub audio_device: &'static str,
    pub no_audio_device: &'static str,
    pub video: &'static str,
    pub duration: &'static str,
    pub metadata: &'static str,
    pub fdmv_files: &'static str,
}

pub const JA: Texts = Texts {
    file: "ファイル",
    open: "開く…",
    close: "閉じる",
    quit: "終了",
    view: "表示",
    info: "ファイル情報",
    fullscreen: "全画面",
    chains_panel: "チェーン一覧",
    chains: "チェーン",
    default_tag: "デフォルト",
    volume: "音量",
    master: "全体音量",
    mute: "ミュート",
    segments: "区間",
    no_chains: "このファイルにはチェーンがありません",
    drop_hint: if cfg!(target_os = "macos") {
        ".fdmv ファイルをここにドロップするか、⌘O で開いてください"
    } else {
        ".fdmv ファイルをここにドロップするか、Ctrl+O で開いてください"
    },
    keys_hint: "Space: 再生/一時停止   ←/→: 5 秒移動   ↑/↓: 音量   M: ミュート   F: 全画面",
    error: "エラー",
    ok: "OK",
    audio_device: "音声出力",
    no_audio_device: "音声出力デバイスがありません（無音で再生します）",
    video: "映像",
    duration: "長さ",
    metadata: "メタデータ",
    fdmv_files: "FDMV 動画",
};

pub const EN: Texts = Texts {
    file: "File",
    open: "Open…",
    close: "Close",
    quit: "Quit",
    view: "View",
    info: "File info",
    fullscreen: "Fullscreen",
    chains_panel: "Chains",
    chains: "Chains",
    default_tag: "default",
    volume: "Volume",
    master: "Master",
    mute: "Mute",
    segments: "segments",
    no_chains: "This file has no chains",
    drop_hint: if cfg!(target_os = "macos") {
        "Drop a .fdmv file here or press ⌘O to open one"
    } else {
        "Drop a .fdmv file here or press Ctrl+O to open one"
    },
    keys_hint: "Space: play/pause   ←/→: seek 5 s   ↑/↓: volume   M: mute   F: fullscreen",
    error: "Error",
    ok: "OK",
    audio_device: "Audio output",
    no_audio_device: "No audio output device (playing silently)",
    video: "Video",
    duration: "Duration",
    metadata: "Metadata",
    fdmv_files: "FDMV video",
};
