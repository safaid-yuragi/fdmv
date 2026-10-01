//! コーデックを使わずに、コンテナの読み書き・追記・リカバリを検査する。

use std::io::Cursor;

use libfdmv::format::{Rational, flags};
use libfdmv::{
    ChainParams, ChainRole, Directory, FdmvReader, Muxer, Packet, Segment, StreamEntry, VideoParams,
};

const VIDEO_TB: Rational = Rational::new(1, 30);

fn video_entry() -> StreamEntry {
    StreamEntry::new_video(
        VideoParams {
            width: 64,
            height: 36,
            frame_rate: Rational::new(30, 1),
        },
        VIDEO_TB,
    )
}

fn chain_entry(name: &str, role: ChainRole, segments: Vec<Segment>) -> StreamEntry {
    let mut e = StreamEntry::new_chain(
        name,
        ChainParams {
            role,
            channels: 2,
            gain_db: 0.0,
            segments,
        },
    );
    e.duration = e
        .chain()
        .unwrap()
        .segments
        .last()
        .map(|s| s.end())
        .unwrap_or(0);
    e
}

/// 3 秒の映像（30 fps、1 秒ごとにキーフレーム）、全区間のデフォルトチェーン、2 区間のサブチェーン。
fn build() -> Vec<u8> {
    let mut mux = Muxer::new(Cursor::new(Vec::new())).unwrap();
    let mut v = video_entry();
    v.duration = 90;
    let vid = mux.add_stream(v).unwrap();
    let main_seg = Segment {
        start: 0,
        length: 144_000,
        pre_skip: 312,
    };
    let main = mux
        .add_stream(chain_entry("main", ChainRole::Default, vec![main_seg]))
        .unwrap();
    let subs = vec![
        Segment {
            start: 48_000,
            length: 9_600,
            pre_skip: 312,
        },
        Segment {
            start: 96_000,
            length: 4_800,
            pre_skip: 312,
        },
    ];
    let sub = mux
        .add_stream(chain_entry("解説", ChainRole::Sub, subs.clone()))
        .unwrap();

    // 時刻順にパケットを並べる
    let mut pkts: Vec<(f64, Packet)> = Vec::new();
    for i in 0..90i64 {
        let f = if i % 30 == 0 { flags::KEYFRAME } else { 0 };
        pkts.push((
            i as f64 / 30.0,
            Packet {
                stream_id: vid,
                flags: f,
                pts: i,
                duration: 1,
                data: vec![i as u8; 100],
            },
        ));
    }
    let mut add_chain = |id: u16, seg: Segment| {
        let mut pts = seg.start - seg.pre_skip as i64;
        let mut first = true;
        while pts < seg.end() {
            let f = if first { flags::SEGMENT_START } else { 0 };
            first = false;
            pkts.push((
                pts as f64 / 48000.0,
                Packet {
                    stream_id: id,
                    flags: f,
                    pts,
                    duration: 960,
                    data: vec![id as u8; 10],
                },
            ));
            pts += 960;
        }
    };
    add_chain(main, main_seg);
    for s in subs {
        add_chain(sub, s);
    }
    pkts.sort_by(|a, b| a.0.total_cmp(&b.0));
    for (_, p) in pkts {
        mux.write_packet(p).unwrap();
    }
    mux.finish().unwrap().into_inner()
}

fn all_packets(r: &mut FdmvReader<Cursor<Vec<u8>>>, id: u16) -> Vec<Packet> {
    let mut c = r.cursor(id);
    let mut out = Vec::new();
    while let Some(p) = c.next_packet(r).unwrap() {
        out.push(p);
    }
    out
}

#[test]
fn write_and_read_back() {
    let mut r = FdmvReader::new(Cursor::new(build())).unwrap();
    assert!(!r.was_recovered());
    let dir = r.directory().clone();
    assert_eq!(dir.streams.len(), 3);
    assert_eq!(dir.default_chain().unwrap().name, "main");
    assert_eq!(dir.duration_seconds(), 3.0);

    let video = all_packets(&mut r, 0);
    assert_eq!(video.len(), 90);
    assert!(
        video
            .iter()
            .enumerate()
            .all(|(i, p)| p.pts == i as i64 && p.data[0] == i as u8)
    );
    let keys: Vec<i64> = dir
        .index_of(0)
        .iter()
        .filter(|e| e.is_keyframe())
        .map(|e| e.pts)
        .collect();
    assert_eq!(keys, vec![0, 30, 60]);

    let sub = all_packets(&mut r, 2);
    assert_eq!(sub.iter().filter(|p| p.is_segment_start()).count(), 2);
    // 各クラスタは 1 秒程度で区切られ、ストリームを含むクラスタはすべて索引にある
    let blocks = r.scan_blocks().unwrap();
    assert!(blocks.len() >= 5, "{} blocks", blocks.len());
}

