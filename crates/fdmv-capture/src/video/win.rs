//! Windows: 録画対象の一覧（画面全体・モニタ・ウィンドウ）。取り込み自体は ffmpeg の gdigrab。

use anyhow::Result;
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
};
use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;
use windows::core::BOOL;

use super::{TargetInfo, VideoTarget};
use crate::audio::wasapi::{process_name, visible_windows};

pub fn targets() -> Result<Vec<TargetInfo>> {
    let mut v = vec![TargetInfo {
        target: VideoTarget::Gdi {
            input: "desktop".into(),
            region: None,
        },
        label: "画面全体（すべてのモニター）".into(),
        app_key: None,
    }];
    for (i, (rect, primary)) in monitors().into_iter().enumerate() {
        let (w, h) = (
            (rect.right - rect.left) as u32,
            (rect.bottom - rect.top) as u32,
        );
        v.push(TargetInfo {
            target: VideoTarget::Gdi {
                input: "desktop".into(),
                region: Some((rect.left, rect.top, w, h)),
            },
            label: format!(
                "モニター {}（{w}×{h}{}）",
                i + 1,
                if primary { "、メイン" } else { "" }
            ),
            app_key: None,
        });
    }
    for (_, title, pid) in visible_windows() {
        if title == "Program Manager" {
            continue;
        }
        let app = process_name(pid).unwrap_or_default();
        v.push(TargetInfo {
            target: VideoTarget::Gdi {
                input: format!("title={title}"),
                region: None,
            },
            label: format!("ウィンドウ: {title}（{app}）"),
            app_key: Some(pid.to_string()),
        });
    }
    Ok(v)
}

fn monitors() -> Vec<(RECT, bool)> {
    unsafe extern "system" fn callback(m: HMONITOR, _: HDC, _: *mut RECT, lp: LPARAM) -> BOOL {
        let out = unsafe { &mut *(lp.0 as *mut Vec<(RECT, bool)>) };
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetMonitorInfoW(m, &mut info) }.as_bool() {
            out.push((info.rcMonitor, info.dwFlags & MONITORINFOF_PRIMARY != 0));
        }
        BOOL(1)
    }
    let mut out: Vec<(RECT, bool)> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(callback),
            LPARAM(&mut out as *mut _ as isize),
        );
    }
    out
}
