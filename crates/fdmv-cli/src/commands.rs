use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, Write};
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result, anyhow, bail};
use libfdmv::decode::{AudioRenderer, ChainDecoder, ChainSelection, VideoStream};
use libfdmv::ffmpeg::{AudioEncodeOptions, VideoEncodeOptions};
use libfdmv::format::{BLOCK_CLUSTER, BLOCK_DIRECTORY, BLOCK_FOOTER, OPUS_SAMPLE_RATE, Rational};
use libfdmv::ivf::IvfWriter;
use libfdmv::model::meta_set;
use libfdmv::pack::{AppendMode, ChainSpec, PackOptions, SegmentSource};
use libfdmv::time::{format_time, parse_time};
use libfdmv::wav::{SampleFormat, WavWriter};
use libfdmv::{ChainRole, Directory, FdmvReader, StreamEntry};
use serde_json::json;

use crate::util::{
    self, Progress, chain_id, extension, human_size, parse_meta, parse_source, split_kv,
};
use crate::{
    AddChainArgs, AudioOpts, ExportArgs, ExtractArgs, ExtractVideoArgs, InfoArgs, PackArgs,
    SnapshotArgs, VerifyArgs, VideoOpts, WavFormat,
};

type Reader = FdmvReader<BufReader<File>>;

fn open(path: &Path) -> Result<Reader> {
    let r = FdmvReader::open(path).with_context(|| format!("opening {}", path.display()))?;
    if r.was_recovered() {
        eprintln!(
            "warning: {} has an incomplete append at the end; opened the last complete version",
            path.display()
        );
    }
    Ok(r)
}

fn audio_options(a: &AudioOpts) -> AudioEncodeOptions {
    AudioEncodeOptions {
        bitrate: a.audio_bitrate.clone(),
        channels: a.channels,
    }
}

fn video_options(v: &VideoOpts) -> VideoEncodeOptions {
    VideoEncodeOptions {
        encoder: v.encoder.clone(),
        crf: v.crf,
        preset: v.preset,
        pix_fmt: if v.ten_bit { "yuv420p10le" } else { "yuv420p" }.into(),
        extra_args: v.ffmpeg_arg.clone(),
    }
}

// ---------------------------------------------------------------------------
// pack / add-chain

pub fn pack(a: PackArgs) -> Result<ExitCode> {
    let ff = util::ffmpeg(a.quiet)?;
    let probe = ff.probe(&a.input)?;
    if !probe.has_video {
        bail!("{} has no video stream", a.input.display());
    }

    let mut chains: Vec<ChainSpec> = Vec::new();
    if !a.no_default_audio {
        let seg = match &a.default_audio {
            Some(spec) => Some(parse_source(spec)?),
            None if probe.audio_streams > 0 => {
                Some(SegmentSource::from_video_audio(&a.input, &probe))
            }
            None => {
                eprintln!("note: the input has no audio; the file will have no default chain");
                None
            }
        };
        if let Some(seg) = seg {
            let mut c = ChainSpec::new(a.default_name.clone(), ChainRole::Default);
            c.segments.push(seg);
            chains.push(c);
        }
    }
    for item in &a.chain {
        let (name, src) = split_kv(item, '=', "--chain")?;
        let seg = parse_source(src)?;
        match chains.iter_mut().find(|c| c.name == name) {
            Some(c) => c.segments.push(seg),
            None => {
                let mut c = ChainSpec::new(name, ChainRole::Sub);
                c.segments.push(seg);
                chains.push(c);
            }
        }
    }
    let find = |chains: &mut Vec<ChainSpec>, name: &str, opt: &str| -> Result<usize> {
        chains
            .iter()
            .position(|c| c.name == name)
            .ok_or_else(|| anyhow!("{opt}: no chain named {name:?}"))
    };
    for g in &a.gain {
        let (name, db) = g
            .rsplit_once('=')
            .ok_or_else(|| anyhow!("--gain {g:?} must be NAME=DB"))?;
        let db: f32 = db
            .trim()
            .parse()
            .map_err(|_| anyhow!("--gain {g:?}: invalid number"))?;
        let i = find(&mut chains, name, "--gain")?;
        chains[i].gain_db = db;
    }
    for m in &a.chain_meta {
        let (left, value) = split_kv(m, '=', "--chain-meta")?;
        let (name, key) = left
            .rsplit_once(':')
            .ok_or_else(|| anyhow!("--chain-meta {m:?} must be NAME:KEY=VALUE"))?;
        let kv = parse_meta(&[format!("{key}={value}")])?;
        let i = find(&mut chains, name, "--chain-meta")?;
        meta_set(&mut chains[i].meta, &kv[0].0, &kv[0].1);
    }
    let mut file_meta = parse_meta(&a.meta)?;
    if let Some(t) = &a.title {
        meta_set(&mut file_meta, "title", t);
    }

    let opts = PackOptions {
        video: video_options(&a.video),
        audio: audio_options(&a.audio),
        file_meta,
    };
    util::ensure_parent(&a.output)?;
    eprintln!("encoding {} ...", a.input.display());
    let report = libfdmv::pack::pack(&ff, &a.input, &chains, &opts, &a.output)?;
    for w in &report.warnings {
        eprintln!("warning: {w}");
    }
    println!(
        "wrote {} ({}, video encoder {})",
        a.output.display(),
        human_size(report.file_size),
        report.encoder
    );
    print_summary(&open(&a.output)?);
    Ok(ExitCode::SUCCESS)
}

