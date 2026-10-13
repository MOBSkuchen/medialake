//! The cache tree and everything that moves bytes in and out of it: block-wise hydration from the host,
//! write bookkeeping, eviction, pins. Blocking by design: the NFS layer calls it from blocking threads
//! and the sync engine calls it from its own.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use medialake_sync::member::transport::{RangeFetch, Transport};
use medialake_sync::util::{hash_file, mtime_ms, now_secs, set_mtime_ms};

use crate::index::{BLOCK, FileRow, Index, Ranges, blocks_of};

/// Largest single request while serving a read, and while fetching a whole file.
const RUN_READ: u32 = 8;
const RUN_FULL: u32 = 32;
/// Read-ahead cap in blocks once a read pattern looks sequential.
const AHEAD_MAX: u32 = 16;

#[derive(Debug, Clone, serde::Serialize)]
pub struct BrowseItem {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    /// Modification time, ms since the epoch.
    pub mtime: i64,
    /// `cloud` (nothing here), `partial`, `cached`, `dirty` (changed here, not uploaded), `local`
    /// (created here, not known to the host yet) or `dir`.
    pub state: &'static str,
    pub pinned: bool,
    pub cached_bytes: u64,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct StoreStats {
    pub cached_bytes: u64,
    pub limit_bytes: u64,
    pub dirty_files: u64,
    pub fetched_bytes: u64,
    pub fetch_requests: u64,
    pub pinned: Vec<String>,
}

pub struct Store {
    tree: PathBuf,
    idx: Index,
    tr: Arc<dyn Transport>,
    limit: AtomicU64,
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    seq: Mutex<HashMap<String, (u64, u32)>>,
    pins: RwLock<Vec<String>>,
    sink: Mutex<Option<Arc<Sink>>>,
    stopped: AtomicBool,
    fetched: AtomicU64,
    requests: AtomicU64,
    /// Receives a message when something needs the user's attention (verification failed …).
    on_problem: Mutex<Option<Arc<dyn Fn(String) + Send + Sync>>>,
}

/// Debounced "these paths changed locally" for the sync engine's wake-ups.
struct Sink {
    pending: Mutex<(HashSet<String>, bool)>,
    cv: Condvar,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

pub struct SinkGuard {
    sink: Arc<Sink>,
}

impl Drop for SinkGuard {
    fn drop(&mut self) {
        self.sink.pending.lock().unwrap().1 = true;
        self.sink.cv.notify_all();
        if let Some(t) = self.sink.thread.lock().unwrap().take() {
            let _ = t.join();
        }
    }
}

pub enum Fetched {
    Ready,
    Stale,
}

impl Store {
    pub fn open(cache_dir: &Path, tr: Arc<dyn Transport>, limit_bytes: u64) -> Result<Arc<Store>> {
        let tree = cache_dir.join("tree");
        std::fs::create_dir_all(&tree).with_context(|| format!("creating {}", tree.display()))?;
        let idx = Index::open(&cache_dir.join("index.db"))?;
        let pins = idx.pins();
        Ok(Arc::new(Store {
            tree,
            idx,
            tr,
            limit: AtomicU64::new(limit_bytes),
            locks: Mutex::new(HashMap::new()),
            seq: Mutex::new(HashMap::new()),
            pins: RwLock::new(pins),
            sink: Mutex::new(None),
            stopped: AtomicBool::new(false),
            fetched: AtomicU64::new(0),
            requests: AtomicU64::new(0),
            on_problem: Mutex::new(None),
        }))
    }

    pub fn tree(&self) -> &Path {
        &self.tree
    }

    pub fn index(&self) -> &Index {
        &self.idx
    }

    pub fn abs(&self, rel: &str) -> PathBuf {
        if rel.is_empty() { self.tree.clone() } else { self.tree.join(rel) }
    }

    pub fn set_limit(&self, bytes: u64) {
        self.limit.store(bytes, Ordering::SeqCst);
    }

    pub fn set_problem_handler(&self, f: Option<Arc<dyn Fn(String) + Send + Sync>>) {
        *self.on_problem.lock().unwrap() = f;
    }

