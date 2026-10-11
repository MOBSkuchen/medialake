//! One hosted sync: a plain folder plus `<store>/state.db`.
//!
//! The folder is the ground truth. Changes arrive two ways, and both end up as numbered revisions in
//! the database (§3.1): commits from members (`commit`) and direct edits on the host (`scan`, driven by
//! the watcher). Everything here is blocking; the async service wraps it in `spawn_blocking`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{Context, Result, bail};
use medialake_proto::sync::*;
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};

use super::config::{BackupPolicy, HostedConfig};
use crate::util::*;

pub type Notifier = Arc<dyn Fn(&str, u64) + Send + Sync>;

/// Author recorded for changes found by scanning the folder.
pub const HOST_AUTHOR: &str = "host";
/// Tombstones are kept this long, so a member that was offline for weeks still learns about deletes.
pub const TOMBSTONE_DAYS: i64 = 90;
const STAGE_MAX_AGE_SECS: i64 = 24 * 3600;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS entries(
    path TEXT PRIMARY KEY, path_key TEXT NOT NULL, kind INTEGER NOT NULL, size INTEGER NOT NULL,
    mtime INTEGER NOT NULL, hash TEXT NOT NULL, exec INTEGER NOT NULL, rev INTEGER NOT NULL,
    deleted INTEGER NOT NULL, author TEXT NOT NULL, deleted_at INTEGER NOT NULL DEFAULT 0);
CREATE UNIQUE INDEX IF NOT EXISTS entries_key ON entries(path_key) WHERE deleted = 0;
CREATE INDEX IF NOT EXISTS entries_rev ON entries(rev);
CREATE INDEX IF NOT EXISTS entries_hash ON entries(hash) WHERE deleted = 0 AND kind = 0;
CREATE TABLE IF NOT EXISTS versions(
    id INTEGER PRIMARY KEY AUTOINCREMENT, path TEXT NOT NULL, rev INTEGER NOT NULL, hash TEXT NOT NULL,
    size INTEGER NOT NULL, mtime INTEGER NOT NULL, author TEXT NOT NULL, created INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS versions_path ON versions(path);
CREATE TABLE IF NOT EXISTS snapshots(id INTEGER PRIMARY KEY AUTOINCREMENT, seq INTEGER NOT NULL, created INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS snapshot_entries(
    snapshot INTEGER NOT NULL, path TEXT NOT NULL, kind INTEGER NOT NULL, hash TEXT NOT NULL,
    size INTEGER NOT NULL, mtime INTEGER NOT NULL, exec INTEGER NOT NULL, PRIMARY KEY(snapshot, path));
CREATE TABLE IF NOT EXISTS blobs(hash TEXT PRIMARY KEY, size INTEGER NOT NULL);
";

const COLS: &str = "path,kind,size,mtime,hash,exec,rev,deleted,author";

fn row_entry(r: &Row) -> rusqlite::Result<Entry> {
    Ok(Entry {
        path: r.get(0)?,
        kind: if r.get::<_, i64>(1)? == 0 { EntryKind::File } else { EntryKind::Dir },
        size: r.get::<_, i64>(2)? as u64,
        mtime: r.get(3)?,
        hash: r.get(4)?,
        exec: r.get::<_, i64>(5)? != 0,
        rev: r.get::<_, i64>(6)? as u64,
        deleted: r.get::<_, i64>(7)? != 0,
        author: r.get(8)?,
    })
}

fn kind_i(k: EntryKind) -> i64 {
    if k == EntryKind::File { 0 } else { 1 }
}

fn get_entry(c: &Connection, path: &str) -> Result<Option<Entry>> {
    Ok(c.query_row(&format!("SELECT {COLS} FROM entries WHERE path=?1"), [path], row_entry).optional()?)
}

fn get_live(c: &Connection, path: &str) -> Result<Option<Entry>> {
    Ok(get_entry(c, path)?.filter(|e| !e.deleted))
}

/// A live entry with the same case-insensitive key but another path.
fn get_collision(c: &Connection, path: &str, ignore: Option<&str>) -> Result<Option<Entry>> {
    let key = path_key(path);
    let e = c
        .query_row(&format!("SELECT {COLS} FROM entries WHERE path_key=?1 AND deleted=0"), [&key], row_entry)
        .optional()?;
    Ok(e.filter(|e| e.path != path && Some(e.path.as_str()) != ignore))
}

fn write_entry(c: &Connection, e: &Entry, deleted_at: i64) -> Result<()> {
    c.execute(
        "INSERT INTO entries(path,path_key,kind,size,mtime,hash,exec,rev,deleted,author,deleted_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
         ON CONFLICT(path) DO UPDATE SET path_key=?2,kind=?3,size=?4,mtime=?5,hash=?6,exec=?7,rev=?8,deleted=?9,author=?10,deleted_at=?11",
        params![
            e.path,
            path_key(&e.path),
            kind_i(e.kind),
            e.size as i64,
            e.mtime,
            e.hash,
            e.exec as i64,
            e.rev as i64,
            e.deleted as i64,
            e.author,
            deleted_at
        ],
    )?;
    Ok(())
}

fn get_meta(c: &Connection, key: &str) -> Result<Option<String>> {
    Ok(c.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0)).optional()?)
}

fn set_meta(c: &Connection, key: &str, value: &str) -> Result<()> {
    c.execute("INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=?2", params![key, value])?;
    Ok(())
}

/// Live entries at `prefix` or below (empty prefix = everything), ordered by path.
fn live_under(c: &Connection, prefix: &str) -> Result<Vec<Entry>> {
    let mut st = c.prepare(&format!(
        "SELECT {COLS} FROM entries WHERE deleted=0 AND (?1='' OR path=?1 OR substr(path,1,length(?1)+1)=?1||'/') ORDER BY path"
    ))?;
    let rows = st.query_map([prefix], row_entry)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn has_live_children(c: &Connection, dir: &str) -> Result<bool> {
    Ok(c.query_row(
        "SELECT 1 FROM entries WHERE deleted=0 AND substr(path,1,length(?1)+1)=?1||'/' LIMIT 1",
        [dir],
        |_| Ok(()),
    )
    .optional()?
    .is_some())
}

/// Per-operation context: the open transaction, the sequence counter and who is acting.
struct Cx<'a> {
    c: &'a Connection,
    seq: u64,
    author: String,
    now: i64,
}

impl Cx<'_> {
    fn next(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }
}

pub enum ChangesResult {
    Reset { epoch: String },
    Page { epoch: String, seq: u64, entries: Vec<Entry>, next_cursor: u64, more: bool },
}

pub enum DownloadSource {
    File { path: PathBuf, size: u64 },
    /// The file no longer matches the revision asked for (a direct edit not processed yet).
    Stale,
    NotFound,
}

struct Found {
    rel: String,
    kind: EntryKind,
    size: u64,
    mtime: i64,
    exec: bool,
}

pub struct HostedSync {
    id: String,
    root: PathBuf,
    store: PathBuf,
    cfg: RwLock<HostedConfig>,
    db: Mutex<Connection>,
    notifier: Mutex<Option<Notifier>>,
    notified: AtomicU64,
    issues: Mutex<Vec<String>>,
}

