//! 元に戻す／やり直し。編集のたびにプロジェクト全体のスナップショットを保存する。

use std::time::{Duration, Instant};

use fdmv_edit::Project;

const LIMIT: usize = 200;
/// 同じ項目への連続した変更（スライダーのドラッグなど）は 1 つにまとめる。
const COALESCE: Duration = Duration::from_millis(1000);

#[derive(Default)]
pub struct History {
    undo: Vec<Project>,
    redo: Vec<Project>,
    last: Option<(String, Instant)>,
}

impl History {
    /// 変更前の状態を記録する。`key` が直前と同じで間隔が短ければ記録しない。
    pub fn record(&mut self, before: &Project, key: Option<&str>) {
        if let (Some(k), Some((lk, t))) = (key, &self.last)
            && k == lk
            && t.elapsed() < COALESCE
        {
            self.last = Some((k.to_owned(), Instant::now()));
            return;
        }
        self.undo.push(before.clone());
        if self.undo.len() > LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
        self.last = key.map(|k| (k.to_owned(), Instant::now()));
    }

    pub fn undo(&mut self, current: &Project) -> Option<Project> {
        let p = self.undo.pop()?;
        self.redo.push(current.clone());
        self.last = None;
        Some(p)
    }

    pub fn redo(&mut self, current: &Project) -> Option<Project> {
        let p = self.redo.pop()?;
        self.undo.push(current.clone());
        self.last = None;
        Some(p)
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn clear(&mut self) {
        *self = History::default();
    }
}