    fn problem(&self, msg: String) {
        tracing::warn!("{msg}");
        if let Some(f) = self.on_problem.lock().unwrap().clone() {
            f(msg);
        }
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }

    pub fn stats(&self) -> StoreStats {
        StoreStats {
            cached_bytes: self.idx.cached_bytes(),
            limit_bytes: self.limit.load(Ordering::SeqCst),
            dirty_files: self.idx.dirty_count(),
            fetched_bytes: self.fetched.load(Ordering::SeqCst),
            fetch_requests: self.requests.load(Ordering::SeqCst),
            pinned: self.pins.read().unwrap().clone(),
        }
    }

    fn lock_for(&self, rel: &str) -> Arc<Mutex<()>> {
        let mut m = self.locks.lock().unwrap();
        if m.len() > 4096 {
            m.retain(|_, v| Arc::strong_count(v) > 1);
        }
        m.entry(rel.to_string()).or_default().clone()
    }

    // ---------------- changes made through the mount ----------------

    /// Starts delivering batches of changed paths (debounced) until the guard is dropped.
    pub fn set_sink(&self, debounce: Duration, cb: Box<dyn Fn(Vec<String>) + Send>) -> SinkGuard {
        let sink = Arc::new(Sink { pending: Mutex::new((HashSet::new(), false)), cv: Condvar::new(), thread: Mutex::new(None) });
        let s2 = sink.clone();
        let t = std::thread::Builder::new()
            .name("medialake-mount-notify".into())
            .spawn(move || {
                loop {
                    let mut g = s2.pending.lock().unwrap();
                    while g.0.is_empty() && !g.1 {
                        g = s2.cv.wait(g).unwrap();
                    }
                    if g.1 {
                        return;
                    }
                    drop(g);
                    std::thread::sleep(debounce);
                    let batch: Vec<String> = std::mem::take(&mut s2.pending.lock().unwrap().0).into_iter().collect();
                    if !batch.is_empty() {
                        cb(batch);
                    }
                }
            })
            .ok();
        *sink.thread.lock().unwrap() = t;
        *self.sink.lock().unwrap() = Some(sink.clone());
        SinkGuard { sink }
    }

    /// Something at `rel` changed through the mount: the engine should look at it soon.
    pub fn changed(&self, rel: &str) {
        if let Some(s) = self.sink.lock().unwrap().as_ref() {
            s.pending.lock().unwrap().0.insert(rel.to_string());
            s.cv.notify_all();
        }
    }

    /// Content of `rel` was written through the mount.
    pub fn after_write(&self, rel: &str) {
        if let Some(mut r) = self.idx.row(rel) {
            if let Ok(m) = std::fs::metadata(self.abs(rel)) {
                r.size = m.len();
                r.mtime = mtime_ms(&m);
                r.dirty = true;
                r.full = true;
                r.blocks = Ranges::all(blocks_of(r.size));
                r.atime = now_secs();
                let _ = self.idx.put(rel, &r);
            }
        }
        self.changed(rel);
    }

    // ---------------- hydration ----------------