impl HostedSync {
    pub fn open(cfg: HostedConfig, notifier: Option<Notifier>) -> Result<Arc<HostedSync>> {
        let root = cfg.root.clone();
        if !root.is_dir() {
            bail!("sync folder {} does not exist", root.display());
        }
        let store = cfg.store_dir();
        std::fs::create_dir_all(store.join("tmp")).with_context(|| format!("creating {}", store.display()))?;
        std::fs::create_dir_all(store.join("blobs"))?;
        let conn = Connection::open(store.join("state.db"))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=5000;")?;
        conn.execute_batch(SCHEMA)?;
        if get_meta(&conn, "epoch")?.is_none() {
            set_meta(&conn, "epoch", &random_id())?;
            set_meta(&conn, "seq", "0")?;
            set_meta(&conn, "floor", "0")?;
        }
        set_meta(&conn, "sync_id", &cfg.id)?;
        let seq: u64 = get_meta(&conn, "seq")?.and_then(|s| s.parse().ok()).unwrap_or(0);
        Ok(Arc::new(HostedSync {
            id: cfg.id.clone(),
            root,
            store,
            cfg: RwLock::new(cfg),
            db: Mutex::new(conn),
            notifier: Mutex::new(notifier),
            notified: AtomicU64::new(seq),
            issues: Mutex::new(vec![]),
        }))
    }

    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn store_dir(&self) -> &Path {
        &self.store
    }
    pub fn config(&self) -> HostedConfig {
        self.cfg.read().unwrap().clone()
    }
    pub fn set_config(&self, cfg: HostedConfig) {
        *self.cfg.write().unwrap() = cfg;
    }
    pub fn set_notifier(&self, n: Option<Notifier>) {
        *self.notifier.lock().unwrap() = n;
    }
    fn policy(&self) -> BackupPolicy {
        self.cfg.read().unwrap().backup.clone()
    }
    fn ignore(&self) -> Vec<String> {
        self.cfg.read().unwrap().ignore.clone()
    }

    pub fn epoch(&self) -> String {
        let c = self.db.lock().unwrap();
        get_meta(&c, "epoch").ok().flatten().unwrap_or_default()
    }

    pub fn seq(&self) -> u64 {
        let c = self.db.lock().unwrap();
        get_meta(&c, "seq").ok().flatten().and_then(|s| s.parse().ok()).unwrap_or(0)
    }

    /// Paths the last scan skipped, with the reason.
    pub fn issues(&self) -> Vec<String> {
        self.issues.lock().unwrap().clone()
    }

    fn notify_if_advanced(&self) {
        let seq = self.seq();
        let prev = self.notified.fetch_max(seq, Ordering::SeqCst);
        if seq > prev {
            if let Some(n) = self.notifier.lock().unwrap().clone() {
                n(&self.id, seq);
            }
        }
    }

    /// Notifies subscribers if another process (an admin command) advanced the sequence.
    pub fn poll_notify(&self) {
        self.notify_if_advanced();
    }

