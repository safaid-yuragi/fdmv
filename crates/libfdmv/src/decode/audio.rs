use std::ffi::c_int;
use std::ptr::NonNull;

use opusic_sys as sys;

use crate::error::{Error, Result, malformed};
use crate::format::{OPUS_MAX_FRAME, OPUS_SAMPLE_RATE};
use crate::model::{Packet, Segment, StreamEntry};
use crate::opus::OpusHead;

/// libopus のデコーダ（48 kHz、モノラル／ステレオ）。
pub struct OpusDecoder {
    ptr: NonNull<sys::OpusDecoder>,
    channels: usize,
}

// libopus のデコーダ状態はスレッドに紐付かない。
unsafe impl Send for OpusDecoder {}

impl OpusDecoder {
    pub fn new(channels: u8) -> Result<Self> {
        let mut err: c_int = 0;
        let ptr = unsafe {
            sys::opus_decoder_create(OPUS_SAMPLE_RATE as i32, channels as c_int, &mut err)
        };
        match NonNull::new(ptr) {
            Some(ptr) if err == sys::OPUS_OK => Ok(OpusDecoder {
                ptr,
                channels: channels as usize,
            }),
            _ => Err(Error::Decoder(format!(
                "opus_decoder_create failed ({err})"
            ))),
        }
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Q7.8 形式の dB でゲインを設定する（OpusHead の output_gain）。
    pub fn set_gain(&mut self, q8: i16) -> Result<()> {
        let r = unsafe {
            sys::opus_decoder_ctl(self.ptr.as_ptr(), sys::OPUS_SET_GAIN_REQUEST, q8 as c_int)
        };
        if r != sys::OPUS_OK {
            return Err(Error::Decoder(format!("OPUS_SET_GAIN failed ({r})")));
        }
        Ok(())
    }

    pub fn reset(&mut self) {
        unsafe { sys::opus_decoder_ctl(self.ptr.as_ptr(), sys::OPUS_RESET_STATE) };
    }

    /// パケットをデコードし、`out` にインターリーブで書く。チャンネルあたりのサンプル数を返す。
    pub fn decode(&mut self, packet: &[u8], out: &mut Vec<f32>) -> Result<usize> {
        out.resize(OPUS_MAX_FRAME * self.channels, 0.0);
        let n = unsafe {
            sys::opus_decode_float(
                self.ptr.as_ptr(),
                packet.as_ptr(),
                packet.len() as i32,
                out.as_mut_ptr(),
                OPUS_MAX_FRAME as c_int,
                0,
            )
        };
        if n < 0 {
            return Err(Error::Decoder(format!("opus_decode_float failed ({n})")));
        }
        out.truncate(n as usize * self.channels);
        Ok(n as usize)
    }
}

impl Drop for OpusDecoder {
    fn drop(&mut self) {
        unsafe { sys::opus_decoder_destroy(self.ptr.as_ptr()) };
    }
}

/// デコード結果。`start` はタイムライン上の位置（48 kHz のサンプル単位）。
pub struct DecodedAudio<'a> {
    pub start: i64,
    pub channels: usize,
    /// インターリーブ。
    pub samples: &'a [f32],
}

impl DecodedAudio<'_> {
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels
    }
}

/// 1 本のチェーンのデコーダ。パケットをタイムライン上の PCM に変換し、セグメント外を切り捨てる。
pub struct ChainDecoder {
    dec: OpusDecoder,
    segments: Vec<Segment>,
    /// 今デコードしているセグメント。
    current: Option<usize>,
    /// 次の SEGMENT_START パケットが始めるセグメント。
    next: usize,
    buf: Vec<f32>,
}

impl ChainDecoder {
    pub fn new(entry: &StreamEntry) -> Result<Self> {
        let Some(chain) = entry.chain() else {
            return malformed(format!("stream {} is not a chain", entry.id));
        };
        let mut dec = OpusDecoder::new(chain.channels)?;
        if let Ok(head) = OpusHead::parse(&entry.codec_private)
            && head.output_gain != 0
        {
            dec.set_gain(head.output_gain)?;
        }
        Ok(ChainDecoder {
            dec,
            segments: chain.segments.clone(),
            current: None,
            next: 0,
            buf: Vec::new(),
        })
    }

    pub fn channels(&self) -> usize {
        self.dec.channels()
    }

    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// 先頭から読み直すときに呼ぶ。
    pub fn rewind(&mut self) {
        self.dec.reset();
        self.current = None;
        self.next = 0;
    }

    /// セグメント `seg` の途中（または先頭）から読み始めるときに呼ぶ。
    /// `at_segment_start` が true なら、次に来るパケットは SEGMENT_START のはず。
    pub fn begin_segment(&mut self, seg: usize, at_segment_start: bool) {
        self.dec.reset();
        if at_segment_start {
            self.current = None;
            self.next = seg;
        } else {
            self.current = Some(seg);
            self.next = seg + 1;
        }
    }

    /// パケットをデコードする。セグメントの範囲に入る部分がなければ None。
    pub fn decode(&mut self, pkt: &Packet) -> Result<Option<DecodedAudio<'_>>> {
        if pkt.is_segment_start() {
            self.dec.reset();
            self.current = Some(self.next);
            self.next += 1;
        }
        let Some(seg) = self.current.and_then(|i| self.segments.get(i)).copied() else {
            return malformed("chain packet does not belong to any segment");
        };
        let n = self.dec.decode(&pkt.data, &mut self.buf)? as i64;
        let ch = self.dec.channels();
        let lo = pkt.pts.max(seg.start);
        let hi = (pkt.pts + n).min(seg.end());
        if lo >= hi {
            return Ok(None);
        }
        let a = (lo - pkt.pts) as usize * ch;
        let b = (hi - pkt.pts) as usize * ch;
        Ok(Some(DecodedAudio {
            start: lo,
            channels: ch,
            samples: &self.buf[a..b],
        }))
    }
}
