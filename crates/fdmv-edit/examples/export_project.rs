//! プロジェクトを GUI なしで書き出す。
//!
//! ```sh
//! cargo run --release -p fdmv-edit --example export_project -- project.fdmvproj out.fdmv
//! ```

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use fdmv_edit::{Project, ProxyStore};
use libfdmv::ffmpeg::Ffmpeg;

fn main() -> anyhow::Result<()> {
    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    let [project, output] = args.as_slice() else {
        anyhow::bail!("usage: export_project <project.fdmvproj> <out.fdmv>");
    };
    let ff = Ffmpeg::locate()?;
    let p = Project::load(project)?;
    let proxies = ProxyStore::new(ProxyStore::default_dir())?;
    let mut last = String::new();
    let report = fdmv_edit::export::export(
        &ff,
        &p,
        &proxies,
        output,
        &mut |pr| {
            if pr.stage != last {
                if !last.is_empty() {
                    eprintln!();
                }
                last = pr.stage.clone();
            }
            eprint!("\r{}: {:3.0}%", pr.stage, pr.fraction * 100.0);
            let _ = std::io::stderr().flush();
        },
        &AtomicBool::new(false),
    )?;
    eprintln!();
    for w in &report.warnings {
        eprintln!("warning: {w}");
    }
    println!("wrote {} ({} bytes)", output.display(), report.file_size);
    Ok(())
}
