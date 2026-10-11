//! Debounced folder watching shared by the host (direct edits) and the mirror frontend (local edits).
//! Every event only says "something happened at this path"; the receiver rescans those paths, so rename
//! pairing quirks (FSEvents reports unpaired name changes) do not matter here.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// Longest a batch is held back while events keep arriving.
const MAX_HOLD: Duration = Duration::from_secs(5);

pub struct PathWatcher {
    _watcher: RecommendedWatcher,
}

impl PathWatcher {
    /// Watches `root` recursively. `skip(rel)` filters paths (ignored names, the store folder);
    /// `on_batch` receives the debounced set of relative, `/`-separated paths. A path that is a
    /// parent of other pending paths is delivered as is: receivers scan it as a subtree.
    pub fn start(
        root: &Path,
        debounce: Duration,
        skip: impl Fn(&str) -> bool + Send + 'static,
        on_batch: impl Fn(Vec<String>) + Send + 'static,
    ) -> notify::Result<PathWatcher> {
        let canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let roots: Vec<PathBuf> = if canon == root { vec![canon.clone()] } else { vec![root.to_path_buf(), canon.clone()] };
        let (tx, rx) = mpsc::channel::<String>();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(ev) = res else { return };
            if matches!(ev.kind, EventKind::Access(_)) {
                return;
            }
            for p in ev.paths {
                let rel = roots.iter().find_map(|r| p.strip_prefix(r).ok());
                let Some(rel) = rel else { continue };
                let Some(rel) = rel.to_str() else { continue };
                let rel = rel.replace('\\', "/");
                if rel.is_empty() || skip(&rel) {
                    continue;
                }
                let _ = tx.send(rel);
            }
        })?;
        watcher.watch(&canon, RecursiveMode::Recursive)?;
        std::thread::Builder::new()
            .name("medialake-watch".into())
            .spawn(move || {
                let mut pending: HashSet<String> = HashSet::new();
                let mut first: Option<Instant> = None;
                loop {
                    match rx.recv_timeout(debounce) {
                        Ok(p) => {
                            pending.insert(p);
                            first.get_or_insert_with(Instant::now);
                            if first.is_some_and(|f| f.elapsed() < MAX_HOLD) {
                                continue;
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            if !pending.is_empty() {
                                on_batch(pending.drain().collect());
                            }
                            return;
                        }
                    }
                    if !pending.is_empty() {
                        first = None;
                        on_batch(pending.drain().collect());
                    }
                }
            })
            .map_err(notify::Error::io)?;
        Ok(PathWatcher { _watcher: watcher })
    }
}
