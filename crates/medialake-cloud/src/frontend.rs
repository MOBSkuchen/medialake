//! The sync engine's view of a Cloud Files sync root.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use medialake_cloudapi::{CloudApi, FileState, Identity, PlaceholderInfo, Pin};
use medialake_sync::member::frontend::{EntryStatus, Frontend, LocalItem, Mirror};
use medialake_sync::util::{mtime_ms, random_id, set_mtime_ms};

/// A file or folder as the sync browser lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowseEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime_ms: i64,
    /// `cloud` (placeholder, content not here), `cached` (content here, in sync), `dirty` (changed here,
    /// not uploaded yet), `local` (not known to the host yet) or `dir`.
    pub state: &'static str,
    pub pinned: bool,
}

/// A conversion that failed because something had the file open; tried again later.
struct Retry {
    rel: String,
    identity: Identity,
    size: u64,
    mtime: i64,
}

struct Inner {
    api: Arc<dyn CloudApi>,
    root: PathBuf,
    read_only: bool,
    retry: Mutex<Vec<Retry>>,
    /// Paths a pin or free is running for (the watcher reports a file again while it changes).
    busy: Mutex<HashSet<String>>,
}

pub struct CloudFrontend {
    inner: Arc<Inner>,
    tree: Mirror,
}

impl CloudFrontend {
    pub fn new(api: Arc<dyn CloudApi>, root: &Path, read_only: bool) -> CloudFrontend {
        CloudFrontend {
            inner: Arc::new(Inner { api, root: root.to_path_buf(), read_only, retry: Mutex::new(vec![]), busy: Mutex::new(HashSet::new()) }),
            tree: Mirror::new(root),
        }
    }

    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    /// The entries of a folder with what each costs on this computer.
    pub fn browse(&self, rel: &str) -> Result<Vec<BrowseEntry>> {
        let dir = self.inner.path(rel);
        let mut out = vec![];
        for e in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))?.flatten() {
            let Some(name) = e.file_name().to_str().map(str::to_string) else { continue };
            let path = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            if medialake_proto::sync::is_ignored(&path, &[]) {
                continue;
            }
            let Ok(md) = e.metadata() else { continue };
            let st = self.inner.api.state_light(&e.path()).unwrap_or_default();
            let state = if md.is_dir() {
                "dir"
            } else if !st.placeholder {
                "local"
            } else if !st.in_sync {
                "dirty"
            } else if st.dehydrated {
                "cloud"
            } else {
                "cached"
            };
            out.push(BrowseEntry {
                name,
                path,
                is_dir: md.is_dir(),
                size: if md.is_dir() { 0 } else { md.len() },
                mtime_ms: mtime_ms(&md),
                state,
                pinned: st.pin == Pin::Pinned,
            });
        }
        out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        Ok(out)
    }

    /// "Always keep on this device": marks `rel` (a file or a folder, `""` = everything) and fetches what is
    /// missing, in the background.
    pub fn pin(&self, rel: &str) -> Result<()> {
        let p = self.inner.path(rel);
        self.inner.api.set_pin(&p, Pin::Pinned).map_err(|e| anyhow::anyhow!("{e}"))?;
        self.inner.spawn_walk(rel.to_string());
        Ok(())
    }

    /// "Free up space": the content may go again (and goes now, for what is in sync).
    pub fn unpin(&self, rel: &str, free_now: bool) -> Result<()> {
        let p = self.inner.path(rel);
        self.inner.api.set_pin(&p, Pin::Unpinned).map_err(|e| anyhow::anyhow!("{e}"))?;
        if free_now {
            self.inner.spawn_walk(rel.to_string());
        }
        Ok(())
    }

    /// Looks at the pin marks of the whole tree once (after a start: pins set while the app was not running).
    pub fn reconcile_pins(&self) {
        self.inner.spawn_walk(String::new());
    }

    /// Tries again the conversions that failed because a file was open.
    fn retry_conversions(&self) {
        let pending: Vec<Retry> = std::mem::take(&mut *self.inner.retry.lock().unwrap());
        for r in pending {
            self.inner.convert(r);
        }
    }
}

impl Inner {
    fn path(&self, rel: &str) -> PathBuf {
        if rel.is_empty() { self.root.clone() } else { self.root.join(rel) }
    }