pub fn add_chain(a: AddChainArgs) -> Result<ExitCode> {
    let ff = util::ffmpeg(a.quiet)?;
    let mut spec = ChainSpec::new(
        a.name.clone(),
        if a.default {
            ChainRole::Default
        } else {
            ChainRole::Sub
        },
    );
    for s in &a.sources {
        spec.segments.push(parse_source(s)?);
    }
    spec.gain_db = a.gain;
    spec.meta = parse_meta(&a.meta)?;
    let mode = match &a.output {
        Some(out) => {
            util::ensure_parent(out)?;
            AppendMode::CopyTo(out.clone())
        }
        None => AppendMode::InPlace,
    };
    let report = libfdmv::pack::add_chain(&ff, &a.file, &spec, &audio_options(&a.audio), &mode)?;
    for w in &report.warnings {
        eprintln!("warning: {w}");
    }
    let target = a.output.as_deref().unwrap_or(&a.file);
    println!(
        "added chain {:?} → {} ({})",
        a.name,
        target.display(),
        human_size(report.file_size)
    );
    print_summary(&open(target)?);
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------------------
// info

fn role_label(e: &StreamEntry) -> &'static str {
    match e.chain().map(|c| c.role) {
        Some(ChainRole::Default) => "default",
        _ => "sub",
    }
}

fn channels_label(n: u8) -> String {
    match n {
        1 => "mono".into(),
        2 => "stereo".into(),
        n => format!("{n} ch"),
    }
}

fn print_summary(r: &Reader) {
    let dir = r.directory();
    if let Some(v) = dir.video()
        && let Some(p) = v.video()
    {
        println!(
            "  video  : AV1 {}x{}, {:.3} fps, {}",
            p.width,
            p.height,
            p.frame_rate.as_f64(),
            format_time(v.duration_seconds())
        );
    }
    for c in dir.chains() {
        let p = c.chain().unwrap();
        println!(
            "  chain  : #{} {:?} [{}] {}, {} segment(s)",
            c.id,
            c.name,
            role_label(c),
            channels_label(p.channels),
            p.segments.len()
        );
    }
}

