//! GUI なしで録画する（動作確認用）。
//!
//! ```sh
//! cargo run --release -p fdmv-capture --example record -- --list
//! cargo run --release -p fdmv-capture --example record -- \
//!     --seconds 10 --main firefox --app java --mic -o out.fdmv [--screen]
//! ```
//!
//! `--screen` を付けないと映像はテストパターンになる（大きさは `FDMV_TEST_PATTERN=2560x1440` で変えられる）。
//! `--encoders` で使える映像エンコーダの一覧、`--encoder NAME` / `--fps N` で指定。

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use fdmv_capture::audio;
use fdmv_capture::video::{self, VideoTarget};
use fdmv_capture::{AudioSource, ChainSetup, RecordOptions, Recorder};
use libfdmv::ffmpeg::Ffmpeg;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut seconds = 5.0;
    let mut main_key = None;
    let mut apps = Vec::new();
    let mut mic = false;
    let mut screen = false;
    let mut output = PathBuf::from("record.fdmv");
    let mut opts = RecordOptions::default();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--list" => {
                for app in audio::list_apps()? {
                    println!(
                        "{:<20} {} [{}] {}",
                        app.key,
                        app.name,
                        if app.playing { "再生中" } else { "停止" },
                        app.detail
                    );
                }
                for m in audio::list_mics()? {
                    println!("mic: {:?} {}", m.id, m.name);
                }
                return Ok(());
            }
            "--seconds" => seconds = args.next().context("--seconds")?.parse()?,
            "--main" => main_key = Some(args.next().context("--main")?),
            "--app" => apps.push(args.next().context("--app")?),
            "--mic" => mic = true,
            "--screen" => screen = true,
            "-o" => output = args.next().context("-o")?.into(),
            "--fps" => opts.fps = args.next().context("--fps")?.parse()?,
            "--encoder" => opts.encoder = Some(args.next().context("--encoder")?),
            "--encoders" => {
                for e in fdmv_capture::codecs::available(&Ffmpeg::locate()?) {
                    println!("{:<14} {}", e.name, e.label());
                }
                return Ok(());
            }
            _ => bail!("unknown argument {a}"),
        }
    }
    let ff = Ffmpeg::locate()?;
    let list = audio::list_apps()?;
    let find = |key: &str| {
        list.iter()
            .find(|a| a.key == key)
            .cloned()
            .with_context(|| format!("app {key} is not playing audio"))
    };
    let mut setups = Vec::new();
    if let Some(k) = &main_key {
        let app = find(k)?;
        setups.push(ChainSetup {
            name: app.name.clone(),
            main: true,
            source: AudioSource::App(app),
        });
    }
    for k in &apps {
        let app = find(k)?;
        setups.push(ChainSetup {
            name: app.name.clone(),
            main: false,
            source: AudioSource::App(app),
        });
    }
    if mic {
        setups.push(ChainSetup {
            name: "マイク".into(),
            main: false,
            source: AudioSource::Mic(audio::list_mics()?.remove(0)),
        });
    }

    let target = if screen {
        VideoTarget::Portal
    } else {
        VideoTarget::TestPattern
    };
    let capture = video::start(&ff, &target, opts.fps)?;

    let rec = Recorder::start(&ff, capture.slot(), setups, opts, output.clone())?;
    let start = Instant::now();
    while start.elapsed().as_secs_f64() < seconds {
        std::thread::sleep(Duration::from_millis(500));
        let stats = rec.video_stats();
        let chains: Vec<String> = rec
            .chain_status()
            .iter()
            .map(|c| {
                format!(
                    "{}={:.3}{}",
                    c.name,
                    c.peak,
                    if c.receiving { "*" } else { "" }
                )
            })
            .collect();
        println!(
            "{:5.1}s frames={} dropped={} {}",
            rec.elapsed().as_secs_f64(),
            stats.frames(),
            stats.dropped(),
            chains.join(" ")
        );
    }
    let t = Instant::now();
    let recording = rec.stop()?;
    println!(
        "stop took {:?}, duration {:.3}",
        t.elapsed(),
        recording.duration
    );
    drop(capture);
    for c in &recording.chains {
        println!("{}: {} parts", c.name, c.parts.len());
    }
    let cancel = AtomicBool::new(false);
    let report = recording.finalize(
        &ff,
        &mut |stage, p| eprint!("\r{stage:?} {:3.0}%   ", p * 100.0),
        &cancel,
    )?;
    eprintln!();
    println!(
        "{} ({} bytes), empty chains: {:?}, warnings: {:?}",
        output.display(),
        report.pack.file_size,
        report.empty_chains,
        report.pack.warnings
    );
    Ok(())
}