    /// Fetches what `[offset, offset + len)` needs (and read-ahead). The caller holds the file's lock.
    fn fetch_for(&self, rel: &str, offset: u64, len: u64, ahead: u32, run_max: u32) -> Result<Fetched> {
        let Some(mut row) = self.idx.row(rel) else { return Ok(Fetched::Ready) };
        if row.full || row.size == 0 {
            return Ok(Fetched::Ready);
        }
        let nblocks = blocks_of(row.size);
        let end = offset.saturating_add(len).min(row.size);
        if offset >= end {
            return Ok(Fetched::Ready);
        }
        let b0 = (offset / BLOCK) as u32;
        let b1 = end.div_ceil(BLOCK) as u32;
        if row.blocks.missing(b0, b1).is_empty() {
            return Ok(Fetched::Ready);
        }
        let want_end = (b1 + ahead).min(nblocks);
        let mut runs: Vec<(u32, u32)> = vec![];
        for (s, e) in row.blocks.missing(b0, want_end) {
            let mut s = s;
            while s < e {
                let t = (s + run_max).min(e);
                runs.push((s, t));
                s = t;
            }
        }
        let path = self.abs(rel);
        let f = std::fs::OpenOptions::new().write(true).open(&path).with_context(|| format!("opening {}", path.display()))?;
        for (s, e) in runs {
            let from = s as u64 * BLOCK;
            let want = (e as u64 * BLOCK).min(row.size) - from;
            self.requests.fetch_add(1, Ordering::Relaxed);
            let data = match self.tr.read_range(rel, row.rev, from, want)? {
                RangeFetch::Data(d) => d,
                RangeFetch::Stale | RangeFetch::NotFound => return Ok(Fetched::Stale),
            };
            if data.len() as u64 != want {
                // the host's file is not the one we know (shorter or longer): wait for the new version
                return Ok(Fetched::Stale);
            }
            f.write_all_at(&data, from)?;
            self.fetched.fetch_add(want, Ordering::Relaxed);
            row.blocks.insert(s, e);
            row.full = row.blocks.missing(0, nblocks).is_empty();
            row.atime = now_secs();
            self.idx.set_blocks(rel, &row.blocks, row.full, row.cached_bytes())?;
        }
        drop(f);
        // writing moved the mtime; the placeholder keeps the host's
        if row.mtime > 0 {
            let _ = set_mtime_ms(&path, row.mtime);
        }
        if row.full && !row.hash.is_empty() {
            let (h, _) = hash_file(&path)?;
            if h != row.hash {
                row.blocks = Ranges::default();
                row.full = false;
                self.idx.set_blocks(rel, &row.blocks, false, 0)?;
                let f = std::fs::OpenOptions::new().write(true).open(&path)?;
                f.set_len(0)?;
                f.set_len(row.size)?;
                let _ = set_mtime_ms(&path, row.mtime);
                bail!("{rel}: the fetched content does not match its hash");
            }
        }
        Ok(Fetched::Ready)
    }

    fn ahead_for(&self, rel: &str, offset: u64, len: u64) -> u32 {
        let mut s = self.seq.lock().unwrap();
        if s.len() > 2048 {
            s.clear();
        }
        let e = s.entry(rel.to_string()).or_insert((0, 0));
        let ahead = if offset == e.0 && offset > 0 { (e.1.max(1) * 2).min(AHEAD_MAX) } else { 0 };
        *e = (offset + len, ahead);
        ahead
    }

    /// Makes `[offset, offset + len)` of `rel` present, fetching missing blocks.
    pub fn ensure(&self, rel: &str, offset: u64, len: u64) -> Result<()> {
        self.with_hydrated(rel, offset, len, |_| Ok(()))
    }

    /// Runs `f` on the tree file while its lock is held and the range is present (so eviction cannot
    /// slip in between).
    fn with_hydrated<T>(&self, rel: &str, offset: u64, len: u64, f: impl FnOnce(&Path) -> Result<T>) -> Result<T> {
        let ahead = self.ahead_for(rel, offset, len);
        let mut f = Some(f);
        for attempt in 0..6u64 {
            {
                let lock = self.lock_for(rel);
                let _g = lock.lock().unwrap_or_else(|e| e.into_inner());
                match self.fetch_for(rel, offset, len, ahead, RUN_READ)? {
                    Fetched::Ready => {
                        self.maybe_touch(rel);
                        let r = (f.take().expect("called once"))(&self.abs(rel));
                        drop(_g);
                        if r.is_ok() {
                            self.enforce_limit();
                        }
                        return r;
                    }
                    Fetched::Stale => {}
                }
            }
            std::thread::sleep(Duration::from_millis(300 * (attempt + 1)));
        }
        bail!("{rel}: the file is changing on the host, try again in a moment")
    }

    fn maybe_touch(&self, rel: &str) {
        if let Some(r) = self.idx.row(rel) {
            let now = now_secs();
            if now - r.atime >= 30 {
                self.idx.touch(rel, now);
            }
        }
    }

