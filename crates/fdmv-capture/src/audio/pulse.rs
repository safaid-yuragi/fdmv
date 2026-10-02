//! Linux: PulseAudio（PipeWire の pipewire-pulse を含む）の外部コマンドで録る。
//!
//! - アプリの一覧: `pactl list sink-inputs` / `pactl list clients`
//! - アプリの音声: `parec --monitor-stream=N` でアプリの再生ストリームを横から録る。
//!   ストリームは再生のたびに作り直されることがあるので、`pactl subscribe` で増減を見張り、
//!   対象アプリのストリームが現れるたびに録り始め、消えたら止める。
//!   PipeWire は録っていたストリームが消えると parec を既定の入力（マイクなど）へつなぎ替えるので、
//!   `node.dont-reconnect` を付けてそれを防ぐ。
//! - マイク: `parec -d SOURCE`
//!
//! `pactl -f json` は ASCII 以外の文字を含む値を壊すので、テキスト出力（C ロケール）を読む。

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::{AudioApp, Capture, MicDevice};
use crate::SAMPLE_RATE;
use crate::chain::{Chain, before};

/// parec に頼む遅延。届いたサンプルはこの分だけ前に鳴っていたとみなす。
const LATENCY_MS: u64 = 20;

fn pactl(args: &[&str]) -> Result<String> {
    let out = Command::new("pactl")
        .args(args)
        .env("LC_ALL", "C.UTF-8")
        .stdin(Stdio::null())
        .output()
        .context("pactl を実行できません（pulseaudio-utils をインストールしてください）")?;
    if !out.status.success() {
        bail!(
            "pactl {} が失敗しました: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `pactl list ...` の 1 項目。
#[derive(Debug, Default)]
struct Item {
    index: u32,
    fields: HashMap<String, String>,
    props: HashMap<String, String>,
}

fn parse_list(text: &str) -> Vec<Item> {
    let mut items: Vec<Item> = Vec::new();
    for line in text.lines() {
        if !line.starts_with(char::is_whitespace) {
            if let Some((_, n)) = line.rsplit_once('#')
                && let Ok(index) = n.trim().parse()
            {
                items.push(Item {
                    index,
                    ..Default::default()
                });
            }
            continue;
        }
        let Some(item) = items.last_mut() else {
            continue;
        };
        if let Some(rest) = line.strip_prefix("\t\t") {
            if let Some((k, v)) = rest.split_once(" = ") {
                item.props.insert(k.trim().to_owned(), unquote(v.trim()));
            }
        } else if let Some(rest) = line.strip_prefix('\t')
            && let Some((k, v)) = rest.split_once(':')
        {
            item.fields.insert(k.trim().to_owned(), v.trim().to_owned());
        }
    }
    items
}

fn unquote(v: &str) -> String {
    let v = v
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(v);
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// 再生ストリーム 1 本。
#[derive(Debug)]
struct PlaybackStream {
    index: u32,
    app: AudioApp,
}

fn playback_streams() -> Result<Vec<PlaybackStream>> {
    let inputs = parse_list(&pactl(&["list", "sink-inputs"])?);
    let clients: HashMap<u32, Item> = parse_list(&pactl(&["list", "clients"])?)
        .into_iter()
        .map(|c| (c.index, c))
        .collect();
    let me = std::process::id();
    let mut out = Vec::new();
    for si in inputs {
        let client = si
            .fields
            .get("Client")
            .and_then(|c| c.parse::<u32>().ok())
            .and_then(|c| clients.get(&c));
        let get = |k: &str| {
            si.props
                .get(k)
                .or_else(|| client.and_then(|c| c.props.get(k)))
                .filter(|v| !v.is_empty())
                .cloned()
        };
        let pid = get("application.process.id").and_then(|p| p.parse::<u32>().ok());
        if pid == Some(me) {
            continue;
        }
        let binary = get("application.process.binary");
        let app_name = get("application.name");
        let node = get("node.name");
        let Some(key) = binary.clone().or(app_name.clone()).or(node.clone()) else {
            continue;
        };
        let name = app_name.or(binary).or(node).unwrap_or_else(|| key.clone());
        let detail = get("media.name")
            .filter(|m| *m != name && m != "(null)" && m != "Playback Stream")
            .unwrap_or_default();
        let playing = si.fields.get("Corked").is_none_or(|c| c == "no");
        out.push(PlaybackStream {
            index: si.index,
            app: AudioApp {
                key,
                name,
                detail,
                pid,
                playing,
            },
        });
    }
    Ok(out)
}

pub fn list_apps() -> Result<Vec<AudioApp>> {
    // 同じアプリの複数のストリームは 1 つにまとめる。
    let mut apps: BTreeMap<String, AudioApp> = BTreeMap::new();
    for s in playback_streams()? {
        match apps.get_mut(&s.app.key) {
            Some(a) => {
                a.playing |= s.app.playing;
                if !s.app.detail.is_empty() && !a.detail.contains(&s.app.detail) {
                    if !a.detail.is_empty() {
                        a.detail.push_str(" / ");
                    }
                    a.detail.push_str(&s.app.detail);
                }
            }
            None => {
                apps.insert(s.app.key.clone(), s.app);
            }
        }
    }
    Ok(apps.into_values().collect())
}

pub fn list_mics() -> Result<Vec<MicDevice>> {
    Ok(parse_list(&pactl(&["list", "sources"])?)
        .into_iter()
        .filter_map(|s| {
            let name = s.fields.get("Name")?.clone();
            let monitor = s.props.get("device.class").is_some_and(|c| c == "monitor")
                || name.ends_with(".monitor");
            (!monitor).then(|| MicDevice {
                name: s.fields.get("Description").cloned().unwrap_or(name.clone()),
                id: Some(name),
            })
        })
        .collect())
}

/// parec 1 つ分の録音。
struct Parec {
    child: Child,
    thread: Option<JoinHandle<()>>,
}

impl Parec {
    fn spawn(source_args: &[String], chain: Arc<Chain>) -> Result<Parec> {
        let mut child = Command::new("parec")
            .args(source_args)
            .args([
                "--format=float32le",
                "--channels=2",
                "--raw",
                "--client-name=fdmv-recorder",
            ])
            .arg(format!("--rate={SAMPLE_RATE}"))
            .arg(format!("--latency-msec={LATENCY_MS}"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("parec を実行できません（pulseaudio-utils をインストールしてください）")?;
        let mut stdout = child.stdout.take().unwrap();
        let thread = std::thread::Builder::new()
            .name("fdmv-parec".into())
            .spawn(move || {
                let mut take = chain.take(SAMPLE_RATE, 2);
                let mut bytes = vec![0u8; 3840 * 2];
                let mut fill = 0usize;
                let mut samples = Vec::new();
                loop {
                    let n = match stdout.read(&mut bytes[fill..]) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    let now = Instant::now();
                    fill += n;
                    let whole = fill / 8 * 8;
                    samples.clear();
                    samples.extend(
                        bytes[..whole]
                            .chunks_exact(4)
                            .map(|c| f32::from_le_bytes(c.try_into().unwrap())),
                    );
                    take.push_at(&samples, before(now, Duration::from_millis(LATENCY_MS)));
                    bytes.copy_within(whole..fill, 0);
                    fill -= whole;
                }
            })?;
        Ok(Parec {
            child,
            thread: Some(thread),
        })
    }

    fn finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }
}

impl Drop for Parec {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct AppCapture {
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    subscribe: Arc<Mutex<Option<Child>>>,
    thread: Option<JoinHandle<()>>,
}

impl Capture for AppCapture {
    fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }
}

impl Drop for AppCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(mut c) = self.subscribe.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub fn capture_app(app: &AudioApp, chain: Arc<Chain>) -> Result<Box<dyn Capture>> {
    let key = app.key.clone();
    // 最初の一覧は呼び出し元のスレッドで取る（pactl が使えなければここでエラーにする）。
    let first = playback_streams()?;
    let stop = Arc::new(AtomicBool::new(false));
    let error: Arc<Mutex<Option<String>>> = Arc::default();

    // ストリームの増減の通知。
    let (tx, rx) = channel::<()>();
    let subscribe: Arc<Mutex<Option<Child>>> = Arc::default();
    match Command::new("pactl")
        .arg("subscribe")
        .env("LC_ALL", "C.UTF-8")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            let out = child.stdout.take().unwrap();
            *subscribe.lock().unwrap() = Some(child);
            std::thread::spawn(move || {
                for line in BufReader::new(out).lines().map_while(|l| l.ok()) {
                    if line.contains("sink-input") && tx.send(()).is_err() {
                        break;
                    }
                }
            });
        }
        // 通知が無くても定期的に見直すので録音はできる。
        Err(_) => drop(tx),
    }

    let thread = {
        let (stop, error) = (stop.clone(), error.clone());
        std::thread::Builder::new()
            .name("fdmv-pulse-watch".into())
            .spawn(move || {
                let mut active: HashMap<u32, Parec> = HashMap::new();
                let mut streams = Ok(first);
                while !stop.load(Ordering::Relaxed) {
                    match streams {
                        Ok(list) => {
                            // 消えたストリームの parec は止める（止めないと、PipeWire が既定の入力に
                            // つなぎ替えてマイクの音を録り続けることがある）。
                            active.retain(|index, _| list.iter().any(|s| s.index == *index));
                            for s in list.into_iter().filter(|s| s.app.key == key) {
                                if active.contains_key(&s.index) {
                                    continue;
                                }
                                let args = vec![
                                    format!("--monitor-stream={}", s.index),
                                    // 録っているストリームが消えても、ほかの入力につなぎ替えさせない。
                                    "--property=node.dont-reconnect=true".to_owned(),
                                ];
                                match Parec::spawn(&args, chain.clone()) {
                                    Ok(p) => {
                                        active.insert(s.index, p);
                                    }
                                    Err(e) => *error.lock().unwrap() = Some(format!("{e:#}")),
                                }
                            }
                        }
                        Err(e) => *error.lock().unwrap() = Some(format!("{e:#}")),
                    }
                    active.retain(|_, p| !p.finished());
                    // 通知が来るか、1 秒ごとに見直す。
                    match rx.recv_timeout(Duration::from_secs(1)) {
                        Ok(()) => while rx.try_recv().is_ok() {},
                        Err(RecvTimeoutError::Timeout) => {}
                        // 通知が使えない（または止めるところ）: 少しずつ待って止める指示に気づけるようにする。
                        Err(RecvTimeoutError::Disconnected) => {
                            for _ in 0..10 {
                                if stop.load(Ordering::Relaxed) {
                                    break;
                                }
                                std::thread::sleep(Duration::from_millis(50));
                            }
                        }
                    }
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    streams = playback_streams();
                }
            })?
    };
    Ok(Box::new(AppCapture {
        stop,
        error,
        subscribe,
        thread: Some(thread),
    }))
}

struct MicCapture {
    parec: Parec,
}

impl Capture for MicCapture {
    fn error(&self) -> Option<String> {
        self.parec
            .finished()
            .then(|| "マイクの録音が止まりました".to_owned())
    }
}

pub fn capture_mic(device: &MicDevice, chain: Arc<Chain>) -> Result<Box<dyn Capture>> {
    let source = device.id.as_deref().unwrap_or("@DEFAULT_SOURCE@");
    let parec = Parec::spawn(&[format!("--device={source}")], chain)?;
    Ok(Box::new(MicCapture { parec }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pactl_list() {
        let text = "Sink Input #159\n\tDriver: PipeWire\n\tClient: 105\n\tCorked: no\n\tVolume: front-left: 1\n\t        balance 0.00\n\tProperties:\n\t\tmedia.name = \"(146) [マイン] \\\"x\\\" - YouTube\"\n\t\tapplication.name = \"Firefox\"\nSink Input #160\n\tClient: 7\n";
        let items = parse_list(text);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].index, 159);
        assert_eq!(items[0].fields["Client"], "105");
        assert_eq!(items[0].fields["Corked"], "no");
        assert_eq!(
            items[0].props["media.name"],
            "(146) [マイン] \"x\" - YouTube"
        );
        assert_eq!(items[0].props["application.name"], "Firefox");
        assert_eq!(items[1].fields["Client"], "7");
    }
}