pub fn info(a: InfoArgs) -> Result<ExitCode> {
    let r = open(&a.file)?;
    let dir = r.directory();
    if a.json {
        println!("{}", serde_json::to_string_pretty(&info_json(&r, a.index))?);
        return Ok(ExitCode::SUCCESS);
    }
    let (major, minor) = r.version();
    println!(
        "file      : {} ({}, FDMV {major}.{minor})",
        a.file.display(),
        human_size(r.file_len())
    );
    println!("duration  : {}", format_time(dir.duration_seconds()));
    for (k, v) in &dir.meta {
        println!("meta      : {k} = {v}");
    }
    for s in &dir.streams {
        println!();
        if let Some(p) = s.video() {
            println!("#{} video  AV1 {}x{}", s.id, p.width, p.height);
            println!(
                "    frame rate : {} ({:.3} fps)",
                p.frame_rate,
                p.frame_rate.as_f64()
            );
            println!("    timebase   : {}", s.timebase);
            println!(
                "    keyframes  : {}",
                dir.index_of(s.id)
                    .iter()
                    .filter(|e| e.is_keyframe())
                    .count()
            );
        } else if let Some(p) = s.chain() {
            println!("#{} chain  {:?} [{}]", s.id, s.name, role_label(s));
            println!(
                "    codec      : Opus 48 kHz {}",
                channels_label(p.channels)
            );
            println!("    gain       : {:+.1} dB", p.gain_db);
            for (k, v) in &s.meta {
                println!("    {k:<10} : {v}");
            }
            println!("    segments   : {}", p.segments.len());
            for seg in &p.segments {
                println!(
                    "      {} – {}  ({:.3}s)",
                    format_time(Rational::OPUS.to_seconds(seg.start)),
                    format_time(Rational::OPUS.to_seconds(seg.end())),
                    Rational::OPUS.to_seconds(seg.length)
                );
            }
        } else {
            println!(
                "#{} unknown stream kind {} (codec {})",
                s.id, s.kind, s.codec
            );
        }
    }
    if a.index {
        println!();
        println!("index ({} entries):", dir.index.len());
        for e in &dir.index {
            let mut f = String::new();
            if e.is_keyframe() {
                f.push('K');
            }
            if e.is_segment_start() {
                f.push('S');
            }
            let tb = dir
                .stream(e.stream_id)
                .map(|s| s.timebase)
                .unwrap_or(Rational::new(1, 1));
            println!(
                "  stream {:>3}  pts {:>12} ({})  cluster @{:<10} #{:<5} {f}",
                e.stream_id,
                e.pts,
                format_time(tb.to_seconds(e.pts)),
                e.cluster_offset,
                e.packet_index
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn info_json(r: &Reader, with_index: bool) -> serde_json::Value {
    let dir = r.directory();
    let meta = |m: &Vec<(String, String)>| -> serde_json::Value {
        m.iter()
            .map(|(k, v)| (k.clone(), json!(v)))
            .collect::<serde_json::Map<_, _>>()
            .into()
    };
    let streams: Vec<_> = dir
        .streams
        .iter()
        .map(|s| {
            let mut v = json!({
                "id": s.id,
                "kind": s.kind,
                "codec": s.codec,
                "timebase": [s.timebase.num, s.timebase.den],
                "duration": s.duration,
                "duration_seconds": s.duration_seconds(),
                "name": s.name,
                "meta": meta(&s.meta),
            });
            if let Some(p) = s.video() {
                v["type"] = json!("video");
                v["width"] = json!(p.width);
                v["height"] = json!(p.height);
                v["frame_rate"] = json!([p.frame_rate.num, p.frame_rate.den]);
            } else if let Some(p) = s.chain() {
                v["type"] = json!("chain");
                v["role"] = json!(role_label(s));
                v["channels"] = json!(p.channels);
                v["gain_db"] = json!(p.gain_db);
                v["segments"] = p
                    .segments
                    .iter()
                    .map(|g| {
                        json!({
                            "start": g.start,
                            "length": g.length,
                            "pre_skip": g.pre_skip,
                            "start_seconds": Rational::OPUS.to_seconds(g.start),
                            "end_seconds": Rational::OPUS.to_seconds(g.end()),
                        })
                    })
                    .collect();
            }
            v
        })
        .collect();
    let mut out = json!({
        "version": [r.version().0, r.version().1],
        "file_size": r.file_len(),
        "duration_seconds": dir.duration_seconds(),
        "meta": meta(&dir.meta),
        "streams": streams,
    });
    if with_index {
        out["index"] = dir
            .index
            .iter()
            .map(|e| {
                json!({
                    "stream_id": e.stream_id,
                    "flags": e.flags,
                    "pts": e.pts,
                    "cluster_offset": e.cluster_offset,
                    "packet_index": e.packet_index,
                })
            })
            .collect();
    }
    out
}

// ---------------------------------------------------------------------------
// extract / export

/// 選んだチェーンをミックスして WAV に書く。
fn render_wav<W: Write>(
    reader: &mut Reader,
    selections: &[ChainSelection],
    channels: Option<usize>,
    format: SampleFormat,
    out: W,
) -> Result<W> {
    let dir = reader.directory().clone();
    let mut renderer = AudioRenderer::new(&dir, selections, channels)?;
    let total = renderer.len().max(0) as u64;
    let mut wav = WavWriter::new(
        out,
        OPUS_SAMPLE_RATE,
        renderer.channels() as u16,
        format,
        total,
    )?;
    let mut buf = vec![0f32; 4800 * renderer.channels()];
    let mut progress = Progress::new("audio");
    loop {
        let n = renderer.render(reader, &mut buf)?;
        if n == 0 {
            break;
        }
        wav.write(&buf[..n * renderer.channels()])?;
        progress.update(renderer.position() as f64, total as f64);
    }
    progress.finish();
    Ok(wav.finish()?)
}

pub fn extract(a: ExtractArgs) -> Result<ExitCode> {
    let mut reader = open(&a.file)?;
    let dir = reader.directory().clone();
    let ids: Vec<u16> = if a.chain.is_empty() {
        vec![
            dir.default_chain()
                .ok_or_else(|| anyhow!("the file has no default chain; use --chain"))?
                .id,
        ]
    } else {
        a.chain
            .iter()
            .map(|n| chain_id(&dir, n))
            .collect::<Result<_>>()?
    };
    let selections: Vec<ChainSelection> = ids
        .iter()
        .map(|&id| {
            if a.apply_gain {
                ChainSelection::with_chain_gain(&dir, id)
            } else {
                ChainSelection {
                    stream_id: id,
                    gain: 1.0,
                }
            }
        })
        .collect();
    let channels = a.channels.map(|c| c as usize);
    util::ensure_parent(&a.output)?;
    if extension(&a.output) == "wav" {
        let format = match a.format {
            WavFormat::S16 => SampleFormat::S16,
            WavFormat::F32 => SampleFormat::F32,
        };
        let f = BufWriter::new(File::create(&a.output)?);
        render_wav(&mut reader, &selections, channels, format, f)?
            .into_inner()
            .map_err(|e| e.into_error())?;
    } else {
        // WAV を ffmpeg にパイプして変換する。
        let ff = util::ffmpeg(true)?;
        let mut child = ff.spawn_with_stdin(&[
            "-f".as_ref(),
            "wav".as_ref(),
            "-i".as_ref(),
            "pipe:0".as_ref(),
            a.output.as_os_str(),
        ])?;
        let stdin = BufWriter::new(child.stdin.take().unwrap());
        let res = render_wav(&mut reader, &selections, channels, SampleFormat::F32, stdin);
        let res = res.and_then(|w| {
            w.into_inner()
                .map(drop)
                .map_err(|e| anyhow!(e.into_error()))
        });
        let status = child.wait()?;
        res?;
        if !status.success() {
            bail!("ffmpeg failed to write {}", a.output.display());
        }
    }
    println!("wrote {}", a.output.display());
    Ok(ExitCode::SUCCESS)
}

fn write_ivf(reader: &mut Reader, path: &Path) -> Result<u64> {
    let dir = reader.directory().clone();
    let v = dir.video().ok_or_else(|| anyhow!("no video stream"))?;
    let p = v.video().unwrap();
    let mut w = IvfWriter::new(
        BufWriter::new(File::create(path)?),
        p.width.min(u16::MAX as u32) as u16,
        p.height.min(u16::MAX as u32) as u16,
        v.timebase,
    )?;
    let mut cursor = reader.cursor(v.id);
    let mut n = 0;
    while let Some(pkt) = cursor.next_packet(reader)? {
        w.write_frame(pkt.pts, &pkt.data)?;
        n += 1;
    }
    w.finish()?.into_inner().map_err(|e| e.into_error())?;
    Ok(n)
}

pub fn extract_video(a: ExtractVideoArgs) -> Result<ExitCode> {
    let mut reader = open(&a.file)?;
    util::ensure_parent(&a.output)?;
    if extension(&a.output) == "ivf" {
        let n = write_ivf(&mut reader, &a.output)?;
        println!("wrote {} ({n} frames)", a.output.display());
    } else {
        let ff = util::ffmpeg(true)?;
        let tmp = tempfile::tempdir()?;
        let ivf = util::temp_path(&tmp, "video.ivf");
        write_ivf(&mut reader, &ivf)?;
        ff.run(&[
            "-i".as_ref(),
            ivf.as_os_str(),
            "-c".as_ref(),
            "copy".as_ref(),
            a.output.as_os_str(),
        ])?;
        println!("wrote {}", a.output.display());
    }
    Ok(ExitCode::SUCCESS)
}

pub fn export(a: ExportArgs) -> Result<ExitCode> {
    let mut reader = open(&a.file)?;
    let dir = reader.directory().clone();
    let mut selections = Vec::new();
    if !a.no_default
        && let Some(d) = dir.default_chain()
    {
        selections.push(ChainSelection::with_chain_gain(&dir, d.id));
    }
    for name in &a.chain {
        selections.push(ChainSelection::with_chain_gain(&dir, chain_id(&dir, name)?));
    }
    let ff = util::ffmpeg(true)?;
    let tmp = tempfile::tempdir()?;
    let ivf = util::temp_path(&tmp, "video.ivf");
    write_ivf(&mut reader, &ivf)?;
    let mut args: Vec<std::ffi::OsString> = vec!["-i".into(), ivf.clone().into()];
    if selections.is_empty() {
        args.extend([
            "-map".into(),
            "0:v:0".into(),
            "-c:v".into(),
            "copy".into(),
            "-an".into(),
        ]);
    } else {
        let wav = util::temp_path(&tmp, "audio.wav");
        let f = BufWriter::new(File::create(&wav)?);
        render_wav(&mut reader, &selections, Some(2), SampleFormat::F32, f)?
            .into_inner()
            .map_err(|e| e.into_error())?;
        let ext = extension(&a.output);
        let codec = a.audio_codec.clone().unwrap_or_else(|| {
            if matches!(ext.as_str(), "mp4" | "m4v" | "mov") {
                "aac"
            } else {
                "libopus"
            }
            .into()
        });
        args.extend(["-i".into(), wav.into()]);
        args.extend(["-map", "0:v:0", "-map", "1:a:0", "-c:v", "copy", "-c:a"].map(Into::into));
        args.extend([codec.into(), "-b:a".into(), a.audio_bitrate.clone().into()]);
        if matches!(ext.as_str(), "mp4" | "m4v" | "mov") {
            args.extend(["-movflags".into(), "+faststart".into()]);
        }
    }
    args.push(a.output.clone().into());
    util::ensure_parent(&a.output)?;
    ff.run(&args)?;
    let names: Vec<String> = selections
        .iter()
        .filter_map(|s| dir.stream(s.stream_id).map(|e| format!("{:?}", e.name)))
        .collect();
    println!(
        "wrote {} (chains: {})",
        a.output.display(),
        if names.is_empty() {
            "none".into()
        } else {
            names.join(" + ")
        }
    );
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------------------
// snapshot

pub fn snapshot(a: SnapshotArgs) -> Result<ExitCode> {
    let mut reader = open(&a.file)?;
    let dir = reader.directory().clone();
    let v = dir.video().ok_or_else(|| anyhow!("no video stream"))?;
    let secs = parse_time(&a.at)?;
    let pts = v
        .timebase
        .floor_seconds(secs)
        .clamp(0, (v.duration - 1).max(0));
    let mut stream = VideoStream::new(&dir, 0)?;
    stream.seek(&mut reader, pts)?;
    let frame = stream
        .next_frame(&mut reader)?
        .ok_or_else(|| anyhow!("no frame at {}", a.at))?;
    let (w, h) = (frame.width() as usize, frame.height() as usize);
    let rgba = frame.to_rgba8();
    let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
    ppm.reserve(w * h * 3);
    for px in rgba.chunks_exact(4) {
        ppm.extend_from_slice(&px[..3]);
    }
    util::ensure_parent(&a.output)?;
    if extension(&a.output) == "ppm" {
        std::fs::write(&a.output, &ppm)?;
    } else {
        let tmp = tempfile::tempdir()?;
        let p = util::temp_path(&tmp, "frame.ppm");
        std::fs::write(&p, &ppm)?;
        util::ffmpeg(true)?.run(&[
            "-i".as_ref(),
            p.as_os_str(),
            "-frames:v".as_ref(),
            "1".as_ref(),
            "-update".as_ref(),
            "1".as_ref(),
            a.output.as_os_str(),
        ])?;
    }
    println!(
        "wrote {} ({w}x{h}, frame at {})",
        a.output.display(),
        format_time(v.timebase.to_seconds(frame.pts()))
    );
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------------------
// verify

struct Check {
    problems: Vec<String>,
}

impl Check {
    fn ok(&mut self, cond: bool, msg: impl FnOnce() -> String) {
        if !cond {
            let m = msg();
            println!("  ✗ {m}");
            self.problems.push(m);
        }
    }
}

pub fn verify(a: VerifyArgs) -> Result<ExitCode> {
    let mut reader = open(&a.file)?;
    let dir = reader.directory().clone();
    let mut c = Check {
        problems: Vec::new(),
    };
    if reader.was_recovered() {
        c.problems.push("incomplete append at end of file".into());
    }

    println!("blocks:");
    match reader.scan_blocks() {
        Ok(blocks) => {
            let count = |k| blocks.iter().filter(|b| b.kind == k).count();
            let dirs = count(BLOCK_DIRECTORY);
            println!(
                "  {} clusters, {} directories ({} stale), {} footers, {} other — CRC OK",
                count(BLOCK_CLUSTER),
                dirs,
                dirs.saturating_sub(1),
                count(BLOCK_FOOTER),
                blocks.len() - count(BLOCK_CLUSTER) - dirs - count(BLOCK_FOOTER)
            );
            c.ok(
                blocks
                    .iter()
                    .any(|b| b.kind == BLOCK_DIRECTORY && b.offset == reader.directory_offset()),
                || "active directory is not on the block chain".into(),
            );
        }
        Err(e) => c.ok(false, || format!("block scan failed: {e}")),
    }

    println!("streams:");
    for s in &dir.streams {
        if let Err(e) = verify_stream(&mut reader, &dir, s, &mut c) {
            c.ok(false, || format!("stream #{}: {e:#}", s.id));
        }
    }

    if !a.no_decode {
        println!("decode:");
        if let Err(e) = verify_decode(&mut reader, &dir, &mut c) {
            c.ok(false, || format!("decode: {e:#}"));
        }
    }

    if c.problems.is_empty() {
        println!("result: OK");
        Ok(ExitCode::SUCCESS)
    } else {
        println!("result: {} problem(s)", c.problems.len());
        Ok(ExitCode::FAILURE)
    }
}

fn verify_stream<R: Read + Seek>(
    reader: &mut FdmvReader<R>,
    dir: &Directory,
    s: &StreamEntry,
    c: &mut Check,
) -> Result<()> {
    let mut cursor = reader.cursor(s.id);
    let (mut packets, mut bytes, mut keys, mut seg_starts) = (0u64, 0u64, 0u64, 0usize);
    let mut last_pts: Option<i64> = None;
    let mut first_ok = true;
    let mut order_ok = true;
    while let Some(p) = cursor.next_packet(reader)? {
        if packets == 0 && s.is_video() && !p.is_keyframe() {
            first_ok = false;
        }
        if let Some(l) = last_pts
            && p.pts <= l
            && !p.is_segment_start()
        {
            order_ok = false;
        }
        last_pts = Some(p.pts);
        packets += 1;
        bytes += p.data.len() as u64;
        keys += p.is_keyframe() as u64;
        seg_starts += p.is_segment_start() as usize;
    }
    let secs = s.duration_seconds().max(1e-9);
    let label = if s.is_video() {
        "video".to_string()
    } else {
        format!("chain {:?}", s.name)
    };
    println!(
        "  #{} {label}: {packets} packets, {}, {:.0} kbps{}",
        s.id,
        human_size(bytes),
        bytes as f64 * 8.0 / secs / 1000.0,
        if s.is_video() {
            format!(", {keys} keyframes")
        } else {
            String::new()
        }
    );
    c.ok(order_ok, || {
        format!("{label}: timestamps are not increasing")
    });
    if s.is_video() {
        c.ok(packets > 0, || "video has no packets".into());
        c.ok(first_ok, || "video does not start with a keyframe".into());
        c.ok(last_pts.is_none_or(|l| l < s.duration), || {
            "video packet beyond duration".into()
        });
        let indexed = dir
            .index_of(s.id)
            .iter()
            .filter(|e| e.is_keyframe())
            .count() as u64;
        c.ok(indexed == keys, || {
            format!("video: {keys} keyframes but {indexed} in index")
        });
    }
    if let Some(p) = s.chain() {
        c.ok(seg_starts == p.segments.len(), || {
            format!(
                "{label}: {seg_starts} segment starts but {} segments",
                p.segments.len()
            )
        });
    }
    Ok(())
}

fn verify_decode<R: Read + Seek>(
    reader: &mut FdmvReader<R>,
    dir: &Directory,
    c: &mut Check,
) -> Result<()> {
    let mut stream = VideoStream::new(dir, 0)?;
    let mut frames = 0u64;
    let mut progress = Progress::new("  video");
    let dur = dir.video().map(|v| v.duration).unwrap_or(1) as f64;
    loop {
        match stream.next_frame(reader) {
            Ok(Some(f)) => {
                frames += 1;
                progress.update(f.pts() as f64, dur);
            }
            Ok(None) => break,
            Err(e) => {
                progress.finish();
                c.ok(false, || {
                    format!("video decode error after {frames} frames: {e}")
                });
                break;
            }
        }
    }
    progress.finish();
    let packets = {
        let mut cur = reader.cursor(dir.video().unwrap().id);
        let mut n = 0u64;
        while cur.next_packet(reader)?.is_some() {
            n += 1;
        }
        n
    };
    println!("  video: {frames} frames decoded");
    c.ok(frames == packets, || {
        format!("video: {packets} packets but {frames} frames decoded")
    });

    for s in dir.chains() {
        let mut dec = ChainDecoder::new(s)?;
        let mut cur = reader.cursor(s.id);
        let mut samples = 0i64;
        let mut err = None;
        while let Some(p) = cur.next_packet(reader)? {
            match dec.decode(&p) {
                Ok(Some(d)) => samples += d.frames() as i64,
                Ok(None) => {}
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        let expected: i64 = s.chain().unwrap().segments.iter().map(|g| g.length).sum();
        println!(
            "  chain {:?}: {} decoded",
            s.name,
            format_time(Rational::OPUS.to_seconds(samples))
        );
        c.ok(err.is_none(), || {
            format!(
                "chain {:?}: decode error: {}",
                s.name,
                err.as_ref().unwrap()
            )
        });
        c.ok(samples == expected, || {
            format!(
                "chain {:?}: decoded {samples} samples, segments say {expected}",
                s.name
            )
        });
    }
    Ok(())
}