    /// A read as the NFS server sees it: hydrates, then reads. Returns the bytes and whether the end of
    /// the file was reached.
    pub fn read(&self, rel: &str, offset: u64, count: u32) -> Result<(Vec<u8>, bool)> {
        self.with_hydrated(rel, offset, count as u64, |p| {
            let f = std::fs::File::open(p)?;
            let size = f.metadata()?.len();
            if offset >= size {
                return Ok((vec![], true));
            }
            let n = (count as u64).min(size - offset) as usize;
            let mut buf = vec![0u8; n];
            f.read_exact_at(&mut buf, offset)?;
            Ok((buf, offset + n as u64 >= size))
        })
    }

    /// Everything of `rel` present (before the file is written to, hashed or uploaded).
    pub fn ensure_full(&self, rel: &str) -> Result<()> {
        let Some(row) = self.idx.row(rel) else { return Ok(()) };
        if row.full {
            return Ok(());
        }
        let size = row.size;
        let mut at = 0u64;
        while at < size {
            if self.stopped.load(Ordering::SeqCst) {
                bail!("stopped");
            }
            let len = (RUN_FULL as u64 * BLOCK).min(size - at);
            let mut ok = false;
            for attempt in 0..6u64 {
                let lock = self.lock_for(rel);
                let g = lock.lock().unwrap_or_else(|e| e.into_inner());
                match self.fetch_for(rel, at, len, 0, RUN_FULL)? {
                    Fetched::Ready => {
                        ok = true;
                        break;
                    }
                    Fetched::Stale => {
                        drop(g);
                        std::thread::sleep(Duration::from_millis(300 * (attempt + 1)));
                    }
                }
            }
            if !ok {
                bail!("{rel}: the file is changing on the host");
            }
            at += len;
        }
        // a file that shrank or was replaced meanwhile has a fresh row; check once more
        match self.idx.row(rel) {
            Some(r) if !r.full => bail!("{rel}: content incomplete"),
            _ => Ok(()),
        }
    }

    // ---------------- eviction and pins ----------------

    pub fn is_pinned(&self, rel: &str) -> bool {
        self.pins.read().unwrap().iter().any(|p| p.is_empty() || p == rel || (rel.len() > p.len() && rel.starts_with(p.as_str()) && rel.as_bytes()[p.len()] == b'/'))
    }

    /// Drops the cached content of `rel` (keeping its place and metadata). False if it cannot go.
    pub fn evict(&self, rel: &str) -> Result<bool> {
        let lock = self.lock_for(rel);
        let Ok(_g) = lock.try_lock() else { return Ok(false) };
        let Some(mut row) = self.idx.row(rel) else { return Ok(false) };
        if row.dirty || row.cached_bytes() == 0 || self.is_pinned(rel) {
            return Ok(false);
        }
        let path = self.abs(rel);
        let f = std::fs::OpenOptions::new().write(true).open(&path)?;
        f.set_len(0)?;
        f.set_len(row.size)?;
        drop(f);
        let _ = set_mtime_ms(&path, row.mtime);
        row.blocks = Ranges::default();
        row.full = false;
        self.idx.set_blocks(rel, &row.blocks, false, 0)?;
        Ok(true)
    }

    /// Evicts least recently used clean files until the cache is at 90 % of its limit.
    pub fn enforce_limit(&self) {
        let limit = self.limit.load(Ordering::SeqCst);
        if limit == 0 {
            return;
        }
        let mut used = self.idx.cached_bytes();
        if used <= limit {
            return;
        }
        let target = limit / 10 * 9;
        for (path, row) in self.idx.eviction_candidates() {
            if used <= target {
                break;
            }
            let bytes = row.cached_bytes();
            if matches!(self.evict(&path), Ok(true)) {
                used = used.saturating_sub(bytes);
            }
        }
    }

    pub fn pin(self: &Arc<Self>, rel: &str) -> Result<()> {
        self.idx.add_pin(rel)?;
        {
            let mut p = self.pins.write().unwrap();
            if !p.iter().any(|x| x == rel) {
                p.push(rel.to_string());
            }
        }
        self.hydrate_pinned(rel.to_string());
        Ok(())
    }

