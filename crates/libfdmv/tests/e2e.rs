//! ffmpeg で素材を作り、pack → デコード → シークまで通しで検査する。
//! ffmpeg が無い環境ではスキップする。

#![cfg(feature = "decode")]

use std::path::Path;
use std::process::Command;

use libfdmv::decode::{AudioRenderer, ChainSelection, VideoStream};
use libfdmv::ffmpeg::{Ffmpeg, VideoEncodeOptions};
use libfdmv::pack::{AppendMode, ChainSpec, PackOptions, SegmentSource, add_chain, pack};
use libfdmv::{ChainRole, FdmvReader};

fn ffmpeg() -> Option<Ffmpeg> {
    match Ffmpeg::locate() {
        Ok(f) => Some(f),
        Err(e) => {
            eprintln!("skipping: {e}");
            None
        }
    }
}

fn lavfi(out: &Path, args: &[&str]) {
    let st = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi"])
        .args(args)
        .arg(out)
        .status()
        .unwrap();
    assert!(st.success());
}

fn render_all(
    r: &mut FdmvReader<impl std::io::Read + std::io::Seek>,
    ren: &mut AudioRenderer,
) -> Vec<f32> {
    let mut out = Vec::new();
    let mut buf = vec![0f32; 1000 * ren.channels()];
    loop {
        let n = ren.render(r, &mut buf).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n * ren.channels()]);
    }
    out
}

#[test]
fn pack_decode_and_seek() {
    let Some(ff) = ffmpeg() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let video = tmp.path().join("in.mkv");
    lavfi(
        &video,
        &[
            "-i",
            "testsrc2=size=160x90:rate=25",
            "-f",
            "lavfi",
            "-i",
            "anoisesrc=c=pink:r=48000:a=0.2:seed=1",
            "-t",
            "4",
            "-c:v",
            "ffv1",
            "-c:a",
            "pcm_s16le",
            "-shortest",
        ],
    );
    let voice = tmp.path().join("voice.wav");
    lavfi(
        &voice,
        &[
            "-i",
            "anoisesrc=c=white:r=48000:a=0.1:seed=2,lowpass=f=3000",
            "-t",
            "0.7",
            "-ac",
            "1",
        ],
    );

    let probe = ff.probe(&video).unwrap();
    let mut main = ChainSpec::new("main", ChainRole::Default);
    main.segments
        .push(SegmentSource::from_video_audio(&video, &probe));
    let mut sub = ChainSpec::new("voice", ChainRole::Sub);
    sub.segments.push(SegmentSource::new(&voice, 1.0));
    sub.segments.push(SegmentSource::new(&voice, 2.5));
    sub.gain_db = -6.0;
    let opts = PackOptions {
        video: VideoEncodeOptions {
            preset: Some(12),
            crf: 40,
            ..Default::default()
        },
        ..Default::default()
    };
    let out = tmp.path().join("out.fdmv");
    pack(&ff, &video, &[main, sub], &opts, &out).unwrap();

    let mut r = FdmvReader::open(&out).unwrap();
    let dir = r.directory().clone();
    assert_eq!(dir.duration_seconds(), 4.0);
    let voice_id = dir.chain_by_name("voice").unwrap().id;
    let segs = dir
        .stream(voice_id)
        .unwrap()
        .chain()
        .unwrap()
        .segments
        .clone();
    assert_eq!(segs.len(), 2);
    assert_eq!((segs[0].start, segs[0].length), (48_000, 33_600));
    assert_eq!(segs[1].start, 120_000);

    // サブチェーンだけを描画すると、セグメント外は完全な無音
    let sel = [ChainSelection {
        stream_id: voice_id,
        gain: 1.0,
    }];
    let mut ren = AudioRenderer::new(&dir, &sel, Some(1)).unwrap();
    let pcm = render_all(&mut r, &mut ren);
    assert_eq!(pcm.len(), 192_000);
    let nonzero: Vec<usize> = pcm
        .iter()
        .enumerate()
        .filter(|(_, v)| **v != 0.0)
        .map(|(i, _)| i)
        .collect();
    assert!(*nonzero.first().unwrap() >= 48_000 && *nonzero.last().unwrap() < 120_000 + 33_600);
    assert!(pcm[81_600..120_000].iter().all(|v| *v == 0.0));

    // シークしてから描画した結果は、先頭から描画した結果とほぼ一致する
    let both = [
        ChainSelection::with_chain_gain(&dir, dir.default_chain().unwrap().id),
        ChainSelection::with_chain_gain(&dir, voice_id),
    ];
    let mut ren = AudioRenderer::new(&dir, &both, Some(2)).unwrap();
    let full = render_all(&mut r, &mut ren);
    for t in [
        0i64, 1_234, 47_999, 48_000, 60_000, 100_000, 119_000, 191_000,
    ] {
        ren.seek(&mut r, t).unwrap();
        let mut buf = vec![0f32; 2 * 960];
        let n = ren.render(&mut r, &mut buf).unwrap();
        let reference = &full[t as usize * 2..(t as usize + n) * 2];
        let err = buf[..n * 2]
            .iter()
            .zip(reference)
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        assert!(err < 0.02, "seek to {t}: max error {err}");
    }

    // 映像のシークは指定時刻のフレームを返す
    let vtb = dir.video().unwrap().timebase;
    let mut vs = VideoStream::new(&dir, 1).unwrap();
    for secs in [0.0, 1.0, 2.36, 3.96] {
        let pts = vtb.from_seconds(secs);
        vs.seek(&mut r, pts).unwrap();
        let f = vs.next_frame(&mut r).unwrap().unwrap();
        assert_eq!(f.pts(), pts);
        assert_eq!((f.width(), f.height()), (160, 90));
    }

    // 追記したチェーンもデコードできる
    let mut extra = ChainSpec::new("extra", ChainRole::Sub);
    extra.segments.push(SegmentSource::new(&voice, 3.0));
    let report = add_chain(&ff, &out, &extra, &Default::default(), &AppendMode::InPlace).unwrap();
    assert!(report.warnings.is_empty());
    let mut r = FdmvReader::open(&out).unwrap();
    let dir = r.directory().clone();
    let id = dir.chain_by_name("extra").unwrap().id;
    let mut ren = AudioRenderer::new(
        &dir,
        &[ChainSelection {
            stream_id: id,
            gain: 1.0,
        }],
        None,
    )
    .unwrap();
    let pcm = render_all(&mut r, &mut ren);
    let first = pcm.iter().position(|v| *v != 0.0).unwrap() / ren.channels();
    assert_eq!(first, 144_000);
}
