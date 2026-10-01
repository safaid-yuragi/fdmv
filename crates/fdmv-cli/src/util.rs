use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use libfdmv::Directory;
use libfdmv::pack::SegmentSource;
use libfdmv::time::parse_time;

/// `PATH` または `PATH@TIME` を解析する。`@` 以降が時刻として読めなければ全体をパスとみなす。
pub fn parse_source(spec: &str) -> Result<SegmentSource> {
    if let Some((path, t)) = spec.rsplit_once('@')
        && !path.is_empty()
        && let Ok(start) = parse_time(t)
    {
        return Ok(SegmentSource::new(path, start));
    }
    if spec.is_empty() {
        bail!("empty source");
    }
    Ok(SegmentSource::new(spec, 0.0))
}

pub fn split_kv<'a>(s: &'a str, sep: char, what: &str) -> Result<(&'a str, &'a str)> {
    s.split_once(sep)
        .ok_or_else(|| anyhow!("{what} {s:?} must be in the form A{sep}B"))
}

pub fn parse_meta(items: &[String]) -> Result<Vec<(String, String)>> {
    items
        .iter()
        .map(|s| {
            let (k, v) = split_kv(s, '=', "metadata")?;
            if !libfdmv::model::is_valid_meta_key(k) {
                bail!("invalid metadata key {k:?} (use lowercase ASCII, digits, '_', '-', '.')");
            }
            Ok((k.to_owned(), v.to_owned()))
        })
        .collect()
}

pub fn chain_id(dir: &Directory, name: &str) -> Result<u16> {
    dir.chain_by_name(name).map(|s| s.id).ok_or_else(|| {
        let names: Vec<String> = dir.chains().map(|c| format!("{:?}", c.name)).collect();
        anyhow!("no chain named {name:?} (available: {})", names.join(", "))
    })
}

pub fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

pub fn ffmpeg(quiet: bool) -> Result<libfdmv::ffmpeg::Ffmpeg> {
    let mut ff = libfdmv::ffmpeg::Ffmpeg::locate()?;
    ff.verbose = !quiet && std::io::stderr().is_terminal();
    Ok(ff)
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// 端末の場合だけ進捗を表示する。
pub struct Progress {
    enabled: bool,
    label: String,
    last: i32,
}

impl Progress {
    pub fn new(label: &str) -> Self {
        Progress {
            enabled: std::io::stderr().is_terminal(),
            label: label.to_owned(),
            last: -1,
        }
    }
    pub fn update(&mut self, done: f64, total: f64) {
        if !self.enabled || total <= 0.0 {
            return;
        }
        let pct = (done / total * 100.0).clamp(0.0, 100.0) as i32;
        if pct != self.last {
            self.last = pct;
            eprint!("\r{}: {pct:3}%", self.label);
        }
    }
    pub fn finish(&mut self) {
        if self.enabled && self.last >= 0 {
            eprintln!();
        }
    }
}

pub fn temp_path(dir: &tempfile::TempDir, name: &str) -> PathBuf {
    dir.path().join(name)
}

pub fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(p) = path.parent()
        && !p.as_os_str().is_empty()
        && !p.exists()
    {
        std::fs::create_dir_all(p).with_context(|| format!("creating {}", p.display()))?;
    }
    Ok(())
}
