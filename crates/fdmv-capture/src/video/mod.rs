//! 画面・ウィンドウの取り込み。
//!
//! 取り込みは届いたフレームを [`FrameSlot`] に置くだけ。一定間隔で読み出してエンコーダに送るのは
//! [`crate::encoder`] の役目（画面は変化があったときしかフレームが来ないことがあるため）。
//!
//! | OS | 方式 |
//! |---|---|
//! | Linux | xdg-desktop-portal の画面共有ダイアログ ＋ PipeWire（X11 では ffmpeg の x11grab も使える） |
//! | Windows | ffmpeg の gdigrab（画面全体・モニタ・ウィンドウ） |
//! | macOS | ScreenCaptureKit（ディスプレイ・ウィンドウ） |

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use libfdmv::ffmpeg::Ffmpeg;

#[cfg(any(target_os = "linux", windows))]
mod ffgrab;
#[cfg(target_os = "macos")]
pub(crate) mod macos;
mod pattern;
#[cfg(target_os = "linux")]
mod portal;
#[cfg(windows)]
mod win;

/// BGRA（1 画素 4 バイト、行の詰め物なし）のフレーム。アルファの値は意味を持たない。
#[derive(Clone, Debug)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

impl Frame {
    /// 行ごとに `stride` バイトの画像から作る。`order` はバイト順（[`PixelOrder`]）。
    pub fn from_strided(
        width: u32,
        height: u32,
        stride: usize,
        data: &[u8],
        order: PixelOrder,
    ) -> Option<Frame> {
        let row = width as usize * 4;
        if stride < row || data.len() < stride * (height as usize - 1) + row {
            return None;
        }
        let mut bgra = Vec::with_capacity(row * height as usize);
        for y in 0..height as usize {
            let src = &data[y * stride..y * stride + row];
            match order {
                // アルファは使わない（エンコード時に捨てる）ので、BGRx もそのまま写す。
                PixelOrder::Bgra | PixelOrder::Bgrx => bgra.extend_from_slice(src),
                PixelOrder::Rgba | PixelOrder::Rgbx => {
                    let opaque = order == PixelOrder::Rgbx;
                    for px in src.chunks_exact(4) {
                        bgra.extend_from_slice(&[
                            px[2],
                            px[1],
                            px[0],
                            if opaque { 255 } else { px[3] },
                        ]);
                    }
                }
            }
        }
        Some(Frame {
            width,
            height,
            bgra,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelOrder {
    Bgra,
    Bgrx,
    Rgba,
    Rgbx,
}

/// 最新のフレームを 1 枚だけ持つ置き場。
#[derive(Default)]
pub struct FrameSlot {
    frame: Mutex<Option<Arc<Frame>>>,
    generation: AtomicU64,
}

impl FrameSlot {
    pub fn put(&self, frame: Frame) {
        *self.frame.lock().unwrap() = Some(Arc::new(frame));
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// フレームが届くたびに増える番号。
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// (番号, 最新のフレーム)
    pub fn latest(&self) -> (u64, Option<Arc<Frame>>) {
        let f = self.frame.lock().unwrap().clone();
        (self.generation(), f)
    }
}

/// 動いている取り込み。drop すると止まる。
pub trait VideoCapture: Send {
    fn slot(&self) -> Arc<FrameSlot>;
    /// 取り込みが止まってしまったときの理由。
    fn error(&self) -> Option<String>;
}

/// 録画する対象。
#[derive(Clone, Debug, PartialEq)]
pub enum VideoTarget {
    /// Linux: 画面共有ダイアログ（xdg-desktop-portal）で選ぶ。
    Portal,
    /// Linux (X11): ffmpeg の x11grab で画面全体。
    X11Screen { display: String },
    /// Windows: ffmpeg の gdigrab。
    Gdi {
        /// `desktop` または `title=...`
        input: String,
        /// 画面の一部（モニタ）を切り出すときの (x, y, 幅, 高さ)。
        region: Option<(i32, i32, u32, u32)>,
    },
    /// macOS: ScreenCaptureKit のディスプレイ。
    MacDisplay { id: u32 },
    /// macOS: ScreenCaptureKit のウィンドウ。
    MacWindow { id: u32 },
    /// 動作確認用のテストパターン。
    TestPattern,
}

/// 一覧に出す録画対象。
#[derive(Clone, Debug)]
pub struct TargetInfo {
    pub target: VideoTarget,
    pub label: String,
    /// ウィンドウの持ち主のアプリ（音声の自動選択に使う）。
    pub app_key: Option<String>,
}

/// 選べる録画対象の一覧。
pub fn targets() -> Result<Vec<TargetInfo>> {
    let mut v = platform_targets()?;
    if std::env::var_os("FDMV_TEST_PATTERN").is_some() {
        v.push(TargetInfo {
            target: VideoTarget::TestPattern,
            label: "テストパターン".into(),
            app_key: None,
        });
    }
    Ok(v)
}

fn platform_targets() -> Result<Vec<TargetInfo>> {
    #[cfg(target_os = "linux")]
    {
        let mut v = vec![TargetInfo {
            target: VideoTarget::Portal,
            label: "画面またはウィンドウ（共有ダイアログで選ぶ）".into(),
            app_key: None,
        }];
        if std::env::var_os("WAYLAND_DISPLAY").is_none()
            && let Some(display) = std::env::var_os("DISPLAY")
        {
            v.push(TargetInfo {
                target: VideoTarget::X11Screen {
                    display: display.to_string_lossy().into_owned(),
                },
                label: "画面全体（X11）".into(),
                app_key: None,
            });
        }
        Ok(v)
    }
    #[cfg(windows)]
    {
        win::targets()
    }
    #[cfg(target_os = "macos")]
    {
        macos::targets()
    }
    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    {
        Ok(Vec::new())
    }
}

/// 取り込みを始める。ダイアログを出すことがあるので、UI スレッド以外で呼ぶ。
pub fn start(ff: &Ffmpeg, target: &VideoTarget, fps: u32) -> Result<Box<dyn VideoCapture>> {
    let _ = (ff, fps);
    match target {
        VideoTarget::TestPattern => {
            // FDMV_TEST_PATTERN=2560x1440 のように大きさを指定できる。
            let (w, h) = std::env::var("FDMV_TEST_PATTERN")
                .ok()
                .and_then(|v| {
                    let (w, h) = v.split_once('x')?;
                    Some((w.parse().ok()?, h.parse().ok()?))
                })
                .unwrap_or((1280, 720));
            Ok(pattern::start(w, h, fps))
        }
        #[cfg(target_os = "linux")]
        VideoTarget::Portal => portal::start(),
        #[cfg(target_os = "linux")]
        VideoTarget::X11Screen { display } => ffgrab::start(
            ff,
            &["-f", "x11grab", "-draw_mouse", "1"],
            display,
            None,
            fps,
        ),
        #[cfg(windows)]
        VideoTarget::Gdi { input, region } => ffgrab::start(
            ff,
            &["-f", "gdigrab", "-draw_mouse", "1"],
            input,
            *region,
            fps,
        ),
        #[cfg(target_os = "macos")]
        VideoTarget::MacDisplay { .. } | VideoTarget::MacWindow { .. } => {
            macos::start_video(target, fps)
        }
        #[allow(unreachable_patterns)]
        _ => anyhow::bail!("この OS ではこの録画対象を使えません"),
    }
}

/// `src` を `w`×`h` の BGRA に収める（縦横比を保って縮小・拡大し、余白は黒）。
pub fn fit(src: &Frame, w: u32, h: u32, out: &mut Vec<u8>) {
    let (w, h) = (w as usize, h as usize);
    out.resize(w * h * 4, 0);
    let (sw, sh) = (src.width as usize, src.height as usize);
    if sw == 0 || sh == 0 {
        out.fill(0);
        return;
    }
    // ほぼ同じ大きさ（偶数に揃えるための 1 画素の違い）なら左上を切り出す。
    if sw >= w && sh >= h && sw - w <= 1 && sh - h <= 1 {
        for y in 0..h {
            out[y * w * 4..(y + 1) * w * 4]
                .copy_from_slice(&src.bgra[y * sw * 4..y * sw * 4 + w * 4]);
        }
        return;
    }
    // 縮尺（最近傍）。
    let scale = (w as f64 / sw as f64).min(h as f64 / sh as f64);
    let dw = ((sw as f64 * scale).round() as usize).clamp(1, w);
    let dh = ((sh as f64 * scale).round() as usize).clamp(1, h);
    let (ox, oy) = ((w - dw) / 2, (h - dh) / 2);
    out.fill(0);
    for px in out.chunks_exact_mut(4) {
        px[3] = 255;
    }
    let xs: Vec<usize> = (0..dw).map(|x| (x * sw / dw).min(sw - 1)).collect();
    for y in 0..dh {
        let sy = (y * sh / dh).min(sh - 1);
        let src_row = &src.bgra[sy * sw * 4..(sy + 1) * sw * 4];
        let dst_row = &mut out[((oy + y) * w + ox) * 4..((oy + y) * w + ox + dw) * 4];
        for (d, &sx) in dst_row.chunks_exact_mut(4).zip(&xs) {
            d.copy_from_slice(&src_row[sx * 4..sx * 4 + 4]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_letterboxes() {
        let src = Frame {
            width: 2,
            height: 1,
            bgra: vec![1, 2, 3, 255, 4, 5, 6, 255],
        };
        let mut out = Vec::new();
        fit(&src, 4, 4, &mut out);
        // 4×2 に拡大されて上下に 1 行ずつ黒
        assert_eq!(&out[0..4], &[0, 0, 0, 255]);
        assert_eq!(&out[16..20], &[1, 2, 3, 255]);
        assert_eq!(&out[28..32], &[4, 5, 6, 255]);
        assert_eq!(&out[48..52], &[0, 0, 0, 255]);
    }

    #[test]
    fn fit_crops_odd_size() {
        let src = Frame {
            width: 3,
            height: 3,
            bgra: (0..36).collect(),
        };
        let mut out = Vec::new();
        fit(&src, 2, 2, &mut out);
        assert_eq!(
            out,
            [0, 1, 2, 3, 4, 5, 6, 7, 12, 13, 14, 15, 16, 17, 18, 19]
        );
    }

    #[test]
    fn converts_rgbx() {
        let f = Frame::from_strided(
            1,
            2,
            8,
            &[1, 2, 3, 0, 9, 9, 9, 9, 4, 5, 6, 0],
            PixelOrder::Rgbx,
        )
        .unwrap();
        assert_eq!(f.bgra, [3, 2, 1, 255, 6, 5, 4, 255]);
    }
}
