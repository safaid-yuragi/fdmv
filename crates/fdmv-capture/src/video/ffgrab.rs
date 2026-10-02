//! ffmpeg の画面取り込み（Windows の gdigrab、X11 の x11grab）。
//!
//! ffmpeg に BGRA の生フレームを標準出力へ出させて読む。フレームの大きさは ffmpeg のログから知る。

use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use libfdmv::ffmpeg::Ffmpeg;

use super::{Frame, FrameSlot, VideoCapture};

struct FfmpegGrab {
    child: Child,
    slot: Arc<FrameSlot>,
    error: Arc<Mutex<Option<String>>>,
    thread: Option<JoinHandle<()>>,
}

impl VideoCapture for FfmpegGrab {
    fn slot(&self) -> Arc<FrameSlot> {
        self.slot.clone()
    }

    fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }
}

impl Drop for FfmpegGrab {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// `grab_args` = 入力形式の指定（`-f gdigrab` など）、`input` = `-i` に渡すもの。
pub fn start(
    ff: &Ffmpeg,
    grab_args: &[&str],
    input: &str,
    region: Option<(i32, i32, u32, u32)>,
    fps: u32,
) -> Result<Box<dyn VideoCapture>> {
    let mut cmd = Command::new(&ff.ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "info", "-nostats", "-nostdin"])
        .args(grab_args)
        .args(["-framerate", &fps.to_string()]);
    if let Some((x, y, w, h)) = region {
        cmd.args(["-offset_x", &x.to_string(), "-offset_y", &y.to_string()])
            .args(["-video_size", &format!("{w}x{h}")]);
    }
    cmd.args(["-i", input, "-f", "rawvideo", "-pix_fmt", "bgra", "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().context("ffmpeg を起動できません")?;
    let stderr = child.stderr.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();

    // ログから出力の大きさを読む（"Stream #0:0: Video: rawvideo (BGRA / ...), bgra, 1920x1080, ..."）。
    let (size_tx, size_rx) = mpsc::channel::<std::result::Result<(u32, u32), String>>();
    std::thread::spawn(move || {
        let mut output = false;
        let mut log = Vec::new();
        let mut sent = false;
        for line in BufReader::new(stderr).lines().map_while(|l| l.ok()) {
            if line.starts_with("Output #0") {
                output = true;
            }
            if !sent
                && output
                && line.contains("Video:")
                && let Some(size) = parse_size(&line)
            {
                let _ = size_tx.send(Ok(size));
                sent = true;
            }
            if !sent {
                log.push(line);
            }
        }
        if !sent {
            let _ = size_tx.send(Err(log.join("\n")));
        }
    });
    let (w, h) = match size_rx.recv_timeout(Duration::from_secs(15)) {
        Ok(Ok(size)) => size,
        Ok(Err(log)) => {
            let _ = child.kill();
            bail!("画面を取り込めません（ffmpeg）: {}", log.trim());
        }
        Err(_) => {
            let _ = child.kill();
            bail!("画面の取り込みが始まりません（ffmpeg）");
        }
    };

    let slot = Arc::new(FrameSlot::default());
    let error: Arc<Mutex<Option<String>>> = Arc::default();
    let thread = {
        let (slot, error) = (slot.clone(), error.clone());
        std::thread::Builder::new()
            .name("fdmv-ffgrab".into())
            .spawn(move || {
                let mut buf = vec![0u8; w as usize * h as usize * 4];
                loop {
                    if stdout.read_exact(&mut buf).is_err() {
                        *error.lock().unwrap() = Some("画面の取り込みが止まりました".into());
                        break;
                    }
                    slot.put(Frame {
                        width: w,
                        height: h,
                        bgra: buf.clone(),
                    });
                }
            })?
    };
    Ok(Box::new(FfmpegGrab {
        child,
        slot,
        error,
        thread: Some(thread),
    }))
}

/// "…, bgra, 1920x1080 [SAR 1:1 DAR 16:9], …" から大きさを読む。
fn parse_size(line: &str) -> Option<(u32, u32)> {
    line.split([',', ' '])
        .filter_map(|t| {
            let (w, h) = t.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .find(|&(w, h): &(u32, u32)| w > 0 && h > 0)
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_size() {
        let l = "  Stream #0:0: Video: rawvideo (BGRA / 0x41524742), bgra(pc, gbr/unknown/unknown, progressive), 1920x1080, q=2-31, 3981312 kb/s, 30 fps";
        assert_eq!(super::parse_size(l), Some((1920, 1080)));
        // "0x41524742" は大きさではない
        assert_eq!(
            super::parse_size("Video: rawvideo (BGRA / 0x41524742)"),
            None
        );
    }
}
