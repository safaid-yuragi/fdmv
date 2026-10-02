//! 映像のエンコード → 音声パートのミックス → FDMV までを、OS の音声・画面を使わずに通しで検査する。
//! ffmpeg（と AV1 エンコーダ）が無い環境ではスキップする。

use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use fdmv_capture::chain::Chain;
use fdmv_capture::codecs::{self, EncoderKind, LiveEncoder};
use fdmv_capture::encoder::VideoEncoder;
use fdmv_capture::finalize::{ChainParts, FinalizeJob, finalize};
use fdmv_capture::video::{self, VideoTarget};
use libfdmv::ffmpeg::{AudioEncodeOptions, Ffmpeg};
use libfdmv::{ChainRole, FdmvReader};

fn ffmpeg() -> Option<Ffmpeg> {
    match Ffmpeg::locate().and_then(|ff| ff.pick_av1_encoder(None).map(|_| ff)) {
        Ok(ff) => Some(ff),
        Err(e) => {
            eprintln!("skipping: {e}");
            None
        }
    }
}

#[test]
fn records_with_software_av1() {
    let Some(ff) = ffmpeg() else { return };
    let enc = codecs::available(&ff)
        .into_iter()
        .find(|e| e.kind == EncoderKind::SoftwareAv1)
        .unwrap();
    record(&ff, &enc);
}

/// GPU のエンコーダがあれば、それでも録って AV1 に変換できること。
#[test]
fn records_with_hardware_encoder() {
    let Some(ff) = ffmpeg() else { return };
    let Some(enc) = codecs::available(&ff)
        .into_iter()
        .find(|e| e.kind != EncoderKind::SoftwareAv1)
    else {
        eprintln!("skipping: no hardware encoder");
        return;
    };
    record(&ff, &enc);
}

fn record(ff: &Ffmpeg, live: &LiveEncoder) {
    let dir = tempfile::tempdir().unwrap();
    let opts = fdmv_capture::RecordOptions::default();

    let capture = video::start(ff, &VideoTarget::TestPattern, 30).unwrap();
    let epoch = Instant::now() + Duration::from_millis(200);
    let video = dir.path().join(if live.is_av1() {
        "video.ivf"
    } else {
        "video.mkv"
    });
    let enc = VideoEncoder::start(
        ff,
        capture.slot(),
        (1280, 720),
        30,
        Some(360),
        live,
        opts.quality,
        live.needs_conversion(),
        &video,
        epoch,
    )
    .unwrap();

    // メイン: 0.5 秒から 1 秒間の 440 Hz、サブ: 録画開始前から 0.3 秒まで
    let main = Chain::new(
        "main",
        ChainRole::Default,
        Vec::new(),
        dir.path().join("c0"),
        epoch,
    )
    .unwrap();
    let sub = Chain::new(
        "sub",
        ChainRole::Sub,
        Vec::new(),
        dir.path().join("c1"),
        epoch,
    )
    .unwrap();
    let silent = Chain::new(
        "silent",
        ChainRole::Sub,
        Vec::new(),
        dir.path().join("c2"),
        epoch,
    )
    .unwrap();
    let tone = |n: usize| -> Vec<f32> {
        (0..n)
            .flat_map(|i| {
                let s = (i as f32 * 440.0 * std::f32::consts::TAU / 48000.0).sin() * 0.3;
                [s, s]
            })
            .collect()
    };
    let mut t = main.take(48000, 2);
    t.push_at(&tone(48000), epoch + Duration::from_millis(1500));
    drop(t);
    let mut t = sub.take(48000, 2);
    t.push_at(&tone(24000), epoch + Duration::from_millis(300));
    drop(t);
    let mut t = silent.take(48000, 2);
    t.push_at(&vec![0.0; 96000], epoch + Duration::from_millis(1000));
    drop(t);

    // 2 秒の時点で止まるよう先に指示しておく。
    enc.stop_at(Duration::from_secs(2));
    let duration = enc.finish(Duration::from_secs(2)).unwrap();
    assert_eq!(duration, 2.0);
    drop(capture);

    let chains: Vec<ChainParts> = [&main, &sub, &silent]
        .iter()
        .map(|c| ChainParts {
            name: c.name.clone(),
            role: c.role,
            meta: Vec::new(),
            parts: c.parts(),
        })
        .collect();
    let out = dir.path().join("out.fdmv");
    let reencode = live
        .needs_conversion()
        .then(|| opts.final_video_options(&ff.pick_av1_encoder(None).unwrap(), 30));
    let job = FinalizeJob {
        video: &video,
        encoder: &live.name,
        duration: 2.0,
        chains: &chains,
        reencode: reencode.as_ref(),
        audio: &AudioEncodeOptions::default(),
        file_meta: vec![("title".into(), "テスト".into())],
        workdir: dir.path(),
        output: &out,
    };
    let report = finalize(ff, &job, &mut |_, _| {}, &AtomicBool::new(false)).unwrap();
    assert_eq!(report.empty_chains, ["silent"]);

    let r = FdmvReader::open(&out).unwrap();
    let d = r.directory();
    assert_eq!(d.meta("title"), Some("テスト"));
    let v = d.video().unwrap().video().unwrap();
    assert_eq!((v.width, v.height), (640, 360));
    assert!(
        (d.duration_seconds() - 2.0).abs() < 0.05,
        "{}",
        d.duration_seconds()
    );
    let main = d.default_chain().unwrap();
    assert_eq!(main.name, "main");
    let seg = &main.chain().unwrap().segments;
    assert_eq!(seg.len(), 1);
    let tb = main.timebase.den as f64 / main.timebase.num as f64;
    assert!((seg[0].start as f64 / tb - 0.5).abs() < 0.01, "{:?}", seg);
    let sub = d.chain_by_name("sub").unwrap();
    let seg = &sub.chain().unwrap().segments;
    assert_eq!(seg[0].start, 0);
    assert!(d.chain_by_name("silent").is_none());
}
