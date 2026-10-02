//! 録画中に使う映像エンコーダの選択。
//!
//! - **ハードウェア AV1**（NVIDIA RTX 40 以降の NVENC、Intel Arc の QSV、AMD RX 7000 以降の AMF、VA-API）:
//!   GPU で AV1 にするので CPU をほとんど使わず、停止後の変換も要らない。
//! - **ハードウェア HEVC / H.264**（上記より古い GPU、macOS の VideoToolbox）: GPU で一時ファイルに録り、
//!   停止後に CPU で AV1 に変換する（FDMV は AV1 しか入れられないため）。
//! - **ソフトウェア AV1**（SVT-AV1 など）: GPU が使えないときの最後の手段。CPU を多く使う。
//!
//! 一覧にあっても使えない（ドライバが無い、変換用のデコーダが無いなど）ことがあるので、
//! 小さな映像を実際にエンコードして確かめる。

use std::ffi::OsString;
use std::process::{Command, Stdio};
use std::sync::Mutex;

use anyhow::Result;
use libfdmv::ffmpeg::{AV1_ENCODERS, Ffmpeg, Quality, VideoEncodeOptions};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncoderKind {
    /// GPU で AV1。
    HardwareAv1,
    /// GPU で HEVC / H.264 に一時保存し、停止後に AV1 に変換する。
    HardwareIntermediate,
    /// CPU で AV1。
    SoftwareAv1,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveEncoder {
    /// ffmpeg のエンコーダ名。
    pub name: String,
    pub kind: EncoderKind,
}

impl LiveEncoder {
    pub fn is_av1(&self) -> bool {
        self.kind != EncoderKind::HardwareIntermediate
    }

    /// 停止後に AV1 への変換が必ず要るか。
    pub fn needs_conversion(&self) -> bool {
        !self.is_av1()
    }

    pub fn label(&self) -> String {
        let vendor = match self.name.rsplit('_').next() {
            Some("nvenc") => "NVIDIA",
            Some("qsv") => "Intel",
            Some("amf") => "AMD",
            Some("vaapi") => "VA-API",
            Some("videotoolbox") => "Apple",
            _ => "",
        };
        let codec = if self.name.starts_with("hevc") {
            "HEVC"
        } else if self.name.starts_with("h264") {
            "H.264"
        } else {
            "AV1"
        };
        match self.kind {
            EncoderKind::HardwareAv1 => format!("GPU（{vendor} {codec}）"),
            EncoderKind::HardwareIntermediate => {
                format!("GPU（{vendor} {codec}）→ 停止後に AV1 へ変換")
            }
            EncoderKind::SoftwareAv1 => format!("CPU（{}）", self.name),
        }
    }

    /// 出力ファイルの拡張子（ffmpeg の形式名）。
    pub fn container(&self) -> &'static str {
        if self.is_av1() { "ivf" } else { "matroska" }
    }

    /// エンコーダの前に ffmpeg に渡す、入力側の引数（VA-API の装置）。
    pub fn input_args(&self) -> Vec<OsString> {
        if self.name.ends_with("_vaapi") {
            vec!["-vaapi_device".into(), vaapi_device().into()]
        } else {
            Vec::new()
        }
    }

    /// 色変換などのフィルタの後に付けるもの（VA-API は GPU にアップロードする）。
    pub fn filter_suffix(&self) -> &'static str {
        if self.name.ends_with("_vaapi") {
            ",format=nv12,hwupload"
        } else {
            ""
        }
    }

    /// 出力側の引数（エンコーダと画質）。`intermediate_quality` なら変換前提の高画質にする。
    pub fn output_args(
        &self,
        ff: &Ffmpeg,
        quality: Quality,
        intermediate: bool,
    ) -> Result<Vec<OsString>> {
        if self.kind == EncoderKind::SoftwareAv1 {
            let (crf, preset) = if intermediate {
                (16, 12)
            } else {
                match quality {
                    Quality::Best => (20, 10),
                    Quality::High => (26, 11),
                    Quality::Standard => (32, 12),
                    Quality::Small => (38, 12),
                }
            };
            let mut o = VideoEncodeOptions {
                encoder: Some(self.name.clone()),
                crf,
                ..Default::default()
            };
            match self.name.as_str() {
                "libsvtav1" => o.preset = Some(preset),
                "libaom-av1" => {
                    o.preset = Some(10);
                    o.extra_args = vec!["-usage".into(), "realtime".into()];
                }
                _ => o.preset = Some(10),
            }
            return Ok(ff.av1_output_args(&o)?.1);
        }
        // ハードウェア: 品質の数値（小さいほど高画質）。HEVC / H.264 は 0–51、AV1 は 0–255 の目盛り。
        let q51: u32 = if intermediate || self.kind == EncoderKind::HardwareIntermediate {
            18
        } else {
            match quality {
                Quality::Best => 22,
                Quality::High => 26,
                Quality::Standard => 30,
                Quality::Small => 36,
            }
        };
        let av1 = self.kind == EncoderKind::HardwareAv1;
        let q = |v: u32| {
            (if av1 && !self.name.ends_with("_nvenc") {
                v * 5
            } else {
                v
            })
            .to_string()
        };
        let mut a: Vec<String> = vec!["-c:v".into(), self.name.clone()];
        let vendor = self.name.rsplit('_').next().unwrap_or("");
        match vendor {
            "nvenc" => {
                a.extend(
                    [
                        "-pix_fmt", "yuv420p", "-preset", "p4", "-tune", "hq", "-rc", "vbr",
                    ]
                    .map(String::from),
                );
                a.extend(["-cq".into(), q(q51), "-b:v".into(), "0".into()]);
            }
            "qsv" => {
                a.extend(["-pix_fmt", "nv12", "-preset", "faster"].map(String::from));
                a.extend(["-global_quality".into(), q(q51)]);
            }
            "amf" => {
                a.extend(
                    ["-pix_fmt", "nv12", "-usage", "lowlatency", "-rc", "cqp"].map(String::from),
                );
                a.extend(["-qp_i".into(), q(q51), "-qp_p".into(), q(q51)]);
            }
            "vaapi" => {
                a.extend(["-rc_mode".into(), "CQP".into(), "-qp".into(), q(q51)]);
            }
            "videotoolbox" => {
                // 0–100（大きいほど高画質）
                let vt = 100u32.saturating_sub(q51 * 2).clamp(30, 90);
                a.extend(
                    ["-pix_fmt", "yuv420p", "-realtime", "1", "-allow_sw", "0"].map(String::from),
                );
                a.extend(["-q:v".into(), vt.to_string()]);
            }
            _ => {}
        }
        Ok(a.into_iter().map(OsString::from).collect())
    }
}

