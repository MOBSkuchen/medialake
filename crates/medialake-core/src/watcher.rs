//! File watching: `notify` events are debounced per file and fed to the [`Indexer`].
//! A file that is still being written is re-indexed once, `debounce` after its last event.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use crate::config::{Config, IndexedDir};
use crate::extract;
use crate::indexer::Indexer;

use medialake_sync::watch::{FsChange as Change, normalize};

pub struct FileWatcher {
    watcher: std::sync::Mutex<RecommendedWatcher>,
    watched: std::sync::Mutex<Vec<IndexedDir>>,
}

impl FileWatcher {
    pub fn start(indexer: Arc<Indexer>, cfg: Arc<RwLock<Config>>) -> notify::Result<Arc<FileWatcher>> {
        let (tx, mut rx) = mpsc::unbounded_channel::<Change>();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(ev) = res else { return };
            tracing::debug!("fs event {:?} {:?}", ev.kind, ev.paths);
            // rename pairing and the FSEvents special case live in medialake-sync, shared with sync
            for ch in normalize(ev) {
                let _ = tx.send(ch);
            }
        })?;
        let fw = Arc::new(FileWatcher { watcher: std::sync::Mutex::new(watcher), watched: std::sync::Mutex::new(vec![]) });

        // debouncer: path -> due time; ticks every 250 ms
        tokio::spawn(async move {
            let mut due: HashMap<PathBuf, Instant> = HashMap::new();
            let mut pending_from: Option<(PathBuf, Instant)> = None;
            let mut tick = tokio::time::interval(Duration::from_millis(250));
            loop {
                tokio::select! {
                    Some(ch) = rx.recv() => {
                        let debounce = Duration::from_millis(cfg.read().unwrap().debounce_ms);
                        match ch {
                            Change::Touched(p) => {
                                if !extract::is_ignored(&p) {
                                    due.insert(p, Instant::now() + debounce);
                                }
                            }
                            Change::Removed(p) => {
                                due.remove(&p);
                                indexer.remove_path(&p);
                            }
                            Change::RenamedFrom(p) => {
                                if let Some((old, _)) = pending_from.take() {
                                    due.remove(&old);
                                    indexer.remove_path(&old);
                                }
                                pending_from = Some((p, Instant::now()));
                            }
                            Change::RenamedTo(to) => match pending_from.take() {
                                Some((from, _)) => {
                                    due.remove(&from);
                                    indexer.rename_path(&from, &to);
                                }
                                None => {
                                    due.insert(to, Instant::now() + debounce);
                                }
                            },
                            Change::Renamed(from, to) => {
                                due.remove(&from);
                                indexer.rename_path(&from, &to);
                            }
                        }
                    }
                    _ = tick.tick() => {
                        let now = Instant::now();
                        if let Some((from, at)) = &pending_from {
                            if now.duration_since(*at) > Duration::from_millis(600) {
                                // moved out of the watched tree
                                indexer.remove_path(from);
                                pending_from = None;
                            }
                        }
                        let ready: Vec<PathBuf> = due.iter().filter(|(_, t)| **t <= now).map(|(p, _)| p.clone()).collect();
                        for p in ready {
                            due.remove(&p);
                            indexer.enqueue(p);
                        }
                    }
                }
            }
        });
        Ok(fw)
    }

    /// Replaces the set of watched roots.
    pub fn set_dirs(&self, dirs: &[IndexedDir]) {
        let mut w = self.watcher.lock().unwrap();
        let mut cur = self.watched.lock().unwrap();
        for d in cur.iter() {
            let _ = w.unwatch(&d.path);
        }
        cur.clear();
        for d in dirs {
            let mode = if d.recursive { RecursiveMode::Recursive } else { RecursiveMode::NonRecursive };
            match w.watch(Path::new(&d.path), mode) {
                Ok(()) => cur.push(d.clone()),
                Err(e) => tracing::warn!("cannot watch {}: {e}", d.path.display()),
            }
        }
    }
}
