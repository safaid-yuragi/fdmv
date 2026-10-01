//! プロジェクトを .fdmv に書き出す。
//!
//! 1. 映像: 元素材から各クリップを切り出し（`-ss` / `-t`）、解像度・フレームレートをそろえて連結し、AV1 にエンコード
//! 2. チェーン: プレビューと同じミックス処理で、音のある区間ごとに WAV を作る（区間外は無音のセグメントなし）
//! 3. libfdmv の pack で格納（Opus へのエンコードを含む）

use std::ffi::OsString;
use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, bail};
use libfdmv::ffmpeg::{AudioEncodeOptions, Ffmpeg, VideoEncodeOptions};
use libfdmv::model::ChainRole as FileRole;
use libfdmv::pack::{ChainSpec, PackOptions, PackReport, SegmentSource, pack_ivf};
use libfdmv::wav::{SampleFormat, WavWriter};

use crate::preview::{Mixer, RATE, chain_items, merge_ranges};
use crate::project::{ChainRole, Project};
use crate::proxy::ProxyStore;

/// これ未満の隙間はセグメントを分けずに無音でつなぐ（48 kHz サンプル、0.5 秒）。
const SEGMENT_GAP: i64 = 24_000;

#[derive(Clone, Debug)]
pub struct Progress {
    pub stage: String,
    /// 全体の進捗（0.0–1.0）。
    pub fraction: f64,
}

pub fn video_options(project: &Project) -> VideoEncodeOptions {
    let s = &project.settings;
    VideoEncodeOptions {
        encoder: None,
        crf: s.crf,
        preset: s.preset,
        pix_fmt: if s.ten_bit { "yuv420p10le" } else { "yuv420p" }.into(),
        extra_args: Vec::new(),
    }
}

/// 映像トラックを書き出す ffmpeg の入力引数と filter_complex。
pub fn video_graph(project: &Project) -> (Vec<OsString>, String) {
    let (w, h, fn_, fd) = project.format();
    let pix = if project.settings.ten_bit {
        "yuv420p10le"
    } else {
        "yuv420p"
    };
    let mut inputs: Vec<OsString> = Vec::new();
    let mut filter = String::new();
    let mut labels = String::new();
    for (i, c) in project.video.iter().enumerate() {
        let s = project
            .source(c.source)
            .expect("checked by check_exportable");
        inputs.extend([
            "-ss".into(),
            format!("{:.6}", s.video_ss_offset + c.src_in).into(),
            "-t".into(),
            format!("{:.6}", c.duration() + 0.5).into(),
            "-i".into(),
            s.path.clone().into(),
        ]);
        filter.push_str(&format!(
            "[{i}:v:0]trim=duration={d:.6},setpts=PTS-STARTPTS,fps={fn_}/{fd},\
             scale={w}:{h}:force_original_aspect_ratio=decrease:flags=lanczos,\
             pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=black,setsar=1,format={pix}[v{i}];",
            d = c.duration()
        ));
        labels.push_str(&format!("[v{i}]"));
    }
    filter.push_str(&format!(
        "{labels}concat=n={}:v=1:a=0[vout]",
        project.video.len()
    ));
    (inputs, filter)
}

pub fn export(
    ff: &Ffmpeg,
    project: &Project,
    proxies: &ProxyStore,
    output: &Path,
    progress: &mut dyn FnMut(Progress),
    cancel: &AtomicBool,
) -> Result<PackReport> {
    project.check_exportable()?;
    let mut report = |stage: &str, fraction: f64| {
        progress(Progress {
            stage: stage.to_owned(),
            fraction: fraction.clamp(0.0, 1.0),
        })
    };
    let check_cancel = || -> Result<()> {
        if cancel.load(Ordering::Relaxed) {
            bail!("キャンセルしました");
        }
        Ok(())
    };

    // 音声プロキシ（書き出しのミックスにも使う）が揃っていなければ作る。
    let used: Vec<_> = project
        .sources
        .iter()
        .filter(|s| {
            s.has_audio
                && (project.video.iter().any(|c| c.source == s.id)
                    || project
                        .chains
                        .iter()
                        .any(|ch| ch.clips.iter().any(|c| c.source == s.id)))
        })
        .collect();
    for (i, s) in used.iter().enumerate() {
        if !proxies.is_ready(s) {
            let base = i as f64 / used.len() as f64 * 0.1;
            proxies.build(
                ff,
                s,
                &mut |p| report("プロキシを作成中", base + p * 0.1 / used.len() as f64),
                cancel,
            )?;
        }
    }
    check_cancel()?;

    let tmp = tempfile::tempdir()?;
    let total = project.duration();

    // 1. 映像
    let (inputs, filter) = video_graph(project);
    let ivf = tmp.path().join("video.ivf");
    let encoder = ff.encode_video_complex(
        &inputs,
        &filter,
        "[vout]",
        &ivf,
        &video_options(project),
        total,
        &mut |p| report("映像をエンコード中", 0.1 + p * 0.7),
        Some(cancel),
    )?;
    check_cancel()?;

    // 2. チェーン
    let end = (total * RATE).round() as i64;
    let mut mixer = Mixer::new(proxies.clone(), project);
    let mut specs = Vec::new();
    for (ci, ch) in project.chains.iter().enumerate() {
        report(
            &format!("チェーン「{}」を作成中", ch.name),
            0.8 + 0.05 * ci as f64 / project.chains.len() as f64,
        );
        // チェーンのゲインは .fdmv の gain_db として記録し、音声には焼き込まない。
        let items = chain_items(project, ch, false);
        let ranges = merge_ranges(&items, SEGMENT_GAP, end);
        if ranges.is_empty() {
            continue;
        }
        let mut spec = ChainSpec::new(
            ch.name.clone(),
            if ch.role == ChainRole::Default {
                FileRole::Default
            } else {
                FileRole::Sub
            },
        );
        spec.gain_db = ch.gain_db;
        if !ch.language.trim().is_empty() {
            spec.meta
                .push(("language".into(), ch.language.trim().to_owned()));
        }
        if !ch.description.trim().is_empty() {
            spec.meta
                .push(("description".into(), ch.description.trim().to_owned()));
        }
        for (si, (a, b)) in ranges.into_iter().enumerate() {
            let path = tmp.path().join(format!("chain{ci}_seg{si}.wav"));
            let mut w = WavWriter::new(
                BufWriter::new(File::create(&path)?),
                48_000,
                2,
                SampleFormat::F32,
                (b - a) as u64,
            )?;
            let mut buf = vec![0f32; 4800 * 2];
            let mut pos = a;
            while pos < b {
                let n = (b - pos).min(4800) as usize;
                let out = &mut buf[..n * 2];
                out.fill(0.0);
                mixer.mix(&items, pos, out)?;
                w.write(out)?;
                pos += n as i64;
            }
            w.finish()?;
            spec.segments
                .push(SegmentSource::new(path, a as f64 / RATE));
        }
        specs.push(spec);
        check_cancel()?;
    }

    // 3. 格納
    report("ファイルに格納中", 0.85);
    let mut meta = Vec::new();
    if !project.settings.title.trim().is_empty() {
        meta.push(("title".to_owned(), project.settings.title.trim().to_owned()));
    }
    let opts = PackOptions {
        video: video_options(project),
        audio: AudioEncodeOptions {
            bitrate: project.settings.audio_bitrate.clone(),
            channels: 2,
        },
        file_meta: meta,
    };
    let result = pack_ivf(ff, &ivf, &encoder, &specs, &opts, output)?;
    report("完了", 1.0);
    Ok(result)
}