    /// True if `rel`, or the nearest folder above it that has a mark, is pinned.
    fn is_pinned(&self, rel: &str) -> bool {
        let mut cur = rel.to_string();
        loop {
            let st = self.api.state_light(&self.path(&cur)).unwrap_or_default();
            match st.pin {
                Pin::Pinned => return true,
                Pin::Unpinned => return false,
                _ => {}
            }
            match cur.rsplit_once('/') {
                Some((parent, _)) => cur = parent.to_string(),
                None if cur.is_empty() => return false,
                None => cur = String::new(),
            }
        }
    }

    /// What the pin mark of one file asks for.
    fn apply_file(&self, path: &Path, st: &FileState) {
        match (st.pin, st.dehydrated, st.in_sync) {
            (Pin::Pinned, true, _) => {
                if let Err(e) = self.api.hydrate(path) {
                    tracing::warn!("keeping {} on this device: {e}", path.display());
                }
            }
            (Pin::Unpinned, false, true) => {
                if let Err(e) = self.api.dehydrate(path) {
                    tracing::debug!("freeing {}: {e}", path.display());
                }
            }
            _ => {}
        }
    }

    /// Applies the marks of `rel` and, for a folder, everything below it.
    fn walk(&self, rel: &str) {
        let mut stack = vec![rel.to_string()];
        while let Some(r) = stack.pop() {
            let p = self.path(&r);
            let st = self.api.state_light(&p).unwrap_or_default();
            if !st.exists {
                continue;
            }
            if st.is_dir {
                let Ok(rd) = std::fs::read_dir(&p) else { continue };
                for e in rd.flatten() {
                    let Some(name) = e.file_name().to_str().map(str::to_string) else { continue };
                    let child = if r.is_empty() { name } else { format!("{r}/{name}") };
                    if !medialake_proto::sync::is_ignored(&child, &[]) {
                        stack.push(child);
                    }
                }
            } else {
                // the mark of a file that has none comes from its folder
                let mut st = st;
                if st.pin == Pin::Unspecified && self.is_pinned(&r) {
                    st.pin = Pin::Pinned;
                }
                self.apply_file(&p, &st);
            }
        }
    }

    fn spawn_walk(self: &Arc<Self>, rel: String) {
        let me = self.clone();
        if !me.busy.lock().unwrap().insert(rel.clone()) {
            return;
        }
        let _ = std::thread::Builder::new().name("medialake-cloud-pin".into()).spawn(move || {
            me.walk(&rel);
            me.busy.lock().unwrap().remove(&rel);
        });
    }

    fn convert(&self, r: Retry) {
        let path = self.path(&r.rel);
        // only what is still the file that was pushed: a newer edit is the next round's business
        match std::fs::metadata(&path) {
            Ok(m) if m.len() == r.size && mtime_ms(&m) == r.mtime => {}
            _ => return,
        }
        if let Err(e) = self.api.mark_synced(&path, &r.identity) {
            tracing::debug!("{}: {e}; tried again later", r.rel);
            self.retry.lock().unwrap().push(r);
        }
    }
}

/// What `watch` hands back: dropping it stops the file watcher and the pin worker.
struct WatchGuard {
    _watcher: Option<Box<dyn std::any::Any + Send>>,
    _stop: mpsc::Sender<()>,
}

impl Frontend for CloudFrontend {
    fn check_root(&self) -> Result<()> {
        self.tree.check_root()
    }

    fn scan(&self, prefix: &str, ignore: &[String]) -> Result<Vec<LocalItem>> {
        self.retry_conversions();
        self.tree.scan(prefix, ignore)
    }

    /// Reading a placeholder makes the platform fetch it through the provider.
    fn local_path(&self, rel: &str) -> PathBuf {
        self.inner.path(rel)
    }

    fn meta_path(&self, rel: &str) -> PathBuf {
        self.inner.path(rel)
    }

    fn lazy(&self) -> bool {
        true
    }

