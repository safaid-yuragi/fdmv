//! 素材の取り込み → 編集 → プレビュー → 書き出しを通しで検査する。ffmpeg が無ければスキップ。

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicBool;

use fdmv_edit::preview::{TimelineAudio, TimelineVideo};
use fdmv_edit::proxy::probe_source;
use fdmv_edit::{Project, ProxyStore};
use fdmv_gui::{FrameSource, PcmSource};
use libfdmv::FdmvReader;
use libfdmv::decode::{AudioRenderer, ChainSelection, VideoStream};
use libfdmv::ffmpeg::Ffmpeg;

fn lavfi(out: &Path, args: &[&str]) {
    let st = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi"])
        .args(args)
        .arg(out)
        .status()
        .unwrap();
    assert!(st.success());
}

/// ゼロ交差から周波数を推定する（左チャンネル）。
fn freq(pcm: &[f32], from: f64, to: f64) -> f64 {
    let (a, b) = ((from * 48000.0) as usize, (to * 48000.0) as usize);
    let left: Vec<f32> = pcm[a * 2..b * 2].iter().step_by(2).copied().collect();
    let zc = left
        .windows(2)
        .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
        .count();
    zc as f64 / 2.0 / (to - from)
}

fn rms(pcm: &[f32], from: f64, to: f64) -> f32 {
    let (a, b) = ((from * 48000.0) as usize * 2, (to * 48000.0) as usize * 2);
    (pcm[a..b].iter().map(|v| v * v).sum::<f32>() / (b - a) as f32).sqrt()
}