    pub fn unpin(&self, rel: &str) -> Result<()> {
        self.idx.remove_pin(rel)?;
        self.pins.write().unwrap().retain(|p| p != rel);
        Ok(())
    }

    /// Fetches, in the background, every incomplete file at or below `prefix`.
    pub fn hydrate_pinned(self: &Arc<Self>, prefix: String) {
        let me = self.clone();
        let _ = std::thread::Builder::new().name("medialake-mount-pin".into()).spawn(move || {
            for rel in me.idx.incomplete_under(&prefix) {
                if me.stopped.load(Ordering::SeqCst) || !me.is_pinned(&rel) {
                    return;
                }
                if let Err(e) = me.ensure_full(&rel) {
                    me.problem(format!("could not keep {rel} on this device: {e:#}"));
                }
            }
        });
    }

    /// Re-fetches pinned files that are not complete (after a start, or after the host changed them).
    pub fn resume_pins(self: &Arc<Self>) {
        for p in self.pins.read().unwrap().clone() {
            self.hydrate_pinned(p);
        }
    }

    // ---------------- what the sync engine tells us ----------------

    pub fn content_hash(&self, rel: &str) -> Option<String> {
        let r = self.idx.row(rel)?;
        if r.dirty || r.hash.is_empty() { None } else { Some(r.hash) }
    }

    /// `rel` has content `hash` at host revision `rev` (and, if the file still has `size` / `mtime`, it
    /// is clean).
    pub fn note_content(&self, rel: &str, hash: &str, rev: u64, size: u64, mtime: i64) {
        let Ok(m) = std::fs::metadata(self.abs(rel)) else { return };
        if m.len() != size || mtime_ms(&m) != mtime {
            return;
        }
        let existing = self.idx.row(rel);
        let mut r = existing.unwrap_or_else(|| FileRow {
            size,
            mtime,
            hash: String::new(),
            rev: 0,
            dirty: false,
            full: true,
            blocks: Ranges::all(blocks_of(size)),
            atime: now_secs(),
        });
        r.size = size;
        r.mtime = mtime;
        r.hash = hash.to_string();
        r.rev = rev;
        r.dirty = false;
        if let Err(e) = self.idx.put(rel, &r) {
            tracing::warn!("recording {rel}: {e:#}");
        }
        self.enforce_limit();
    }

    /// A placeholder for the host's file: full size, host mtime, no content. False if pinned.
    pub fn place_placeholder(&self, rel: &str, size: u64, mtime: i64, exec: bool, hash: &str, rev: u64) -> Result<bool> {
        if self.is_pinned(rel) {
            return Ok(false);
        }
        let lock = self.lock_for(rel);
        let _g = lock.lock().unwrap_or_else(|e| e.into_inner());
        let p = self.abs(rel);
        if p.is_dir() {
            bail!("a folder is in the way at {}", p.display());
        }
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // row first: a reader that arrives meanwhile sees "nothing present"
        self.idx.put(
            rel,
            &FileRow { size, mtime, hash: hash.to_string(), rev, dirty: false, full: false, blocks: Ranges::default(), atime: now_secs() },
        )?;
        let f = std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(&p)?;
        f.set_len(size)?;
        drop(f);
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(if exec { 0o755 } else { 0o644 }))?;
        if mtime > 0 {
            set_mtime_ms(&p, mtime)?;
        }
        Ok(true)
    }