    fn place_placeholder(&self, rel: &str, size: u64, mtime: i64, _exec: bool, hash: &str, rev: u64) -> Result<bool> {
        // a pinned file (or one in a pinned folder) is kept on this device: the engine downloads it
        if self.inner.is_pinned(rel) {
            return Ok(false);
        }
        let path = self.inner.path(rel);
        let info = PlaceholderInfo {
            is_dir: false,
            size,
            mtime_ms: mtime,
            read_only: self.inner.read_only,
            identity: Some(Identity { rev, hash: hash.to_string() }),
            in_sync: true,
        };
        let st = self.inner.api.state(&path).map_err(|e| anyhow::anyhow!("{e}"))?;
        if st.exists {
            if st.is_dir {
                bail!("a folder is in the way at {}", path.display());
            }
            if st.placeholder {
                // new content on the host: the placeholder takes it and lets go of the old
                self.inner.api.update_placeholder(&path, &info, true).map_err(|e| anyhow::anyhow!("{e}"))?;
                return Ok(true);
            }
            // a plain file that was clean: replaced by the placeholder
            std::fs::remove_file(&path).with_context(|| format!("replacing {}", path.display()))?;
        }
        let parent = path.parent().context("a file has a folder")?;
        std::fs::create_dir_all(parent)?;
        let name = path.file_name().and_then(|n| n.to_str()).context("file name")?;
        self.inner.api.create_placeholder(parent, name, &info).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(true)
    }

    fn content_hash(&self, rel: &str) -> Option<String> {
        let st = self.inner.api.state(&self.inner.path(rel)).ok()?;
        if st.placeholder && st.in_sync && !st.is_dir { st.identity.map(|i| i.hash) } else { None }
    }

    fn note_content(&self, rel: &str, hash: &str, rev: u64, size: u64, mtime: i64) {
        self.inner.convert(Retry { rel: rel.to_string(), identity: Identity { rev, hash: hash.to_string() }, size, mtime });
    }

    fn place_file(&self, rel: &str, staged: &Path, mtime: i64, _exec: bool) -> Result<()> {
        let dest = self.inner.path(rel);
        if dest.is_dir() {
            bail!("a folder is in the way at {}", dest.display());
        }
        if let Some(p) = dest.parent() {
            std::fs::create_dir_all(p)?;
        }
        let tmp = dest.with_file_name(format!(".{}.medialake.part", random_id()));
        if std::fs::rename(staged, &tmp).is_err() {
            std::fs::copy(staged, &tmp).with_context(|| format!("copying into {}", tmp.display()))?;
            let _ = std::fs::remove_file(staged);
        }
        if mtime > 0 {
            set_mtime_ms(&tmp, mtime)?;
        }
        std::fs::rename(&tmp, &dest).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
        // `note_content` follows and turns it into an in-sync placeholder
        Ok(())
    }

    fn make_dir(&self, rel: &str) -> Result<()> {
        let path = self.inner.path(rel);
        if path.is_dir() {
            return Ok(());
        }
        if let (Some(parent), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) {
            if parent.is_dir() {
                let info = PlaceholderInfo { is_dir: true, size: 0, mtime_ms: 0, read_only: false, identity: None, in_sync: true };
                if self.inner.api.create_placeholder(parent, name, &info).is_ok() {
                    return Ok(());
                }
            }
        }
        std::fs::create_dir_all(&path)?;
        Ok(())
    }

    fn remove(&self, rel: &str, is_dir: bool) -> Result<bool> {
        self.tree.remove(rel, is_dir)
    }

    fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.tree.rename(from, to)
    }

    fn set_status(&self, _rel: &str, _status: EntryStatus) {}

    fn watch(&self, debounce: Duration, on_batch: Box<dyn Fn(Vec<String>) + Send>) -> Option<Box<dyn std::any::Any + Send>> {
        // Explorer's pin and free-up change attributes and tell nobody: the watcher sees them, and the
        // worker acts on them before the engine is woken
        let (tx, rx) = mpsc::channel::<Vec<String>>();
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let inner = self.inner.clone();
        let _ = std::thread::Builder::new().name("medialake-cloud-attrs".into()).spawn(move || {
            while stop_rx.try_recv().is_err() {
                match rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(paths) => {
                        for rel in paths {
                            let st = inner.api.state_light(&inner.path(&rel)).unwrap_or_default();
                            if st.exists && (st.pin != Pin::Unspecified || st.is_dir) {
                                inner.walk(&rel);
                            }
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
        });
        let watcher = self.tree.watch(
            debounce,
            Box::new(move |paths| {
                let _ = tx.send(paths.clone());
                on_batch(paths);
            }),
        );
        Some(Box::new(WatchGuard { _watcher: watcher, _stop: stop_tx }))
    }
}
