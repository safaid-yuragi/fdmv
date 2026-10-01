//! 時刻文字列の解析と表示。

use crate::error::{Result, invalid};

/// `12.5` / `1:02.5` / `01:02:03.25` 形式の時刻を秒に変換する。
pub fn parse_time(s: &str) -> Result<f64> {
    let parts: Vec<&str> = s.trim().split(':').collect();
    if parts.is_empty() || parts.len() > 3 {
        return invalid(format!("invalid time {s:?}"));
    }
    let mut secs = 0.0;
    for (i, p) in parts.iter().enumerate() {
        let last = i == parts.len() - 1;
        let v: f64 = if last {
            p.parse().ok().filter(|v: &f64| v.is_finite() && *v >= 0.0)
        } else {
            p.parse::<u32>().ok().map(f64::from)
        }
        .ok_or_else(|| crate::Error::Invalid(format!("invalid time {s:?}")))?;
        if i > 0 && v >= 60.0 {
            return invalid(format!("invalid time {s:?}: field out of range"));
        }
        secs = secs * 60.0 + v;
    }
    Ok(secs)
}

/// 秒を `HH:MM:SS.mmm` 形式にする。
pub fn format_time(secs: f64) -> String {
    let neg = secs < 0.0;
    let ms = (secs.abs() * 1000.0).round() as u64;
    let (h, m, s, ms) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    format!("{}{h:02}:{m:02}:{s:02}.{ms:03}", if neg { "-" } else { "" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_formats() {
        assert_eq!(parse_time("12.5").unwrap(), 12.5);
        assert_eq!(parse_time("1:02.5").unwrap(), 62.5);
        assert_eq!(parse_time("01:02:03.25").unwrap(), 3723.25);
        assert!(parse_time("1:60").is_err());
        assert!(parse_time("abc").is_err());
        assert!(parse_time("-1").is_err());
    }

    #[test]
    fn format() {
        assert_eq!(format_time(3723.25), "01:02:03.250");
    }
}