#[test]
fn seek_to_keyframe() {
    let mut r = FdmvReader::new(Cursor::new(build())).unwrap();
    let mut c = r.cursor(0);
    let e = c
        .seek_last_where(&mut r, |e| e.is_keyframe() && e.pts <= 45)
        .unwrap()
        .unwrap();
    assert_eq!(e.pts, 30);
    let p = c.next_packet(&mut r).unwrap().unwrap();
    assert_eq!(p.pts, 30);
    assert!(p.is_keyframe());
    assert_eq!(c.next_packet(&mut r).unwrap().unwrap().pts, 31);
}

fn append_chain(file: Vec<u8>, at: u64, dir: Directory, name: &str) -> Vec<u8> {
    let mut mux = Muxer::append(Cursor::new(file), dir, at).unwrap();
    let seg = Segment {
        start: 24_000,
        length: 48_000,
        pre_skip: 312,
    };
    let id = mux
        .add_stream(chain_entry(name, ChainRole::Sub, vec![seg]))
        .unwrap();
    // 既存のストリームには書けない
    assert!(
        mux.write_packet(Packet {
            stream_id: 0,
            flags: 0,
            pts: 1000,
            duration: 1,
            data: vec![]
        })
        .is_err()
    );
    let mut pts = seg.start - 312;
    let mut first = true;
    while pts < seg.end() {
        let f = if first { flags::SEGMENT_START } else { 0 };
        first = false;
        mux.write_packet(Packet {
            stream_id: id,
            flags: f,
            pts,
            duration: 960,
            data: vec![0xab; 7],
        })
        .unwrap();
        pts += 960;
    }
    mux.finish().unwrap().into_inner()
}

#[test]
fn append_in_place_keeps_old_content() {
    let original = build();
    let r = FdmvReader::new(Cursor::new(original.clone())).unwrap();
    let dir = r.directory().clone();
    let len = original.len() as u64;
    let appended = append_chain(original.clone(), len, dir.clone(), "追加");
    assert_eq!(
        &appended[..original.len()],
        &original[..],
        "in-place append must not modify existing bytes"
    );

    let mut r = FdmvReader::new(Cursor::new(appended.clone())).unwrap();
    let d = r.directory().clone();
    assert_eq!(d.streams.len(), 4);
    let added = d.chain_by_name("追加").unwrap().id;
    assert_eq!(added, 3);
    assert_eq!(all_packets(&mut r, added).len(), 51);
    assert_eq!(all_packets(&mut r, 0).len(), 90);
    // 古いディレクトリとフッタもブロックとして並んでいる
    let blocks = r.scan_blocks().unwrap();
    assert_eq!(blocks.iter().filter(|b| &b.kind == b"DIRC").count(), 2);

    // 名前の重複は拒否される
    let mut mux = Muxer::append(Cursor::new(appended), d.clone(), 0).unwrap();
    mux.add_stream(chain_entry("追加", ChainRole::Sub, vec![]))
        .unwrap();
    assert!(mux.finish().is_err());
}

#[test]
fn append_after_directory_offset_drops_stale_directory() {
    let original = build();
    let r = FdmvReader::new(Cursor::new(original.clone())).unwrap();
    let dir = r.directory().clone();
    let at = r.directory_offset();
    let copy = original[..at as usize].to_vec();
    let appended = append_chain(copy, at, dir, "追加");
    let mut r = FdmvReader::new(Cursor::new(appended)).unwrap();
    let blocks = r.scan_blocks().unwrap();
    assert_eq!(blocks.iter().filter(|b| &b.kind == b"DIRC").count(), 1);
}

#[test]
fn interrupted_append_is_recovered() {
    let original = build();
    let r = FdmvReader::new(Cursor::new(original.clone())).unwrap();
    let dir = r.directory().clone();
    let appended = append_chain(original.clone(), original.len() as u64, dir, "追加");
    for cut in [
        original.len() + 10,
        original.len() + 200,
        appended.len() - 1,
    ] {
        let r = FdmvReader::new(Cursor::new(appended[..cut].to_vec())).unwrap();
        assert!(r.was_recovered(), "cut at {cut}");
        assert_eq!(r.directory().streams.len(), 3);
        assert_eq!(r.valid_end(), original.len() as u64);
    }
}

#[test]
fn corrupted_files_are_rejected() {
    let mut f = build();
    // ディレクトリが壊れていて、それ以前に有効なものも無い
    let r = FdmvReader::new(Cursor::new(f.clone())).unwrap();
    let d = r.directory_offset() as usize;
    f[d + 20] ^= 0xff;
    assert!(FdmvReader::new(Cursor::new(f.clone())).is_err());
    // マジック不一致
    let mut g = build();
    g[1] = b'X';
    assert!(matches!(
        FdmvReader::new(Cursor::new(g)),
        Err(libfdmv::Error::NotFdmv)
    ));
    // クラスタのビット反転は読み込み時に CRC で検出される
    let mut h = build();
    h[100] ^= 1;
    let mut r = FdmvReader::new(Cursor::new(h)).unwrap();
    assert!(r.cursor(0).next_packet(&mut r).is_err());
}