    pub fn info(&self, access: Access) -> SyncInfo {
        let c = self.db.lock().unwrap();
        let (files, bytes): (i64, i64) = c
            .query_row("SELECT COUNT(*), COALESCE(SUM(size),0) FROM entries WHERE deleted=0 AND kind=0", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap_or((0, 0));
        let cfg = self.cfg.read().unwrap();
        SyncInfo {
            id: self.id.clone(),
            name: cfg.name.clone(),
            access,
            files: files as u64,
            bytes: bytes as u64,
            backup: cfg.backup.describe(),
            seq: get_meta(&c, "seq").ok().flatten().and_then(|s| s.parse().ok()).unwrap_or(0),
        }
    }

    pub fn entry(&self, path: &str) -> Option<Entry> {
        let c = self.db.lock().unwrap();
        get_entry(&c, path).ok().flatten()
    }

    /// All live entries, ordered by path.
    pub fn list(&self) -> Vec<Entry> {
        let c = self.db.lock().unwrap();
        live_under(&c, "").unwrap_or_default()
    }

    // ---------------- paths on disk ----------------

    fn abs(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn blob_path(&self, hash: &str) -> PathBuf {
        self.store.join("blobs").join(&hash[..2]).join(hash)
    }

    fn tmp_file(&self, name: &str) -> PathBuf {
        self.store.join("tmp").join(name)
    }

    // ---------------- changes ----------------

    pub fn changes(&self, epoch: &str, cursor: u64, limit: u32) -> Result<ChangesResult> {
        let c = self.db.lock().unwrap();
        let my_epoch = get_meta(&c, "epoch")?.unwrap_or_default();
        let floor: u64 = get_meta(&c, "floor")?.and_then(|s| s.parse().ok()).unwrap_or(0);
        if (!epoch.is_empty() && epoch != my_epoch) || (cursor != 0 && cursor < floor) {
            return Ok(ChangesResult::Reset { epoch: my_epoch });
        }
        let seq: u64 = get_meta(&c, "seq")?.and_then(|s| s.parse().ok()).unwrap_or(0);
        let limit = limit.clamp(1, MAX_CHANGES_PAGE) as i64;
        let mut st = c.prepare(&format!("SELECT {COLS} FROM entries WHERE rev > ?1 AND (?1 > 0 OR deleted = 0) ORDER BY rev LIMIT ?2"))?;
        let mut entries = st.query_map(params![cursor as i64, limit + 1], row_entry)?.collect::<rusqlite::Result<Vec<_>>>()?;
        let more = entries.len() as i64 > limit;
        entries.truncate(limit as usize);
        let next_cursor = entries.last().map(|e| e.rev).unwrap_or(cursor);
        Ok(ChangesResult::Page { epoch: my_epoch, seq, entries, next_cursor, more })
    }

    // ---------------- propose / commit ----------------

    pub fn propose(&self, access: Access, author: &str, changes: &[Change]) -> Result<Verdict> {
        let mut c = self.db.lock().unwrap();
        let (results, need) = self.process(&mut c, access, author, changes, true)?;
        Ok(Verdict { results, need })
    }

    pub fn commit(&self, access: Access, author: &str, changes: &[Change]) -> Result<Vec<ChangeResult>> {
        let results = {
            let mut c = self.db.lock().unwrap();
            self.process(&mut c, access, author, changes, false)?.0
        };
        self.notify_if_advanced();
        Ok(results)
    }

    fn process(&self, conn: &mut Connection, access: Access, author: &str, changes: &[Change], dry: bool) -> Result<(Vec<ChangeResult>, Vec<String>)> {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let seq: u64 = get_meta(&tx, "seq")?.and_then(|s| s.parse().ok()).unwrap_or(0);
        let mut cx = Cx { c: &tx, seq, author: author.to_string(), now: now_secs() };
        let mut results = Vec::with_capacity(changes.len());
        let mut need: Vec<String> = vec![];
        for ch in changes {
            let r = if access != Access::ReadWrite {
                ChangeResult::Denied
            } else {
                match self.apply(&mut cx, ch, dry, &mut need) {
                    Ok(r) => r,
                    Err(e) => ChangeResult::Invalid { message: format!("{e:#}") },
                }
            };
            results.push(r);
        }
        if dry {
            drop(tx);
        } else {
            set_meta(&tx, "seq", &cx.seq.to_string())?;
            tx.commit()?;
        }
        Ok((results, need))
    }

    fn conflict(reason: ConflictReason, current: Option<Entry>) -> ChangeResult {
        ChangeResult::Conflict { reason, current }
    }

    /// Makes the database describe a path as it is on disk right now, so that a member's base is
    /// compared against the truth and not against a stale row (a direct edit the watcher has not
    /// processed yet).
    fn refresh_path(&self, cx: &mut Cx, rel: &str) -> Result<()> {
        self.observe(cx, rel)
    }

    fn apply(&self, cx: &mut Cx, ch: &Change, dry: bool, need: &mut Vec<String>) -> Result<ChangeResult> {
        let ignore = self.ignore();
        let check_path = |p: &str| -> std::result::Result<String, ChangeResult> {
            let n = normalize_path(p).map_err(|e| ChangeResult::Invalid { message: e.to_string() })?;
            if is_ignored(&n, &ignore) {
                return Err(ChangeResult::Invalid { message: format!("{n:?} is an ignored name") });
            }
            Ok(n)
        };
        match ch {
            Change::Put { path, hash, size, mtime, exec, base_rev } => {
                let path = match check_path(path) {
                    Ok(p) => p,
                    Err(r) => return Ok(r),
                };
                if !is_hash(hash) {
                    return Ok(ChangeResult::Invalid { message: "bad content hash".into() });
                }
                self.refresh_path(cx, &path)?;
                let cur = get_entry(cx.c, &path)?;
                if let Some(r) = self.check_base(&cur, *base_rev) {
                    return Ok(r);
                }
                if let Some(e) = &cur {
                    if !e.deleted && e.kind == EntryKind::Dir {
                        return Ok(Self::conflict(ConflictReason::TargetExists, cur));
                    }
                }
                if let Some(o) = get_collision(cx.c, &path, None)? {
                    return Ok(Self::conflict(ConflictReason::CaseCollision, Some(o)));
                }
                let missing = match self.check_parents(cx.c, &path)? {
                    Ok(m) => m,
                    Err(r) => return Ok(r),
                };
                let live_same = cur.as_ref().is_some_and(|e| !e.deleted && e.hash == *hash);
                let src = if live_same { None } else { self.find_content(cx.c, hash, *size)? };
                if !live_same && src.is_none() {
                    if dry {
                        if !need.contains(hash) {
                            need.push(hash.clone());
                        }
                        return Ok(ChangeResult::Ok { rev: 0 });
                    }
                    return Ok(ChangeResult::MissingContent);
                }
                if dry {
                    return Ok(ChangeResult::Ok { rev: 0 });
                }
                self.create_parents(cx, &missing)?;
                let dest = self.abs(&path);
                if let Some(src) = src {
                    self.place(&src, &dest)?;
                }
                if *mtime > 0 {
                    set_mtime_ms(&dest, *mtime)?;
                }
                set_exec(&dest, *exec)?;
                let m = std::fs::metadata(&dest)?;
                let rev = cx.next();
                let e = Entry {
                    path: path.clone(),
                    kind: EntryKind::File,
                    size: m.len(),
                    mtime: mtime_ms(&m),
                    hash: hash.clone(),
                    exec: is_exec(&m) && *exec,
                    rev,
                    deleted: false,
                    author: cx.author.clone(),
                };
                write_entry(cx.c, &e, 0)?;
                self.make_current(cx, &e, &dest)?;
                Ok(ChangeResult::Ok { rev })
            }
            Change::Mkdir { path } => {
                let path = match check_path(path) {
                    Ok(p) => p,
                    Err(r) => return Ok(r),
                };
                self.refresh_path(cx, &path)?;
                let cur = get_entry(cx.c, &path)?;
                if let Some(e) = &cur {
                    if !e.deleted {
                        return Ok(match e.kind {
                            EntryKind::Dir => ChangeResult::Ok { rev: e.rev },
                            EntryKind::File => Self::conflict(ConflictReason::TargetExists, cur),
                        });
                    }
                }
                if let Some(o) = get_collision(cx.c, &path, None)? {
                    return Ok(Self::conflict(ConflictReason::CaseCollision, Some(o)));
                }
                let missing = match self.check_parents(cx.c, &path)? {
                    Ok(m) => m,
                    Err(r) => return Ok(r),
                };
                if dry {
                    return Ok(ChangeResult::Ok { rev: 0 });
                }
                self.create_parents(cx, &missing)?;
                std::fs::create_dir_all(self.abs(&path))?;
                let rev = cx.next();
                let e = Entry {
                    path,
                    kind: EntryKind::Dir,
                    size: 0,
                    mtime: now_ms(),
                    hash: String::new(),
                    exec: false,
                    rev,
                    deleted: false,
                    author: cx.author.clone(),
                };
                write_entry(cx.c, &e, 0)?;
                Ok(ChangeResult::Ok { rev })
            }
            Change::Delete { path, base_rev } => {
                let path = match check_path(path) {
                    Ok(p) => p,
                    Err(r) => return Ok(r),
                };
                self.refresh_path(cx, &path)?;
                let cur = get_entry(cx.c, &path)?;
                let Some(e) = cur.clone().filter(|e| !e.deleted) else {
                    // already gone: idempotent when the member's idea of it is consistent
                    return Ok(match &cur {
                        Some(t) if *base_rev == t.rev || *base_rev == 0 => ChangeResult::Ok { rev: t.rev },
                        None if *base_rev == 0 => ChangeResult::Ok { rev: 0 },
                        _ => Self::conflict(ConflictReason::BaseMismatch, cur),
                    });
                };
                if e.rev != *base_rev {
                    return Ok(Self::conflict(ConflictReason::BaseMismatch, Some(e)));
                }
                if e.kind == EntryKind::Dir {
                    // pick up files created directly in it before deciding that it is empty
                    self.observe_tree(cx, &path)?;
                    if has_live_children(cx.c, &path)? {
                        return Ok(Self::conflict(ConflictReason::NotEmpty, get_entry(cx.c, &path)?));
                    }
                }
                if dry {
                    return Ok(ChangeResult::Ok { rev: 0 });
                }
                let abs = self.abs(&path);
                match e.kind {
                    EntryKind::File => match std::fs::remove_file(&abs) {
                        Ok(()) => {}
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                        Err(err) => return Err(err.into()),
                    },
                    EntryKind::Dir => match std::fs::remove_dir_all(&abs) {
                        Ok(()) => {}
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                        Err(err) => return Err(err.into()),
                    },
                }
                let rev = self.tombstone(cx, &e)?;
                Ok(ChangeResult::Ok { rev })
            }
            Change::Move { from, to, base_rev } => {
                let (from, to) = match (check_path(from), check_path(to)) {
                    (Ok(a), Ok(b)) => (a, b),
                    (Err(r), _) | (_, Err(r)) => return Ok(r),
                };
                if is_within(&to, &from) {
                    return Ok(ChangeResult::Invalid { message: "cannot move a folder into itself".into() });
                }
                self.refresh_path(cx, &from)?;
                self.refresh_path(cx, &to)?;
                let Some(src) = get_live(cx.c, &from)? else {
                    return Ok(Self::conflict(ConflictReason::BaseMismatch, get_entry(cx.c, &from)?));
                };
                if src.rev != *base_rev {
                    return Ok(Self::conflict(ConflictReason::BaseMismatch, Some(src)));
                }
                if src.kind == EntryKind::Dir {
                    self.observe_tree(cx, &from)?;
                }
                if let Some(t) = get_live(cx.c, &to)? {
                    return Ok(Self::conflict(ConflictReason::TargetExists, Some(t)));
                }
                if let Some(o) = get_collision(cx.c, &to, Some(&from))? {
                    return Ok(Self::conflict(ConflictReason::CaseCollision, Some(o)));
                }
                let missing = match self.check_parents(cx.c, &to)? {
                    Ok(m) => m,
                    Err(r) => return Ok(r),
                };
                if dry {
                    return Ok(ChangeResult::Ok { rev: 0 });
                }
                self.create_parents(cx, &missing)?;
                let dest = self.abs(&to);
                if let Some(p) = dest.parent() {
                    std::fs::create_dir_all(p)?;
                }
                std::fs::rename(self.abs(&from), &dest)?;
                let rev = self.move_tree(cx, &from, &to)?;
                Ok(ChangeResult::Ok { rev })
            }
        }
    }

    /// `base_rev` rules: a live entry needs `base_rev == rev`; a missing entry or a tombstone accepts 0
    /// or the tombstone's own revision.
    fn check_base(&self, cur: &Option<Entry>, base_rev: u64) -> Option<ChangeResult> {
        match cur {
            Some(e) if !e.deleted => (e.rev != base_rev).then(|| Self::conflict(ConflictReason::BaseMismatch, cur.clone())),
            Some(t) => (base_rev != 0 && base_rev != t.rev).then(|| Self::conflict(ConflictReason::BaseMismatch, cur.clone())),
            None => (base_rev != 0).then(|| Self::conflict(ConflictReason::BaseMismatch, None)),
        }
    }

    /// Ancestors of `path` that do not exist yet, or the reason it cannot be created.
    fn check_parents(&self, c: &Connection, path: &str) -> Result<std::result::Result<Vec<String>, ChangeResult>> {
        let mut missing = vec![];
        let mut acc = String::new();
        let comps: Vec<&str> = path.split('/').collect();
        for comp in &comps[..comps.len() - 1] {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(comp);
            match get_live(c, &acc)? {
                Some(e) if e.kind == EntryKind::File => return Ok(Err(Self::conflict(ConflictReason::ParentIsFile, Some(e)))),
                Some(_) => {}
                None => {
                    if let Some(o) = get_collision(c, &acc, None)? {
                        return Ok(Err(Self::conflict(ConflictReason::CaseCollision, Some(o))));
                    }
                    missing.push(acc.clone());
                }
            }
        }
        Ok(Ok(missing))
    }

    fn create_parents(&self, cx: &mut Cx, missing: &[String]) -> Result<()> {
        for d in missing {
            std::fs::create_dir_all(self.abs(d))?;
            let rev = cx.next();
            let e = Entry {
                path: d.clone(),
                kind: EntryKind::Dir,
                size: 0,
                mtime: now_ms(),
                hash: String::new(),
                exec: false,
                rev,
                deleted: false,
                author: cx.author.clone(),
            };
            write_entry(cx.c, &e, 0)?;
        }
        Ok(())
    }

    fn tombstone(&self, cx: &mut Cx, e: &Entry) -> Result<u64> {
        let rev = cx.next();
        let t = Entry {
            path: e.path.clone(),
            kind: e.kind,
            size: 0,
            mtime: now_ms(),
            hash: String::new(),
            exec: false,
            rev,
            deleted: true,
            author: cx.author.clone(),
        };
        write_entry(cx.c, &t, cx.now)?;
        Ok(rev)
    }

    /// Tombstones `rel` and everything below it, children first.
    fn tombstone_tree(&self, cx: &mut Cx, rel: &str) -> Result<()> {
        let mut es = live_under(cx.c, rel)?;
        es.sort_by(|a, b| b.path.cmp(&a.path));
        for e in es {
            self.tombstone(cx, &e)?;
        }
        Ok(())
    }

    /// Re-records every live entry at or below `from` under `to` (tombstone + new entry, history
    /// follows). Returns the revision of the new top entry.
    fn move_tree(&self, cx: &mut Cx, from: &str, to: &str) -> Result<u64> {
        let es = live_under(cx.c, from)?;
        let mut top = 0;
        // new entries parents-first (ascending), tombstones afterwards children-first
        for e in &es {
            let new_path = format!("{to}{}", &e.path[from.len()..]);
            let rev = cx.next();
            let mut n = e.clone();
            n.path = new_path.clone();
            n.rev = rev;
            n.author = cx.author.clone();
            // free the old key first so that case-only renames do not collide
            let old_t = Entry { deleted: true, hash: String::new(), size: 0, exec: false, ..e.clone() };
            let mut old_t = old_t;
            old_t.rev = rev;
            old_t.author = cx.author.clone();
            write_entry(cx.c, &old_t, cx.now)?;
            write_entry(cx.c, &n, 0)?;
            cx.c.execute("UPDATE versions SET path=?1 WHERE path=?2", params![new_path, e.path])?;
            if e.path == from {
                top = rev;
            }
        }
        if let Some(e) = es.iter().find(|e| e.path == from) {
            if e.kind == EntryKind::File {
                let new = get_entry(cx.c, to)?.unwrap();
                self.make_current(cx, &new, &self.abs(to))?;
            }
        }
        Ok(top)
    }

    /// Where the content of `hash` can be read from: a finished upload, the blob store, or a live
    /// file that still matches its row.
    fn find_content(&self, c: &Connection, hash: &str, size: u64) -> Result<Option<PathBuf>> {
        let ready = self.tmp_file(&format!("{hash}.ready"));
        if ready.is_file() {
            return Ok(Some(ready));
        }
        let blob = self.blob_path(hash);
        if blob.is_file() {
            return Ok(Some(blob));
        }
        let mut st = c.prepare("SELECT path,size,mtime FROM entries WHERE hash=?1 AND deleted=0 AND kind=0 LIMIT 8")?;
        let rows = st.query_map([hash], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64, r.get::<_, i64>(2)?)))?;
        for row in rows {
            let (p, sz, mt) = row?;
            if size != 0 && sz != size {
                continue;
            }
            let abs = self.abs(&p);
            if let Ok(m) = std::fs::metadata(&abs) {
                if m.is_file() && m.len() == sz && mtime_ms(&m) == mt {
                    return Ok(Some(abs));
                }
            }
        }
        Ok(None)
    }

