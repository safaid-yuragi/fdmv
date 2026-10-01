use std::collections::VecDeque;
use std::io::{Read, Seek};

use dav1d::{PlanarImageComponent, pixel};

use crate::error::{Error, Result, invalid};
use crate::model::{Directory, Packet};
use crate::reader::{FdmvReader, StreamCursor};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelLayout {
    I400,
    I420,
    I422,
    I444,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaneKind {
    Y,
    U,
    V,
}

impl PlaneKind {
    fn component(self) -> PlanarImageComponent {
        match self {
            PlaneKind::Y => PlanarImageComponent::Y,
            PlaneKind::U => PlanarImageComponent::U,
            PlaneKind::V => PlanarImageComponent::V,
        }
    }
}

/// デコード済みの映像フレーム（YUV 平面）。
pub struct VideoFrame {
    pic: dav1d::Picture,
}

impl VideoFrame {
    /// 提示時刻（映像ストリームのタイムベース）。
    pub fn pts(&self) -> i64 {
        self.pic.timestamp().unwrap_or(0)
    }
    pub fn width(&self) -> u32 {
        self.pic.width()
    }
    pub fn height(&self) -> u32 {
        self.pic.height()
    }
    pub fn bit_depth(&self) -> u8 {
        self.pic.bit_depth() as u8
    }
    pub fn layout(&self) -> PixelLayout {
        match self.pic.pixel_layout() {
            dav1d::PixelLayout::I400 => PixelLayout::I400,
            dav1d::PixelLayout::I420 => PixelLayout::I420,
            dav1d::PixelLayout::I422 => PixelLayout::I422,
            dav1d::PixelLayout::I444 => PixelLayout::I444,
        }
    }
    pub fn full_range(&self) -> bool {
        matches!(self.pic.color_range(), pixel::YUVRange::Full)
    }
    /// 1 行あたりのバイト数。
    pub fn stride(&self, plane: PlaneKind) -> usize {
        self.pic.stride(plane.component()) as usize
    }
    /// 平面のバイト列。ビット深度が 8 を超える場合は u16（ネイティブエンディアン）が並ぶ。
    pub fn plane(&self, plane: PlaneKind) -> &[u8] {
        let c = plane.component();
        let (stride, height) = self.pic.plane_data_geometry(c);
        let ptr = self.pic.plane_data_ptr(c) as *const u8;
        if ptr.is_null() || stride == 0 {
            return &[];
        }
        // Picture が生きている間、平面データは有効。
        unsafe { std::slice::from_raw_parts(ptr, stride as usize * height as usize) }
    }

    /// 8 bit RGBA に変換する（BT.709 / BT.601、固定小数点。再生用に十分な速度）。
    pub fn to_rgba8(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.write_rgba8(&mut out);
        out
    }

    /// `out` を `width * height * 4` バイトの RGBA で埋める（バッファの再利用用）。
    pub fn write_rgba8(&self, out: &mut Vec<u8>) {
        let (w, h) = (self.width() as usize, self.height() as usize);
        out.resize(w * h * 4, 255);
        let depth = self.bit_depth() as u32;
        // 行列係数: 指定が無ければ HD 以上は BT.709、それ未満は BT.601。
        let bt709 = match self.pic.matrix_coefficients() {
            pixel::MatrixCoefficients::BT709 => true,
            pixel::MatrixCoefficients::BT470BG | pixel::MatrixCoefficients::ST170M => false,
            _ => h >= 720,
        };
        let c = Coefficients::new(bt709, self.full_range(), depth);
        let layout = self.layout();
        let (sx, sy) = match layout {
            PixelLayout::I420 => (1, 1),
            PixelLayout::I422 => (1, 0),
            _ => (0, 0),
        };
        let planes = [
            (self.plane(PlaneKind::Y), self.stride(PlaneKind::Y)),
            (self.plane(PlaneKind::U), self.stride(PlaneKind::U)),
            (self.plane(PlaneKind::V), self.stride(PlaneKind::V)),
        ];
        let gray = layout == PixelLayout::I400;
        if depth > 8 {
            convert::<true>(out, w, h, &planes, sx, sy, gray, &c);
        } else {
            convert::<false>(out, w, h, &planes, sx, sy, gray, &c);
        }
    }
}

/// 16 bit 固定小数点の変換係数。
struct Coefficients {
    y_off: i32,
    c_off: i32,
    y: i32,
    r_cr: i32,
    g_cb: i32,
    g_cr: i32,
    b_cb: i32,
}

impl Coefficients {
    fn new(bt709: bool, full: bool, depth: u32) -> Self {
        let (kr, kb) = if bt709 {
            (0.2126, 0.0722)
        } else {
            (0.299, 0.114)
        };
        let kg = 1.0 - kr - kb;
        let shift = depth.saturating_sub(8);
        let (ys, cs) = if full {
            (1.0, 1.0)
        } else {
            (255.0 / 219.0, 255.0 / 224.0)
        };
        let f = |v: f64| (v * 65536.0 / (1u32 << shift) as f64).round() as i32;
        Coefficients {
            y_off: if full { 0 } else { 16 << shift },
            c_off: 128 << shift,
            y: f(ys),
            r_cr: f(cs * 2.0 * (1.0 - kr)),
            g_cb: f(cs * 2.0 * (1.0 - kb) * kb / kg),
            g_cr: f(cs * 2.0 * (1.0 - kr) * kr / kg),
            b_cb: f(cs * 2.0 * (1.0 - kb)),
        }
    }
}

#[inline(always)]
fn load<const HBD: bool>(plane: &[u8], row: usize, x: usize) -> i32 {
    if HBD {
        u16::from_ne_bytes([plane[row + 2 * x], plane[row + 2 * x + 1]]) as i32
    } else {
        plane[row + x] as i32
    }
}

#[allow(clippy::too_many_arguments)]
fn convert<const HBD: bool>(
    out: &mut [u8],
    w: usize,
    h: usize,
    planes: &[(&[u8], usize); 3],
    sx: usize,
    sy: usize,
    gray: bool,
    c: &Coefficients,
) {
    let clamp = |v: i32| ((v + 32768) >> 16).clamp(0, 255) as u8;
    for (y, dst) in out.chunks_exact_mut(w * 4).take(h).enumerate() {
        let yrow = y * planes[0].1;
        let crow_u = (y >> sy) * planes[1].1;
        let crow_v = (y >> sy) * planes[2].1;
        for (x, px) in dst.chunks_exact_mut(4).enumerate() {
            let yy = (load::<HBD>(planes[0].0, yrow, x) - c.y_off) * c.y;
            let (cb, cr) = if gray {
                (0, 0)
            } else {
                (
                    load::<HBD>(planes[1].0, crow_u, x >> sx) - c.c_off,
                    load::<HBD>(planes[2].0, crow_v, x >> sx) - c.c_off,
                )
            };
            px[0] = clamp(yy + c.r_cr * cr);
            px[1] = clamp(yy - c.g_cb * cb - c.g_cr * cr);
            px[2] = clamp(yy + c.b_cb * cb);
            px[3] = 255;
        }
    }
}

fn dav1d_err(e: dav1d::Error) -> Error {
    Error::Decoder(format!("dav1d: {e:?}"))
}

/// AV1 デコーダ（dav1d）。パケットを送り、フレームを受け取る。
pub struct VideoDecoder {
    dec: dav1d::Decoder,
    ready: VecDeque<VideoFrame>,
}

impl VideoDecoder {
    /// `threads` = 0 なら自動。
    pub fn new(threads: u32) -> Result<Self> {
        let mut s = dav1d::Settings::new();
        s.set_n_threads(threads);
        let dec = dav1d::Decoder::with_settings(&s).map_err(dav1d_err)?;
        Ok(VideoDecoder {
            dec,
            ready: VecDeque::new(),
        })
    }

    fn collect(&mut self) -> Result<()> {
        loop {
            match self.dec.get_picture() {
                Ok(pic) => self.ready.push_back(VideoFrame { pic }),
                Err(dav1d::Error::Again) => return Ok(()),
                Err(e) => return Err(dav1d_err(e)),
            }
        }
    }

    pub fn send(&mut self, pkt: Packet) -> Result<()> {
        let r = self
            .dec
            .send_data(pkt.data, None, Some(pkt.pts), Some(pkt.duration as i64));
        match r {
            Ok(()) => {}
            Err(dav1d::Error::Again) => loop {
                // デコーダが詰まっているので、フレームを取り出してから残りを送る。
                self.collect()?;
                match self.dec.send_pending_data() {
                    Ok(()) => break,
                    Err(dav1d::Error::Again) => continue,
                    Err(e) => return Err(dav1d_err(e)),
                }
            },
            Err(e) => return Err(dav1d_err(e)),
        }
        self.collect()
    }

    pub fn receive(&mut self) -> Option<VideoFrame> {
        self.ready.pop_front()
    }

    /// 入力の終わり。デコーダ内に残っているフレームを取り出せるようにする。
    pub fn drain(&mut self) -> Result<()> {
        self.collect()
    }

    /// シーク時に呼ぶ。内部状態と未取得のフレームを捨てる。
    pub fn flush(&mut self) {
        self.dec.flush();
        self.ready.clear();
    }
}

/// ファイル内の映像ストリームを順に、またはシークしてデコードする。
pub struct VideoStream {
    cursor: StreamCursor,
    dec: VideoDecoder,
    eof: bool,
    /// シーク後、この pts より前のフレームは捨てる。
    skip_before: Option<i64>,
}

impl VideoStream {
    pub fn new(dir: &Directory, threads: u32) -> Result<Self> {
        let Some(v) = dir.video() else {
            return invalid("no video stream");
        };
        Ok(VideoStream {
            cursor: StreamCursor::new(dir, v.id),
            dec: VideoDecoder::new(threads)?,
            eof: false,
            skip_before: None,
        })
    }

    /// `pts` を含むフレームから読めるよう、直前のキーフレームに移動する。
    /// 次の [`VideoStream::next_frame`] は、`pts` に表示されるフレームを返す。
    pub fn seek<R: Read + Seek>(&mut self, reader: &mut FdmvReader<R>, pts: i64) -> Result<()> {
        self.dec.flush();
        self.eof = false;
        self.cursor
            .seek_last_where(reader, |e| e.is_keyframe() && e.pts <= pts)?;
        self.skip_before = Some(pts);
        Ok(())
    }

    pub fn next_frame<R: Read + Seek>(
        &mut self,
        reader: &mut FdmvReader<R>,
    ) -> Result<Option<VideoFrame>> {
        loop {
            let frame = loop {
                if let Some(f) = self.dec.receive() {
                    break Some(f);
                }
                if self.eof {
                    break None;
                }
                match self.cursor.next_packet(reader)? {
                    Some(p) => self.dec.send(p)?,
                    None => {
                        self.eof = true;
                        self.dec.drain()?;
                    }
                }
            };
            let Some(f) = frame else { return Ok(None) };
            if let Some(t) = self.skip_before {
                // pts 以前の最後のフレームを返したいので、1 枚先を見て判断する。
                let next_pts = self.peek_pts(reader)?;
                if next_pts.is_some_and(|n| n <= t) {
                    continue;
                }
                self.skip_before = None;
            }
            return Ok(Some(f));
        }
    }

    fn peek_pts<R: Read + Seek>(&mut self, reader: &mut FdmvReader<R>) -> Result<Option<i64>> {
        if let Some(f) = self.dec.ready.front() {
            return Ok(Some(f.pts()));
        }
        Ok(self.cursor.peek(reader)?.map(|p| p.pts))
    }
}
