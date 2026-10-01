//! バックグラウンド処理: プロキシの作成（順番に 1 つずつ）と書き出し。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};

use eframe::egui;
use fdmv_edit::export::{Progress, export};
use fdmv_edit::{Id, Project, ProxyStore, Source};
use libfdmv::ffmpeg::Ffmpeg;

#[derive(Clone, Debug, PartialEq)]
pub enum ProxyState {
    Queued,
    Building(f32),
    Ready,
    Failed(String),
}

pub struct ProxyJobs {
    states: Arc<Mutex<HashMap<Id, ProxyState>>>,
    tx: Sender<Source>,
    cancel: Arc<AtomicBool>,
    /// 完了するたびに増える（プレビューの更新のきっかけ）。
    generation: Arc<Mutex<u64>>,
}

impl ProxyJobs {
    pub fn new(ff: Ffmpeg, store: ProxyStore, ctx: egui::Context) -> Self {
        let states: Arc<Mutex<HashMap<Id, ProxyState>>> = Arc::default();
        let generation: Arc<Mutex<u64>> = Arc::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel::<Source>();
        let (st, gen_, cn) = (states.clone(), generation.clone(), cancel.clone());
        std::thread::Builder::new()
            .name("fdmv-proxy".into())
            .spawn(move || {
                for src in rx {
                    if cn.load(Ordering::Relaxed) {
                        break;
                    }
                    let id = src.id;
                    let set = |s: ProxyState| {
                        st.lock().unwrap().insert(id, s);
                        ctx.request_repaint();
                    };
                    set(ProxyState::Building(0.0));
                    let mut last = 0.0f32;
                    let res = store.build(
                        &ff,
                        &src,
                        &mut |p| {
                            let p = p as f32;
                            if p - last >= 0.01 {
                                last = p;
                                set(ProxyState::Building(p));
                            }
                        },
                        &cn,
                    );
                    match res {
                        Ok(()) => set(ProxyState::Ready),
                        Err(e) => set(ProxyState::Failed(format!("{e:#}"))),
                    }
                    *gen_.lock().unwrap() += 1;
                    ctx.request_repaint();
                }
            })
            .expect("spawn proxy thread");
        ProxyJobs {
            states,
            tx,
            cancel,
            generation,
        }
    }

    /// プロキシが無ければ作成を予約する。
    pub fn ensure(&self, store: &ProxyStore, src: &Source) {
        let mut st = self.states.lock().unwrap();
        if store.is_ready(src) {
            st.insert(src.id, ProxyState::Ready);
            return;
        }
        if matches!(
            st.get(&src.id),
            Some(ProxyState::Queued | ProxyState::Building(_))
        ) {
            return;
        }
        st.insert(src.id, ProxyState::Queued);
        let _ = self.tx.send(src.clone());
    }

    pub fn state(&self, id: Id) -> Option<ProxyState> {
        self.states.lock().unwrap().get(&id).cloned()
    }

    pub fn generation(&self) -> u64 {
        *self.generation.lock().unwrap()
    }

    pub fn busy(&self) -> bool {
        self.states
            .lock()
            .unwrap()
            .values()
            .any(|s| matches!(s, ProxyState::Queued | ProxyState::Building(_)))
    }
}

impl Drop for ProxyJobs {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

#[derive(Default)]
pub struct ExportStatus {
    pub progress: Option<Progress>,
    /// 完了したら Some（成功ならファイルサイズ、失敗ならエラー）。
    pub done: Option<Result<u64, String>>,
}

pub struct ExportJob {
    pub output: PathBuf,
    pub status: Arc<Mutex<ExportStatus>>,
    cancel: Arc<AtomicBool>,
}

impl ExportJob {
    pub fn start(
        ff: Ffmpeg,
        project: Project,
        store: ProxyStore,
        output: PathBuf,
        ctx: egui::Context,
    ) -> Self {
        let status: Arc<Mutex<ExportStatus>> = Arc::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let (st, cn, out) = (status.clone(), cancel.clone(), output.clone());
        std::thread::Builder::new()
            .name("fdmv-export".into())
            .spawn(move || {
                let res = export(
                    &ff,
                    &project,
                    &store,
                    &out,
                    &mut |p| {
                        st.lock().unwrap().progress = Some(p);
                        ctx.request_repaint();
                    },
                    &cn,
                );
                st.lock().unwrap().done =
                    Some(res.map(|r| r.file_size).map_err(|e| format!("{e:#}")));
                ctx.request_repaint();
            })
            .expect("spawn export thread");
        ExportJob {
            output,
            status,
            cancel,
        }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}