    /// Puts `src` at `dest` atomically (staged uploads are moved, everything else copied or cloned).
    fn place(&self, src: &Path, dest: &Path) -> Result<()> {
        if let Some(p) = dest.parent() {
            std::fs::create_dir_all(p)?;
        }
        if dest.is_dir() {
            bail!("a folder is in the way at {}", dest.display());
        }
        let staged = src.starts_with(self.store.join("tmp"));
        if staged {
            move_file(src, dest)?;
            return Ok(());
        }
        let tmp = dest.with_file_name(format!(".{}.medialake.part", random_id()));
        if !try_clone(src, &tmp) {
            std::fs::copy(src, &tmp)?;
        }
        std::fs::rename(&tmp, dest).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
        Ok(())
    }

    // ---------------- uploads and downloads ----------------

    /// Bytes of the upload of `hash` the host already holds (resume point). `size` means: complete.
    pub fn upload_have(&self, hash: &str, size: u64) -> u64 {
        if !is_hash(hash) {
            return 0;
        }
        if self.tmp_file(&format!("{hash}.ready")).is_file() || self.blob_path(hash).is_file() {
            return size;
        }
        std::fs::metadata(self.tmp_file(&format!("{hash}.part"))).map(|m| m.len().min(size)).unwrap_or(0)
    }