    /// The whole file arrived (pinned, conflict resolution …): move it into the tree as present.
    pub fn place_file(&self, rel: &str, staged: &Path, mtime: i64, exec: bool) -> Result<()> {
        let lock = self.lock_for(rel);
        let _g = lock.lock().unwrap_or_else(|e| e.into_inner());
        let dest = self.abs(rel);
        if dest.is_dir() {
            bail!("a folder is in the way at {}", dest.display());
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = dest.with_file_name(format!(".{}.medialake.part", medialake_sync::util::random_id()));
        if std::fs::rename(staged, &tmp).is_err() {
            std::fs::copy(staged, &tmp).with_context(|| format!("copying into {}", tmp.display()))?;
            let _ = std::fs::remove_file(staged);
        }
        if mtime > 0 {
            set_mtime_ms(&tmp, mtime)?;
        }
        medialake_sync::util::set_exec(&tmp, exec)?;
        let m = std::fs::metadata(&tmp)?;
        std::fs::rename(&tmp, &dest).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
        self.idx.put(
            rel,
            &FileRow {
                size: m.len(),
                mtime: if mtime > 0 { mtime } else { mtime_ms(&m) },
                hash: String::new(),
                rev: 0,
                dirty: false,
                full: true,
                blocks: Ranges::all(blocks_of(m.len())),
                atime: now_secs(),
            },
        )?;
        Ok(())
    }

    // ---------------- listing ----------------

    pub fn browse(&self, rel: &str) -> Result<Vec<BrowseItem>> {
        let dir = self.abs(rel);
        let mut out = vec![];
        for e in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))?.flatten() {
            let Some(name) = e.file_name().to_str().map(str::to_string) else { continue };
            let path = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            if medialake_proto::sync::is_ignored(&path, &[]) {
                continue;
            }
            let Ok(m) = e.metadata() else { continue };
            let pinned = self.is_pinned(&path);
            if m.is_dir() {
                out.push(BrowseItem { name, path, is_dir: true, size: 0, mtime: mtime_ms(&m), state: "dir", pinned, cached_bytes: 0 });
                continue;
            }
            let (state, cached) = match self.idx.row(&path) {
                None => ("local", m.len()),
                Some(r) if r.dirty => ("dirty", m.len()),
                Some(r) if r.full => ("cached", r.size),
                Some(r) if r.blocks.is_empty() => ("cloud", 0),
                Some(r) => ("partial", r.cached_bytes()),
            };
            out.push(BrowseItem { name, path, is_dir: false, size: m.len(), mtime: mtime_ms(&m), state, pinned, cached_bytes: cached });
        }
        out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        Ok(out)
    }

    /// Copies every file whose content is entirely here to `dest` (a plain folder), keeping the layout.
    /// Used when a sync is left but its downloaded files are kept. Returns (copied, left behind).
    pub fn export_to(&self, dest: &Path) -> Result<(u64, u64)> {
        let (mut copied, mut skipped) = (0u64, 0u64);
        let mut stack = vec![String::new()];
        while let Some(rel) = stack.pop() {
            let dir = self.abs(&rel);
            for e in std::fs::read_dir(&dir)?.flatten() {
                let Some(name) = e.file_name().to_str().map(str::to_string) else { continue };
                let child = if rel.is_empty() { name } else { format!("{rel}/{name}") };
                if medialake_proto::sync::is_ignored(&child, &[]) {
                    continue;
                }
                let Ok(m) = e.metadata() else { continue };
                let target = dest.join(&child);
                if m.is_dir() {
                    std::fs::create_dir_all(&target)?;
                    stack.push(child);
                    continue;
                }
                let present = self.idx.row(&child).is_none_or(|r| r.full);
                if !present {
                    skipped += 1;
                    continue;
                }
                if let Some(p) = target.parent() {
                    std::fs::create_dir_all(p)?;
                }
                if !medialake_sync::util::try_clone(&e.path(), &target) {
                    std::fs::copy(e.path(), &target)?;
                }
                let _ = set_mtime_ms(&target, mtime_ms(&m));
                copied += 1;
            }
        }
        Ok((copied, skipped))
    }

    /// True if reading `rel` would have to fetch something (the indexer asks, to avoid downloads).
    pub fn is_dehydrated(&self, rel: &str) -> bool {
        self.idx.row(rel).is_some_and(|r| !r.full)
    }

    pub fn used_blocks(&self, rel: &str) -> Option<(u64, u64)> {
        let m = std::fs::metadata(self.abs(rel)).ok()?;
        Some((m.blocks() * 512, m.len()))
    }
}