#[test]
fn edit_preview_and_export() {
    let Ok(ff) = Ffmpeg::locate() else {
        eprintln!("skipping: ffmpeg not found");
        return;
    };
    if ff.pick_av1_encoder(None).is_err() {
        eprintln!("skipping: no AV1 encoder");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a.mkv");
    let b = tmp.path().join("b.mkv");
    let voice = tmp.path().join("voice.wav");
    lavfi(
        &a,
        &[
            "-i",
            "testsrc2=size=160x90:rate=25",
            "-f",
            "lavfi",
            "-i",
            "sine=f=440:r=44100",
            "-t",
            "4",
            "-c:v",
            "ffv1",
            "-c:a",
            "pcm_s16le",
        ],
    );
    lavfi(
        &b,
        &[
            "-i",
            "testsrc=size=320x240:rate=25",
            "-f",
            "lavfi",
            "-i",
            "sine=f=660:r=48000",
            "-t",
            "3",
            "-c:v",
            "ffv1",
            "-c:a",
            "pcm_s16le",
        ],
    );
    lavfi(
        &voice,
        &["-i", "sine=f=1000:r=48000", "-t", "1", "-ac", "1"],
    );

    let mut p = Project::new();
    let sa = p.add_source(probe_source(&ff, &a).unwrap());
    let sb = p.add_source(probe_source(&ff, &b).unwrap());
    let sv = p.add_source(probe_source(&ff, &voice).unwrap());
    assert_eq!(p.source(sa).unwrap().duration, 4.0);
    p.insert_video(sa, None).unwrap();
    p.insert_video(sb, None).unwrap();
    // A の [1, 2) を削除 → A[0,1) + A[2,4) + B[0,3) = 6 秒
    p.delete_range(1.0, 2.0);
    assert_eq!(p.duration(), 6.0);
    let sub = p.add_chain("解説");
    p.add_audio(sub, sv, 3.5).unwrap();
    p.chain_mut(sub).unwrap().gain_db = -3.0;
    p.chain_mut(sub).unwrap().language = "ja".into();
    assert_eq!(p.format(), (160, 90, 25, 1));

    let proxies = ProxyStore::new(tmp.path().join("proxies")).unwrap();
    let cancel = AtomicBool::new(false);
    for s in p.sources.clone() {
        proxies.build(&ff, &s, &mut |_| {}, &cancel).unwrap();
        assert!(proxies.is_ready(&s));
    }

    // プレビュー（映像）: 2.5 秒は A の 3.5 秒、4.0 秒は B の 1.0 秒
    let mut tv = TimelineVideo::new(proxies.clone(), &p);
    tv.seek(2.5).unwrap();
    let f = tv.next_frame().unwrap().unwrap();
    assert!((f.pts - 2.48).abs() < 1e-6, "pts {}", f.pts);
    tv.seek(4.0).unwrap();
    let f = tv.next_frame().unwrap().unwrap();
    assert!((f.pts - 4.0).abs() < 0.02, "pts {}", f.pts);
    let mut frames = 0;
    tv.seek(0.0).unwrap();
    while tv.next_frame().unwrap().is_some() {
        frames += 1;
    }
    assert_eq!(frames, 150);

    // プレビュー（音声）
    let mut ta = TimelineAudio::new(proxies.clone(), &p, &HashSet::new());
    let mut pcm = vec![0f32; 6 * 48000 * 2];
    assert_eq!(ta.render(&mut pcm).unwrap(), 6 * 48000);
    assert!((freq(&pcm, 0.2, 0.9) - 440.0).abs() < 5.0);
    assert!((freq(&pcm, 4.6, 5.9) - 660.0).abs() < 5.0);

    // 書き出し
    let out = tmp.path().join("out.fdmv");
    let mut last = 0.0;
    let report = fdmv_edit::export::export(
        &ff,
        &p,
        &proxies,
        &out,
        &mut |pr| {
            assert!(pr.fraction >= last - 1e-9, "progress went backwards");
            last = pr.fraction;
        },
        &cancel,
    )
    .unwrap();
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!(last, 1.0);

    let mut r = FdmvReader::open(&out).unwrap();
    let dir = r.directory().clone();
    assert_eq!(dir.duration_seconds(), 6.0);
    let v = dir.video().unwrap().video().unwrap();
    assert_eq!((v.width, v.height), (160, 90));
    let main = dir.default_chain().unwrap();
    assert_eq!(main.chain().unwrap().segments.len(), 1);
    let s = dir.chain_by_name("解説").unwrap();
    assert_eq!(s.meta("language"), Some("ja"));
    assert_eq!(s.chain().unwrap().gain_db, -3.0);
    let seg = s.chain().unwrap().segments[0];
    assert_eq!((seg.start, seg.length), (168_000, 48_000));

    // 映像は 150 フレーム
    let mut vs = VideoStream::new(&dir, 0).unwrap();
    let mut n = 0;
    while vs.next_frame(&mut r).unwrap().is_some() {
        n += 1;
    }
    assert_eq!(n, 150);

    // デフォルトチェーン: 前半 440 Hz、後半 660 Hz（映像の音声がカットに追従している）
    let mut ren = AudioRenderer::new(
        &dir,
        &[ChainSelection {
            stream_id: main.id,
            gain: 1.0,
        }],
        Some(2),
    )
    .unwrap();
    let mut pcm = vec![0f32; 6 * 48000 * 2];
    let n = ren.render(&mut r, &mut pcm).unwrap();
    assert_eq!(n, 6 * 48000);
    assert!((freq(&pcm, 0.2, 0.9) - 440.0).abs() < 5.0);
    assert!((freq(&pcm, 1.1, 2.9) - 440.0).abs() < 5.0);
    assert!((freq(&pcm, 3.1, 5.9) - 660.0).abs() < 5.0);

    // 解説チェーン: 3.5–4.5 秒だけ 1000 Hz
    let mut ren = AudioRenderer::new(
        &dir,
        &[ChainSelection {
            stream_id: s.id,
            gain: 1.0,
        }],
        Some(2),
    )
    .unwrap();
    let mut pcm = vec![0f32; 6 * 48000 * 2];
    ren.render(&mut r, &mut pcm).unwrap();
    assert_eq!(rms(&pcm, 0.0, 3.49), 0.0);
    assert!((freq(&pcm, 3.6, 4.4) - 1000.0).abs() < 5.0);
    assert_eq!(rms(&pcm, 4.51, 6.0), 0.0);
}