    pub fn upload_part_path(&self, hash: &str) -> PathBuf {
        self.tmp_file(&format!("{hash}.part"))
    }

    /// Verifies a finished upload and keeps it only if the hash matches.
    pub fn upload_finish(&self, hash: &str) -> Result<bool> {
        let part = self.upload_part_path(hash);
        let (h, _) = hash_file(&part)?;
        if h == hash {
            std::fs::rename(&part, self.tmp_file(&format!("{hash}.ready")))?;
            Ok(true)
        } else {
            let _ = std::fs::remove_file(&part);
            Ok(false)
        }
    }

    pub fn download_source(&self, path: &str, rev: u64) -> DownloadSource {
        let c = self.db.lock().unwrap();
        let Ok(Some(e)) = get_live(&c, path) else { return DownloadSource::NotFound };
        if e.kind != EntryKind::File {
            return DownloadSource::NotFound;
        }
        if rev == 0 || rev == e.rev {
            let abs = self.abs(path);
            return match std::fs::metadata(&abs) {
                Ok(m) if m.is_file() && m.len() == e.size && mtime_ms(&m) == e.mtime => DownloadSource::File { path: abs, size: e.size },
                _ => DownloadSource::Stale,
            };
        }
        let v: Option<(String, i64)> = c
            .query_row("SELECT hash,size FROM versions WHERE path=?1 AND rev=?2 ORDER BY id DESC LIMIT 1", params![path, rev as i64], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()
            .ok()
            .flatten();
        match v {
            Some((h, size)) if self.blob_path(&h).is_file() => DownloadSource::File { path: self.blob_path(&h), size: size as u64 },
            _ => DownloadSource::Stale,
        }
    }

    // ---------------- scanning (direct edits on the host) ----------------

    /// Full reconciliation of the folder against the database.
    pub fn scan(&self) -> Result<usize> {
        self.scan_paths(&[String::new()])
    }

    /// Reconciles the given paths (files or folders; empty string = everything), as one batch so that a
    /// rename shows up as a move.
    pub fn scan_paths(&self, prefixes: &[String]) -> Result<usize> {
        let n = {
            let mut c = self.db.lock().unwrap();
            self.scan_locked(&mut c, prefixes)?
        };
        if n > 0 {
            self.notify_if_advanced();
        }
        Ok(n)
    }

    fn walk(&self, rel: &str, found: &mut Vec<Found>, issues: &mut Vec<String>, keys: &mut HashMap<String, String>, ignore: &[String]) {
        let abs = if rel.is_empty() { self.root.clone() } else { self.abs(rel) };
        let Ok(m) = std::fs::symlink_metadata(&abs) else { return };
        if m.file_type().is_symlink() {
            issues.push(format!("{rel}: symlinks are not synced"));
            return;
        }
        if !rel.is_empty() {
            if is_ignored(rel, ignore) {
                return;
            }
            let key = path_key(rel);
            if let Some(other) = keys.get(&key) {
                if other != rel {
                    issues.push(format!("{rel}: differs from {other} only in case, skipped"));
                    return;
                }
            }
            keys.insert(key, rel.to_string());
            found.push(Found {
                rel: rel.to_string(),
                kind: if m.is_dir() { EntryKind::Dir } else { EntryKind::File },
                size: if m.is_dir() { 0 } else { m.len() },
                mtime: mtime_ms(&m),
                exec: is_exec(&m) && !m.is_dir(),
            });
        }
        if !m.is_dir() {
            return;
        }
        let Ok(rd) = std::fs::read_dir(&abs) else { return };
        let mut names: Vec<String> = vec![];
        for ent in rd.flatten() {
            let Some(name) = ent.file_name().to_str().map(str::to_string) else {
                issues.push(format!("{rel}/{:?}: not valid UTF-8, skipped", ent.file_name()));
                continue;
            };
            names.push(name);
        }
        names.sort();
        for name in names {
            let mut name = name;
            let nfc = normalize_path(&name).unwrap_or_else(|_| name.clone());
            if nfc != name && !nfc.contains('/') {
                // macOS APIs may hand out decomposed names: the sync uses NFC, rename on disk
                if std::fs::rename(abs.join(&name), abs.join(&nfc)).is_ok() {
                    name = nfc;
                }
            }
            if name.is_empty() || name == "." || name == ".." {
                continue;
            }
            let child = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            self.walk(&child, found, issues, keys, ignore);
        }
    }

    fn scan_locked(&self, conn: &mut Connection, prefixes: &[String]) -> Result<usize> {
        let ignore = self.ignore();
        if !self.root.is_dir() {
            // vanished root: never turn an unplugged disk into mass deletes
            bail!("sync folder {} is missing", self.root.display());
        }
        let mut found: Vec<Found> = vec![];
        let mut issues = vec![];
        let mut keys = HashMap::new();
        let mut scope: Vec<String> = vec![];
        for p in prefixes {
            let p = p.trim_matches('/').to_string();
            if scope.iter().any(|s| is_within(&p, s)) {
                continue;
            }
            scope.retain(|s| !is_within(s, &p));
            scope.push(p);
        }
        for p in &scope {
            self.walk(p, &mut found, &mut issues, &mut keys, &ignore);
        }
        if scope.iter().any(|p| p.is_empty()) {
            *self.issues.lock().unwrap() = issues;
        }

        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let seq: u64 = get_meta(&tx, "seq")?.and_then(|s| s.parse().ok()).unwrap_or(0);
        let mut cx = Cx { c: &tx, seq, author: HOST_AUTHOR.to_string(), now: now_secs() };

        let mut live: BTreeMap<String, Entry> = BTreeMap::new();
        for p in &scope {
            for e in live_under(&tx, p)? {
                live.insert(e.path.clone(), e);
            }
        }
        let found_paths: HashSet<&str> = found.iter().map(|f| f.rel.as_str()).collect();

        // classify
        let mut new_dirs: Vec<&Found> = vec![];
        let mut puts: Vec<(&Found, String, u64, bool)> = vec![]; // found, hash, size, is_new
        for f in &found {
            let cur = live.get(&f.rel);
            match (f.kind, cur) {
                (EntryKind::Dir, Some(e)) if e.kind == EntryKind::Dir => {}
                (EntryKind::Dir, _) => new_dirs.push(f),
                (EntryKind::File, Some(e)) if e.kind == EntryKind::File && e.size == f.size && e.mtime == f.mtime && e.exec == f.exec => {}
                (EntryKind::File, cur) => {
                    let abs = self.abs(&f.rel);
                    let Ok((hash, size)) = hash_file(&abs) else { continue };
                    let is_new = !matches!(cur, Some(e) if e.kind == EntryKind::File);
                    if let Some(e) = cur {
                        if e.kind == EntryKind::File && e.hash == hash {
                            // only mtime / exec moved: keep the revision, refresh the row
                            let mut n = e.clone();
                            n.size = size;
                            n.mtime = f.mtime;
                            n.exec = f.exec;
                            write_entry(&tx, &n, 0)?;
                            continue;
                        }
                    }
                    puts.push((f, hash, size, is_new));
                }
            }
        }
        let mut missing: Vec<&Entry> = live.values().filter(|e| !found_paths.contains(e.path.as_str())).collect();
        missing.sort_by(|a, b| b.path.cmp(&a.path));

        // moves: a missing file and a new file with the same content
        let mut moved_from: HashSet<String> = HashSet::new();
        let mut moves: Vec<(String, usize)> = vec![]; // from path, index into puts
        let mut by_hash: HashMap<(&str, u64), Vec<&Entry>> = HashMap::new();
        for e in missing.iter().filter(|e| e.kind == EntryKind::File) {
            by_hash.entry((e.hash.as_str(), e.size)).or_default().push(e);
        }
        for (i, (_, hash, size, is_new)) in puts.iter().enumerate() {
            if !*is_new {
                continue;
            }
            if let Some(cands) = by_hash.get_mut(&(hash.as_str(), *size)) {
                if let Some(src) = cands.pop() {
                    moved_from.insert(src.path.clone());
                    moves.push((src.path.clone(), i));
                }
            }
        }
        let move_targets: HashSet<usize> = moves.iter().map(|(_, i)| *i).collect();

        let mut changed = 0usize;
        // tombstones first (frees case-insensitive keys), children before parents
        for e in &missing {
            if moved_from.contains(&e.path) {
                continue;
            }
            self.tombstone(&mut cx, e)?;
            changed += 1;
        }
        for (from, i) in &moves {
            let (f, hash, size, _) = &puts[*i];
            if get_collision(&tx, &f.rel, Some(from))?.is_some() {
                issues_push(&self.issues, format!("{}: differs from another entry only in case, skipped", f.rel));
                continue;
            }
            let src = live.get(from).unwrap();
            self.tombstone(&mut cx, src)?;
            let rev = cx.next();
            let e = Entry {
                path: f.rel.clone(),
                kind: EntryKind::File,
                size: *size,
                mtime: f.mtime,
                hash: hash.clone(),
                exec: f.exec,
                rev,
                deleted: false,
                author: HOST_AUTHOR.into(),
            };
            write_entry(&tx, &e, 0)?;
            tx.execute("UPDATE versions SET path=?1 WHERE path=?2", params![f.rel, from])?;
            self.make_current(&mut cx, &e, &self.abs(&f.rel))?;
            changed += 1;
        }
        new_dirs.sort_by(|a, b| a.rel.cmp(&b.rel));
        for f in new_dirs {
            if get_collision(&tx, &f.rel, None)?.is_some() {
                issues_push(&self.issues, format!("{}: differs from another entry only in case, skipped", f.rel));
                continue;
            }
            let rev = cx.next();
            let e = Entry {
                path: f.rel.clone(),
                kind: EntryKind::Dir,
                size: 0,
                mtime: f.mtime,
                hash: String::new(),
                exec: false,
                rev,
                deleted: false,
                author: HOST_AUTHOR.into(),
            };
            write_entry(&tx, &e, 0)?;
            changed += 1;
        }
        for (i, (f, hash, size, _)) in puts.iter().enumerate() {
            if move_targets.contains(&i) {
                continue;
            }
            if get_collision(&tx, &f.rel, None)?.is_some() {
                issues_push(&self.issues, format!("{}: differs from another entry only in case, skipped", f.rel));
                continue;
            }
            // a directory entry replaced by a file: the old row is overwritten, children are gone
            let rev = cx.next();
            let e = Entry {
                path: f.rel.clone(),
                kind: EntryKind::File,
                size: *size,
                mtime: f.mtime,
                hash: hash.clone(),
                exec: f.exec,
                rev,
                deleted: false,
                author: HOST_AUTHOR.into(),
            };
            write_entry(&tx, &e, 0)?;
            self.make_current(&mut cx, &e, &self.abs(&f.rel))?;
            changed += 1;
        }
        if cx.seq != seq {
            set_meta(&tx, "seq", &cx.seq.to_string())?;
        }
        tx.commit()?;
        Ok(changed)
    }

    /// Single-path observation inside a commit (no move detection, no recursion for new folders).
    fn observe(&self, cx: &mut Cx, rel: &str) -> Result<()> {
        let abs = self.abs(rel);
        let cur = get_live(cx.c, rel)?;
        match std::fs::symlink_metadata(&abs) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if cur.is_some() {
                    self.tombstone_tree(cx, rel)?;
                }
            }
            Err(_) => {}
            Ok(m) if m.file_type().is_symlink() => {}
            Ok(m) if m.is_dir() => {
                if !matches!(&cur, Some(e) if e.kind == EntryKind::Dir) && get_collision(cx.c, rel, None)?.is_none() {
                    let rev = cx.next();
                    let e = Entry {
                        path: rel.to_string(),
                        kind: EntryKind::Dir,
                        size: 0,
                        mtime: mtime_ms(&m),
                        hash: String::new(),
                        exec: false,
                        rev,
                        deleted: false,
                        author: HOST_AUTHOR.into(),
                    };
                    write_entry(cx.c, &e, 0)?;
                }
            }
            Ok(m) => {
                let same = matches!(&cur, Some(e) if e.kind == EntryKind::File && e.size == m.len() && e.mtime == mtime_ms(&m));
                if same || get_collision(cx.c, rel, None)?.is_some() {
                    return Ok(());
                }
                let (hash, size) = hash_file(&abs)?;
                if let Some(e) = &cur {
                    if e.kind == EntryKind::File && e.hash == hash {
                        let mut n = e.clone();
                        n.size = size;
                        n.mtime = mtime_ms(&m);
                        n.exec = is_exec(&m);
                        write_entry(cx.c, &n, 0)?;
                        return Ok(());
                    }
                }
                let rev = cx.next();
                let e = Entry {
                    path: rel.to_string(),
                    kind: EntryKind::File,
                    size,
                    mtime: mtime_ms(&m),
                    hash,
                    exec: is_exec(&m),
                    rev,
                    deleted: false,
                    author: HOST_AUTHOR.into(),
                };
                write_entry(cx.c, &e, 0)?;
                self.make_current(cx, &e, &abs)?;
            }
        }
        Ok(())
    }

    /// Observation of everything below a folder (before deciding it is empty / moving it).
    fn observe_tree(&self, cx: &mut Cx, rel: &str) -> Result<()> {
        let ignore = self.ignore();
        let mut found = vec![];
        let mut issues = vec![];
        let mut keys = HashMap::new();
        self.walk(rel, &mut found, &mut issues, &mut keys, &ignore);
        let seen: HashSet<String> = found.iter().map(|f| f.rel.clone()).collect();
        let mut dirs: Vec<&Found> = found.iter().filter(|f| f.kind == EntryKind::Dir).collect();
        dirs.sort_by(|a, b| a.rel.cmp(&b.rel));
        for f in dirs.iter().chain(found.iter().filter(|f| f.kind == EntryKind::File).collect::<Vec<_>>().iter()) {
            self.observe(cx, &f.rel)?;
        }
        let mut gone: Vec<Entry> = live_under(cx.c, rel)?.into_iter().filter(|e| !seen.contains(&e.path)).collect();
        gone.sort_by(|a, b| b.path.cmp(&a.path));
        for e in gone {
            self.tombstone(cx, &e)?;
        }
        Ok(())
    }

    // ---------------- backups (§6) ----------------

    /// Called when `e` has just become the current version of its path. Preserves the content in the
    /// blob store (copy-on-write clone where possible) and records a version, per the sync's policy.
    fn make_current(&self, cx: &mut Cx, e: &Entry, src: &Path) -> Result<()> {
        let policy = self.policy();
        if !policy.enabled() || e.kind != EntryKind::File {
            return Ok(());
        }
        self.ensure_blob(cx.c, &e.hash, e.size, src)?;
        if let BackupPolicy::File { keep, min_interval_secs, .. } = policy {
            let last: Option<(i64, String, String, i64)> = cx
                .c
                .query_row("SELECT id,hash,author,created FROM versions WHERE path=?1 ORDER BY id DESC LIMIT 1", [&e.path], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })
                .optional()?;
            if let Some((id, hash, author, created)) = last {
                if hash == e.hash {
                    cx.c.execute("UPDATE versions SET rev=?1, mtime=?2, author=?3 WHERE id=?4", params![e.rev as i64, e.mtime, e.author, id])?;
                    return Ok(());
                }
                if author == e.author && cx.now - created < min_interval_secs as i64 {
                    // an editor that saves every few seconds must not burn the history
                    cx.c.execute("DELETE FROM versions WHERE id=?1", [id])?;
                }
            }
            cx.c.execute(
                "INSERT INTO versions(path,rev,hash,size,mtime,author,created) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![e.path, e.rev as i64, e.hash, e.size as i64, e.mtime, e.author, cx.now],
            )?;
            cx.c.execute(
                "DELETE FROM versions WHERE path=?1 AND id NOT IN (SELECT id FROM versions WHERE path=?1 ORDER BY id DESC LIMIT ?2)",
                params![e.path, keep as i64],
            )?;
        }
        Ok(())
    }

    /// Makes sure `<store>/blobs/..` holds `hash`, taking it from `src`. Returns false if `src` no
    /// longer has that content (changed meanwhile).
    fn ensure_blob(&self, c: &Connection, hash: &str, size: u64, src: &Path) -> Result<bool> {
        let blob = self.blob_path(hash);
        if blob.is_file() {
            c.execute("INSERT OR IGNORE INTO blobs(hash,size) VALUES(?1,?2)", params![hash, size as i64])?;
            return Ok(true);
        }
        let tmp = self.tmp_file(&format!("{}.blob", random_id()));
        let (h, n) = if try_clone(src, &tmp) {
            hash_file(&tmp)?
        } else {
            copy_hashing(src, &tmp)?
        };
        if let Some(dir) = blob.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let final_blob = self.blob_path(&h);
        if let Some(dir) = final_blob.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if final_blob.is_file() {
            let _ = std::fs::remove_file(&tmp);
        } else {
            move_file(&tmp, &final_blob)?;
        }
        c.execute("INSERT OR IGNORE INTO blobs(hash,size) VALUES(?1,?2)", params![h, n as i64])?;
        Ok(h == hash)
    }

    /// Preserves every current file that has no blob yet (a backup strategy was just switched on).
    pub fn preserve_current(&self) -> Result<usize> {
        if !self.policy().enabled() {
            return Ok(0);
        }
        let c = self.db.lock().unwrap();
        let mut n = 0;
        for e in live_under(&c, "")? {
            if e.kind == EntryKind::File && !self.blob_path(&e.hash).is_file() {
                let abs = self.abs(&e.path);
                if let Ok(m) = std::fs::metadata(&abs) {
                    if m.len() == e.size && mtime_ms(&m) == e.mtime && self.ensure_blob(&c, &e.hash, e.size, &abs)? {
                        n += 1;
                    }
                }
            }
        }
        Ok(n)
    }

    /// Takes a snapshot now. Copies nothing: the content is already in the blob store.
    pub fn snapshot_now(&self) -> Result<u64> {
        if !matches!(self.policy(), BackupPolicy::Time { .. }) {
            bail!("snapshots need the time-based backup strategy (this sync uses {})", self.policy().describe());
        }
        self.preserve_current()?;
        let mut c = self.db.lock().unwrap();
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let seq: i64 = get_meta(&tx, "seq")?.and_then(|s| s.parse().ok()).unwrap_or(0);
        tx.execute("INSERT INTO snapshots(seq,created) VALUES(?1,?2)", params![seq, now_secs()])?;
        let id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO snapshot_entries(snapshot,path,kind,hash,size,mtime,exec)
             SELECT ?1,path,kind,hash,size,mtime,exec FROM entries WHERE deleted=0",
            [id],
        )?;
        tx.commit()?;
        drop(c);
        self.prune()?;
        Ok(id as u64)
    }

    pub fn snapshots(&self) -> Vec<SnapshotInfo> {
        let c = self.db.lock().unwrap();
        let Ok(mut st) = c.prepare(
            "SELECT s.id,s.seq,s.created,(SELECT COUNT(*) FROM snapshot_entries e WHERE e.snapshot=s.id) FROM snapshots s ORDER BY s.id",
        ) else {
            return vec![];
        };
        st.query_map([], |r| {
            Ok(SnapshotInfo { id: r.get::<_, i64>(0)? as u64, seq: r.get::<_, i64>(1)? as u64, created: r.get(2)?, entries: r.get::<_, i64>(3)? as u64 })
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
    }

    pub fn versions(&self, path: &str) -> Vec<VersionInfo> {
        let c = self.db.lock().unwrap();
        let Ok(mut st) = c.prepare("SELECT rev,hash,size,mtime,author,created FROM versions WHERE path=?1 ORDER BY id DESC") else {
            return vec![];
        };
        st.query_map([path], |r| {
            Ok(VersionInfo {
                rev: r.get::<_, i64>(0)? as u64,
                hash: r.get(1)?,
                size: r.get::<_, i64>(2)? as u64,
                mtime: r.get(3)?,
                author: r.get(4)?,
                created: r.get(5)?,
            })
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
    }

    /// Drops what the policy no longer keeps and sweeps unreferenced blobs.
    pub fn prune(&self) -> Result<()> {
        let policy = self.policy();
        let c = self.db.lock().unwrap();
        let now = now_secs();
        match &policy {
            BackupPolicy::None => {
                c.execute_batch("DELETE FROM versions; DELETE FROM snapshots; DELETE FROM snapshot_entries;")?;
            }
            BackupPolicy::Time { keep, .. } => {
                c.execute("DELETE FROM versions", [])?;
                c.execute("DELETE FROM snapshots WHERE id NOT IN (SELECT id FROM snapshots ORDER BY id DESC LIMIT ?1)", [*keep as i64])?;
                c.execute("DELETE FROM snapshot_entries WHERE snapshot NOT IN (SELECT id FROM snapshots)", [])?;
            }
            BackupPolicy::File { deleted_days, keep, .. } => {
                c.execute_batch("DELETE FROM snapshots; DELETE FROM snapshot_entries;")?;
                let cutoff = now - *deleted_days as i64 * 86_400;
                c.execute(
                    "DELETE FROM versions WHERE path IN (SELECT path FROM entries WHERE deleted=1 AND deleted_at < ?1)",
                    [cutoff],
                )?;
                // paths without any entry at all (tombstone purged) lose their history too
                c.execute("DELETE FROM versions WHERE path NOT IN (SELECT path FROM entries)", [])?;
                // policy.keep may have been lowered
                c.execute(
                    "DELETE FROM versions WHERE id IN (
                        SELECT id FROM (SELECT id, ROW_NUMBER() OVER (PARTITION BY path ORDER BY id DESC) AS n FROM versions) WHERE n > ?1)",
                    [*keep as i64],
                )?;
            }
        }
        self.gc(&c)
    }

    /// Deletes blobs that no live entry, version or snapshot refers to.
    fn gc(&self, c: &Connection) -> Result<()> {
        let keep_current = self.policy().enabled();
        let mut referenced: HashSet<String> = HashSet::new();
        let mut collect = |sql: &str| -> Result<()> {
            let mut st = c.prepare(sql)?;
            for h in st.query_map([], |r| r.get::<_, String>(0))? {
                referenced.insert(h?);
            }
            Ok(())
        };
        if keep_current {
            collect("SELECT hash FROM entries WHERE deleted=0 AND kind=0")?;
        }
        collect("SELECT hash FROM versions")?;
        collect("SELECT hash FROM snapshot_entries WHERE kind=0")?;
        let mut st = c.prepare("SELECT hash FROM blobs")?;
        let all: Vec<String> = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        for h in all {
            if !referenced.contains(&h) {
                let _ = std::fs::remove_file(self.blob_path(&h));
                c.execute("DELETE FROM blobs WHERE hash=?1", [&h])?;
            }
        }
        Ok(())
    }

    /// Forgets tombstones older than `max_age_secs`; members whose cursor predates them reconcile in
    /// full (`ResetRequired`).
    pub fn purge_tombstones(&self, max_age_secs: i64) -> Result<usize> {
        let c = self.db.lock().unwrap();
        let cutoff = now_secs() - max_age_secs;
        let max_purged: Option<i64> =
            c.query_row("SELECT MAX(rev) FROM entries WHERE deleted=1 AND deleted_at < ?1", [cutoff], |r| r.get(0))?;
        let Some(m) = max_purged else { return Ok(0) };
        let n = c.execute("DELETE FROM entries WHERE deleted=1 AND deleted_at < ?1", [cutoff])?;
        let floor: i64 = get_meta(&c, "floor")?.and_then(|s| s.parse().ok()).unwrap_or(0);
        set_meta(&c, "floor", &floor.max(m).to_string())?;
        Ok(n)
    }

    /// Periodic housekeeping: tombstone purge, due snapshots, pruning, stale staging files.
    pub fn maintenance(&self) -> Result<()> {
        let now = now_secs();
        self.purge_tombstones(TOMBSTONE_DAYS * 86_400)?;
        if let BackupPolicy::Time { every_hours, .. } = self.policy() {
            let last: Option<i64> = {
                let c = self.db.lock().unwrap();
                c.query_row("SELECT MAX(created) FROM snapshots", [], |r| r.get(0))?
            };
            if last.is_none_or(|l| now - l >= every_hours as i64 * 3600) {
                self.snapshot_now()?;
            }
        }
        self.prune()?;
        if let Ok(rd) = std::fs::read_dir(self.store.join("tmp")) {
            for ent in rd.flatten() {
                let old = ent
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age.as_secs() as i64 > STAGE_MAX_AGE_SECS);
                if old {
                    let _ = std::fs::remove_file(ent.path());
                }
            }
        }
        Ok(())
    }

    /// Restores a file version or part of a snapshot as a normal new commit by `author`, so it
    /// reaches every member. Files are written from the blob store; folders are re-created.
    pub fn restore(&self, author: &str, what: &RestoreWhat, as_copy: bool) -> Result<Vec<ChangeResult>> {
        // (path, hash, size, mtime, exec) to bring back
        let mut targets: Vec<(String, EntryKind, String, u64, i64, bool)> = vec![];
        {
            let c = self.db.lock().unwrap();
            match what {
                RestoreWhat::File { path, rev } => {
                    let v: Option<(String, i64, i64)> = c
                        .query_row("SELECT hash,size,mtime FROM versions WHERE path=?1 AND rev=?2 ORDER BY id DESC LIMIT 1", params![path, *rev as i64], |r| {
                            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                        })
                        .optional()?;
                    let Some((hash, size, mtime)) = v else { bail!("no version {rev} of {path}") };
                    targets.push((path.clone(), EntryKind::File, hash, size as u64, mtime, false));
                }
                RestoreWhat::Snapshot { id, path } => {
                    let mut st = c.prepare(
                        "SELECT path,kind,hash,size,mtime,exec FROM snapshot_entries WHERE snapshot=?1 ORDER BY path",
                    )?;
                    let rows = st.query_map([*id as i64], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            if r.get::<_, i64>(1)? == 0 { EntryKind::File } else { EntryKind::Dir },
                            r.get::<_, String>(2)?,
                            r.get::<_, i64>(3)? as u64,
                            r.get::<_, i64>(4)?,
                            r.get::<_, i64>(5)? != 0,
                        ))
                    })?;
                    for r in rows {
                        let t = r?;
                        if is_within(&t.0, path) {
                            targets.push(t);
                        }
                    }
                    if targets.is_empty() {
                        bail!("snapshot {id} has nothing at {path:?}");
                    }
                }
            }
        }
        let stamp = {
            let (y, m, d, _, _) = civil(now_secs());
            format!("{y:04}-{m:02}-{d:02}")
        };
        let mut changes: Vec<Change> = vec![];
        for (path, kind, hash, size, mtime, exec) in targets {
            let dest = if as_copy { restored_name(&path, &stamp) } else { path.clone() };
            let cur = self.entry(&dest);
            match kind {
                EntryKind::Dir => {
                    if !matches!(&cur, Some(e) if !e.deleted) {
                        changes.push(Change::Mkdir { path: dest });
                    }
                }
                EntryKind::File => {
                    if matches!(&cur, Some(e) if !e.deleted && e.hash == hash) {
                        continue;
                    }
                    let base_rev = cur.map(|e| e.rev).unwrap_or(0);
                    changes.push(Change::Put { path: dest, hash, size, mtime, exec, base_rev });
                }
            }
        }
        self.commit(Access::ReadWrite, author, &changes)
    }
}

fn issues_push(m: &Mutex<Vec<String>>, s: String) {
    m.lock().unwrap().push(s);
}

/// `name (restored 2026-10-11).ext`
pub fn restored_name(path: &str, stamp: &str) -> String {
    let (dir, name) = match path.rsplit_once('/') {
        Some((d, n)) => (Some(d), n),
        None => (None, path),
    };
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    let n = format!("{stem} (restored {stamp}){ext}");
    match dir {
        Some(d) => format!("{d}/{n}"),
        None => n,
    }
}
