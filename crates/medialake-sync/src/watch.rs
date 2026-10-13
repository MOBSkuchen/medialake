//! Debounced folder watching shared by the host (direct edits) and the mirror frontend (local edits).
//! Every event only says "something happened at this path"; the receiver rescans those paths, so rename
//! pairing quirks (FSEvents reports unpaired name changes) do not matter here.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use notify::event::{ModifyKind, RenameMode};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// What happened to a path, after the platform differences of `notify` are smoothed over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsChange {
    /// Created, written or its metadata changed.
    Touched(PathBuf),
    Removed(PathBuf),
    /// First half of a rename whose halves arrive separately (inotify, ReadDirectoryChanges).
    RenamedFrom(PathBuf),
    RenamedTo(PathBuf),
    /// A rename delivered as one event.
    Renamed(PathBuf, PathBuf),
}

impl FsChange {
    pub fn paths(&self) -> Vec<&Path> {
        match self {
            FsChange::Touched(p) | FsChange::Removed(p) | FsChange::RenamedFrom(p) | FsChange::RenamedTo(p) => vec![p],
            FsChange::Renamed(a, b) => vec![a, b],
        }
    }
}

/// The one place that knows how `notify` reports changes. Used by the host and the mirror frontend
/// (through [`PathWatcher`]) and by the search indexer's watcher, so the three cannot drift.
///
/// FSEvents reports a rename as an unpaired name change on each path: the one that still exists is
/// the new name, the missing one the old name.
pub fn normalize(ev: notify::Event) -> Vec<FsChange> {
    let mut out = vec![];
    let mut paths = ev.paths.into_iter();
    match ev.kind {
        EventKind::Create(_) | EventKind::Modify(ModifyKind::Data(_)) | EventKind::Modify(ModifyKind::Any) | EventKind::Modify(ModifyKind::Metadata(_)) => {
            out.extend(paths.map(FsChange::Touched));
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            if let (Some(a), Some(b)) = (paths.next(), paths.next()) {
                out.push(FsChange::Renamed(a, b));
            }
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => out.extend(paths.map(FsChange::RenamedFrom)),
        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => out.extend(paths.map(FsChange::RenamedTo)),
        EventKind::Modify(ModifyKind::Name(_)) => {
            for p in paths {
                #[cfg(target_os = "macos")]
                out.push(if p.symlink_metadata().is_ok() { FsChange::RenamedTo(p) } else { FsChange::RenamedFrom(p) });
                #[cfg(not(target_os = "macos"))]
                out.push(FsChange::Touched(p));
            }
        }
        EventKind::Remove(_) => out.extend(paths.map(FsChange::Removed)),
        _ => {}
    }
    out
}

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
            // every kind of change only says "look at this path again"
            for ch in normalize(ev) {
                for p in ch.paths() {
                    let rel = roots.iter().find_map(|r| p.strip_prefix(r).ok());
                    let Some(rel) = rel else { continue };
                    let Some(rel) = rel.to_str() else { continue };
                    let rel = rel.replace('\\', "/");
                    if rel.is_empty() || skip(&rel) {
                        continue;
                    }
                    let _ = tx.send(rel);
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, RemoveKind};

    fn ev(kind: EventKind, paths: &[&str]) -> notify::Event {
        let mut e = notify::Event::new(kind);
        for p in paths {
            e = e.add_path(PathBuf::from(p));
        }
        e
    }

    #[test]
    fn normalises_notify_events() {
        assert_eq!(normalize(ev(EventKind::Create(CreateKind::File), &["/a"])), vec![FsChange::Touched("/a".into())]);
        assert_eq!(normalize(ev(EventKind::Remove(RemoveKind::File), &["/a"])), vec![FsChange::Removed("/a".into())]);
        assert_eq!(
            normalize(ev(EventKind::Modify(ModifyKind::Name(RenameMode::Both)), &["/a", "/b"])),
            vec![FsChange::Renamed("/a".into(), "/b".into())]
        );
        assert_eq!(
            normalize(ev(EventKind::Modify(ModifyKind::Name(RenameMode::From)), &["/a"])),
            vec![FsChange::RenamedFrom("/a".into())]
        );
        assert_eq!(normalize(ev(EventKind::Modify(ModifyKind::Name(RenameMode::To)), &["/b"])), vec![FsChange::RenamedTo("/b".into())]);
        // access events are noise
        assert!(normalize(ev(EventKind::Access(notify::event::AccessKind::Read), &["/a"])).is_empty());
        // an unpaired rename event: the missing path is the old name, an existing one the new name
        let missing = normalize(ev(EventKind::Modify(ModifyKind::Name(RenameMode::Any)), &["/definitely/not/here"]));
        if cfg!(target_os = "macos") {
            assert_eq!(missing, vec![FsChange::RenamedFrom("/definitely/not/here".into())]);
        } else {
            assert_eq!(missing, vec![FsChange::Touched("/definitely/not/here".into())]);
        }
    }
}