/// GPU の候補（優先順）。
const HARDWARE_AV1: &[&str] = &["av1_nvenc", "av1_qsv", "av1_amf", "av1_vaapi"];
const HARDWARE_INTERMEDIATE: &[&str] = &[
    "hevc_nvenc",
    "hevc_qsv",
    "hevc_amf",
    "hevc_videotoolbox",
    "hevc_vaapi",
    "h264_nvenc",
    "h264_qsv",
    "h264_amf",
    "h264_videotoolbox",
    "h264_vaapi",
];

fn vaapi_device() -> String {
    std::env::var("FDMV_VAAPI_DEVICE").unwrap_or_else(|_| "/dev/dri/renderD128".into())
}

static PROBED: Mutex<Option<Vec<LiveEncoder>>> = Mutex::new(None);

/// 使えるエンコーダの一覧（おすすめ順）。結果は覚えておく。
pub fn available(ff: &Ffmpeg) -> Vec<LiveEncoder> {
    if let Some(v) = PROBED.lock().unwrap().as_ref() {
        return v.clone();
    }
    let listed = ff.encoders().unwrap_or_default();
    let has = |n: &str| listed.iter().any(|e| e == n);
    let mut v = Vec::new();
    for (names, kind) in [
        (HARDWARE_AV1, EncoderKind::HardwareAv1),
        (HARDWARE_INTERMEDIATE, EncoderKind::HardwareIntermediate),
    ] {
        for n in names.iter().filter(|n| has(n)) {
            let e = LiveEncoder {
                name: n.to_string(),
                kind,
            };
            // 同じ GPU の HEVC があれば H.264 は出さない。
            if n.starts_with("h264")
                && let Some(vendor) = n.strip_prefix("h264_")
                && v.iter()
                    .any(|x: &LiveEncoder| x.name == format!("hevc_{vendor}"))
            {
                continue;
            }
            if works(ff, &e) {
                v.push(e);
            }
        }
    }
    for n in AV1_ENCODERS.iter().filter(|n| has(n)) {
        v.push(LiveEncoder {
            name: n.to_string(),
            kind: EncoderKind::SoftwareAv1,
        });
    }
    *PROBED.lock().unwrap() = Some(v.clone());
    v
}

/// 小さな映像を実際にエンコードしてみる。停止後に変換する形式は、この ffmpeg で読み戻せることも確かめる
/// （例: Fedora の ffmpeg-free には HEVC のデコーダが無い）。
fn works(ff: &Ffmpeg, e: &LiveEncoder) -> bool {
    let Ok(out) = e.output_args(ff, Quality::High, false) else {
        return false;
    };
    let probe =
        std::env::temp_dir().join(format!("fdmv-probe-{}-{}.mkv", std::process::id(), e.name));
    let mut cmd = quiet(ff);
    cmd.args(e.input_args()).args([
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=320x240:rate=30",
        "-frames:v",
        "10",
    ]);
    let vf = format!("format=yuv420p{}", e.filter_suffix());
    cmd.args(["-vf", &vf]).args(out);
    if e.needs_conversion() {
        cmd.args(["-f", "matroska", "-y"]).arg(&probe);
    } else {
        cmd.args(["-f", "null", "-"]);
    }
    let mut ok = cmd.status().is_ok_and(|s| s.success());
    if ok && e.needs_conversion() {
        ok = quiet(ff)
            .args(["-xerror", "-i"])
            .arg(&probe)
            .args(["-f", "null", "-"])
            .status()
            .is_ok_and(|s| s.success());
    }
    let _ = std::fs::remove_file(&probe);
    ok
}

fn quiet(ff: &Ffmpeg) -> Command {
    let mut cmd = Command::new(&ff.ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
}

/// 名前で選ぶ（None ならおすすめ＝一覧の先頭）。
pub fn pick(ff: &Ffmpeg, name: Option<&str>) -> Result<LiveEncoder> {
    let list = available(ff);
    match name {
        Some(n) => list
            .into_iter()
            .find(|e| e.name == n)
            .ok_or_else(|| anyhow::anyhow!("エンコーダ {n} は使えません")),
        None => list.into_iter().next().ok_or_else(|| {
            anyhow::anyhow!(
                "使える映像エンコーダがありません（ffmpeg に AV1 エンコーダが必要です）"
            )
        }),
    }
}
