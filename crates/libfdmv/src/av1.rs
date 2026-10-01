//! AV1 の OBU（低オーバーヘッド形式）を最小限だけ解析する。キーフレーム判定に使う。

use crate::error::{Result, malformed};

pub const OBU_SEQUENCE_HEADER: u8 = 1;
pub const OBU_TEMPORAL_DELIMITER: u8 = 2;
pub const OBU_FRAME_HEADER: u8 = 3;
pub const OBU_FRAME: u8 = 6;

pub struct Obu<'a> {
    pub obu_type: u8,
    /// OBU ヘッダを含む OBU 全体。
    pub raw: &'a [u8],
    pub payload: &'a [u8],
}

fn read_leb128(data: &[u8], pos: &mut usize) -> Result<u64> {
    let mut value = 0u64;
    for i in 0..8 {
        let Some(&b) = data.get(*pos) else {
            return malformed("truncated leb128");
        };
        *pos += 1;
        value |= ((b & 0x7f) as u64) << (i * 7);
        if b & 0x80 == 0 {
            return Ok(value);
        }
    }
    malformed("leb128 too long")
}

pub fn parse_obus(tu: &[u8]) -> Result<Vec<Obu<'_>>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < tu.len() {
        let start = pos;
        let header = tu[pos];
        if header & 0x80 != 0 {
            return malformed("OBU forbidden bit set");
        }
        let obu_type = (header >> 3) & 0x0f;
        let has_extension = header & 0x04 != 0;
        let has_size = header & 0x02 != 0;
        pos += 1 + has_extension as usize;
        let size = if has_size {
            read_leb128(tu, &mut pos)? as usize
        } else {
            tu.len().saturating_sub(pos)
        };
        if pos > tu.len() || tu.len() - pos < size {
            return malformed("truncated OBU");
        }
        out.push(Obu {
            obu_type,
            raw: &tu[start..pos + size],
            payload: &tu[pos..pos + size],
        });
        pos += size;
    }
    Ok(out)
}

/// Temporal Unit の解析結果。
pub struct TemporalUnitInfo {
    pub sequence_header: Option<Vec<u8>>,
    pub is_keyframe: bool,
}

/// Temporal Unit を順に解析する。Sequence Header の内容を次の TU に引き継ぐ。
#[derive(Default)]
pub struct Av1Analyzer {
    reduced_still_picture_header: Option<bool>,
}

impl Av1Analyzer {
    pub fn analyze(&mut self, tu: &[u8]) -> Result<TemporalUnitInfo> {
        let mut sequence_header = None;
        let mut first_frame: Option<(bool, u8, bool)> = None;
        for obu in parse_obus(tu)? {
            match obu.obu_type {
                OBU_SEQUENCE_HEADER => {
                    // seq_profile(3) still_picture(1) reduced_still_picture_header(1)
                    let Some(&b) = obu.payload.first() else {
                        return malformed("empty sequence header");
                    };
                    self.reduced_still_picture_header = Some(b & 0x08 != 0);
                    sequence_header = Some(obu.raw.to_vec());
                }
                OBU_FRAME_HEADER | OBU_FRAME if first_frame.is_none() => {
                    first_frame = Some(self.frame_header_bits(obu.payload)?);
                }
                _ => {}
            }
        }
        // (show_existing_frame, frame_type, show_frame)
        let is_keyframe = sequence_header.is_some()
            && matches!(first_frame, Some((false, 0 /* KEY_FRAME */, true)));
        Ok(TemporalUnitInfo {
            sequence_header,
            is_keyframe,
        })
    }

    fn frame_header_bits(&self, payload: &[u8]) -> Result<(bool, u8, bool)> {
        match self.reduced_still_picture_header {
            None => malformed("frame header before sequence header"),
            Some(true) => Ok((false, 0, true)),
            Some(false) => {
                let Some(&b) = payload.first() else {
                    return malformed("empty frame header");
                };
                let show_existing_frame = b & 0x80 != 0;
                if show_existing_frame {
                    return Ok((true, 0, true));
                }
                Ok((false, (b >> 5) & 0x03, b & 0x10 != 0))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_keyframe() {
        // TD, Sequence Header (profile 0, 非 reduced), Frame (show_existing=0, KEY, show_frame=1)
        let tu = [0x12, 0x00, 0x0a, 0x01, 0x00, 0x32, 0x01, 0b0001_0000];
        let mut a = Av1Analyzer::default();
        let info = a.analyze(&tu).unwrap();
        assert!(info.is_keyframe);
        assert_eq!(info.sequence_header.unwrap(), vec![0x0a, 0x01, 0x00]);

        // INTER_FRAME (frame_type = 1)
        let tu = [0x12, 0x00, 0x32, 0x01, 0b0011_0000];
        assert!(!a.analyze(&tu).unwrap().is_keyframe);
    }
}
