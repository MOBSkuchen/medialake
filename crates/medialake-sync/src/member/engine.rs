//! The member sync engine (§3–§5): pull the host's changes, push local ones, resolve conflicts.
//! Platform-free; a [`Frontend`] shows the result and a [`Transport`] reaches the host.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use medialake_proto::sync::*;

use super::db::{Base, MemberDb};
use super::frontend::{EntryStatus, Frontend, LocalItem};
use super::transport::{ChangesReply, Fetch, Transport, Upload};
use crate::util::*;

/// A single push that would delete more than this many entries, or more than half of the sync (and at
/// least `MASS_DELETE_MIN`), is held back until the user confirms it once (§5).
pub const MASS_DELETE_MAX: usize = 500;
pub const MASS_DELETE_MIN: usize = 10;
const PAGE: u32 = 500;
const ROUNDS: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictPolicy {
    ServerWins,
    Ask,
}

impl ConflictPolicy {
    pub fn parse(s: &str) -> Option<ConflictPolicy> {
        match s {
            "server_wins" | "server" => Some(ConflictPolicy::ServerWins),
            "ask" => Some(ConflictPolicy::Ask),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            ConflictPolicy::ServerWins => "server_wins",
            ConflictPolicy::Ask => "ask",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// Commit the local version on top of the host's.
    Mine,
    /// Drop the local edit and take the host's version.
    Server,
    /// Keep both: the local version becomes a conflict copy.
    Both,
}

impl Choice {
    pub fn parse(s: &str) -> Option<Choice> {
        match s {
            "mine" => Some(Choice::Mine),
            "server" => Some(Choice::Server),
            "both" => Some(Choice::Both),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub struct EngineConfig {
    /// Shown in conflict names and on the host.
    pub device: String,
    pub policy: ConflictPolicy,
    pub access: Access,
    /// Holds `state.db` and `tmp/`.
    pub state_dir: PathBuf,
    /// Files modified more recently than this are not pushed yet (still being written).
    pub settle_ms: i64,
    pub ignore: Vec<String>,
}

#[derive(Debug, Default, Clone)]
pub struct Report {
    pub pulled: usize,
    pub downloaded: usize,
    pub pushed: usize,
    pub uploaded: usize,
    pub removed_local: usize,
    pub conflicts: usize,
    /// A push with this many deletes is waiting for `confirm_deletes`.
    pub held_deletes: Option<usize>,
    /// Something was skipped for now (still being written, host stale): run again soon.
    pub retry: bool,
    pub errors: Vec<String>,
}

impl Report {
    fn activity(&self) -> usize {
        self.pulled + self.downloaded + self.pushed + self.uploaded + self.removed_local + self.conflicts
    }
}

#[derive(Debug, Clone)]
pub struct ConflictInfo {
    pub path: String,
    /// The host deleted it while it was edited here.
    pub host_deleted: bool,
    pub since: i64,
}

pub enum Wake {
    Local(Vec<String>),
    Remote,
    /// Look at everything again, from the start ("Verify").
    Verify,
    /// Runs on the engine's own thread, between rounds (conflict resolution, setting changes).
    Command(Box<dyn FnOnce(&mut Engine) + Send>),
    Stop,
}

/// The host moved on while we were fetching: pull again.
#[derive(Debug)]
struct RemoteMoved(String);

impl std::fmt::Display for RemoteMoved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} changed on the host while it was being fetched", self.0)
    }
}

impl std::error::Error for RemoteMoved {}

struct Local {
    kind: EntryKind,
    size: u64,
    mtime: i64,
    exec: bool,
    /// Files only.
    hash: String,
}

pub struct Engine {
    cfg: EngineConfig,
    db: MemberDb,
    fe: Arc<dyn Frontend>,
    tr: Arc<dyn Transport>,
    /// Paths the watcher saw; `None` = look at everything.
    dirty: Option<HashSet<String>>,
    rejected: HashSet<(String, String)>,
}

impl Engine {
    pub fn new(cfg: EngineConfig, fe: Arc<dyn Frontend>, tr: Arc<dyn Transport>) -> Result<Engine> {
        std::fs::create_dir_all(cfg.state_dir.join("tmp"))?;
        let db = MemberDb::open(&cfg.state_dir.join("state.db"))?;
        Ok(Engine { cfg, db, fe, tr, dirty: None, rejected: HashSet::new() })
    }

    pub fn access(&self) -> Access {
        self.cfg.access
    }

    pub fn set_policy(&mut self, p: ConflictPolicy) {
        self.cfg.policy = p;
    }

    pub fn cursor(&self) -> u64 {
        self.db.cursor()
    }

    pub fn base(&self, path: &str) -> Option<Base> {
        self.db.base(path)
    }

    pub fn conflicts(&self) -> Vec<ConflictInfo> {
        self.db.conflicts().into_iter().map(|(e, since)| ConflictInfo { path: e.path, host_deleted: e.deleted, since }).collect()
    }

    /// Lets the one pending mass delete through.
    pub fn confirm_deletes(&self) -> Result<()> {
        self.db.set_meta("confirm_deletes", "1")
    }

    /// Looks at everything again on the next run (the "Verify" button).
    pub fn verify(&mut self) -> Result<()> {
        self.dirty = None;
        self.db.set_cursor(0)
    }

    pub fn mark_dirty(&mut self, paths: &[String]) {
        if let Some(d) = self.dirty.as_mut() {
            d.extend(paths.iter().cloned());
        }
    }

    fn tmp(&self, name: &str) -> PathBuf {
        self.cfg.state_dir.join("tmp").join(name)
    }

    // ---------------- local state ----------------

    fn file_hash(&self, rel: &str, size: u64, mtime: i64) -> Result<String> {
        if let Some(c) = self.db.local_cache(rel) {
            if c.size == size && c.mtime == mtime {
                return Ok(c.hash);
            }
        }
        let (h, n) = hash_file(&self.fe.local_path(rel)).with_context(|| format!("reading {rel}"))?;
        self.db.set_local_cache(rel, n, mtime, &h)?;
        Ok(h)
    }

    fn local_state(&self, rel: &str) -> Result<Option<Local>> {
        let abs = self.fe.local_path(rel);
        let m = match std::fs::symlink_metadata(&abs) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if m.file_type().is_symlink() {
            return Ok(None);
        }
        if m.is_dir() {
            return Ok(Some(Local { kind: EntryKind::Dir, size: 0, mtime: mtime_ms(&m), exec: false, hash: String::new() }));
        }
        let (size, mtime) = (m.len(), mtime_ms(&m));
        let hash = self.file_hash(rel, size, mtime)?;
        Ok(Some(Local { kind: EntryKind::File, size, mtime, exec: is_exec(&m), hash }))
    }

    // ---------------- one round ----------------

    /// A complete pass: looks at every local item, then pulls, pushes and repeats until nothing moves.
    pub fn sync_once(&mut self) -> Result<Report> {
        self.dirty = None;
        self.round()
    }

    /// Like [`Engine::sync_once`], but only scans the paths the watcher reported since the last round.
    pub fn round(&mut self) -> Result<Report> {
        let mut rep = Report::default();
        self.fe.check_root()?;
        for _ in 0..ROUNDS {
            let before = rep.activity();
            self.pull(&mut rep)?;
            if rep.retry {
                break;
            }
            if self.cfg.access == Access::ReadWrite {
                self.push(&mut rep)?;
            } else {
                self.revert_local(&mut rep)?;
            }
            if rep.activity() == before {
                break;
            }
        }
        Ok(rep)
    }

    // ---------------- pull ----------------

    fn pull(&mut self, rep: &mut Report) -> Result<()> {
        let mut full = self.db.cursor() == 0;
        let mut seen: HashSet<String> = HashSet::new();
        loop {
            match self.tr.changes(&self.db.epoch(), self.db.cursor(), PAGE)? {
                ChangesReply::Reset { epoch } => {
                    // the host's database was rebuilt or our cursor predates its history: start over
                    // with revisions forgotten (hashes still tell what changed here)
                    self.db.forget_revs()?;
                    self.db.set_meta("epoch", &epoch)?;
                    self.db.set_cursor(0)?;
                    full = true;
                    seen.clear();
                }
                ChangesReply::Page { epoch, entries, next_cursor, more, .. } => {
                    if self.db.epoch().is_empty() {
                        self.db.set_meta("epoch", &epoch)?;
                    }
                    for e in &entries {
                        if full {
                            seen.insert(e.path.clone());
                        }
                        match self.apply_remote(e, rep) {
                            Ok(()) => rep.pulled += 1,
                            Err(err) if err.downcast_ref::<RemoteMoved>().is_some() => {
                                rep.retry = true;
                                rep.errors.push(err.to_string());
                                return Ok(()); // cursor stays: the page is delivered again
                            }
                            Err(err) => {
                                rep.errors.push(format!("{}: {err:#}", e.path));
                                rep.retry = true;
                                return Ok(());
                            }
                        }
                    }
                    self.db.set_cursor(next_cursor)?;
                    if !more {
                        break;
                    }
                }
            }
        }
        if full {
            // entries we knew that the host no longer lists were deleted there
            let mut gone: Vec<Base> = self.db.bases_under("").into_iter().filter(|b| !seen.contains(&b.path)).collect();
            gone.sort_by(|a, b| b.path.cmp(&a.path));
            for b in gone {
                let tomb = Entry {
                    path: b.path.clone(),
                    kind: b.kind,
                    size: 0,
                    mtime: 0,
                    hash: String::new(),
                    exec: false,
                    rev: u64::MAX,
                    deleted: true,
                    author: String::new(),
                };
                if let Err(e) = self.apply_remote(&tomb, rep) {
                    rep.errors.push(format!("{}: {e:#}", b.path));
                }
            }
        }
        Ok(())
    }

    fn clean(l: &Local, b: &Base) -> bool {
        match (l.kind, b.kind) {
            (EntryKind::Dir, EntryKind::Dir) => true,
            (EntryKind::File, EntryKind::File) => l.hash == b.hash,
            _ => false,
        }
    }

    /// Applies one host entry against base and local state (§4.4, §5).
    fn apply_remote(&mut self, e: &Entry, rep: &mut Report) -> Result<()> {
        let path = e.path.as_str();
        if is_ignored(path, &self.cfg.ignore) {
            return Ok(());
        }
        if cfg!(windows) && unrepresentable_on_windows(path) {
            rep.errors.push(format!("{path}: cannot be synced to this device"));
            return Ok(());
        }
        let base = self.db.base(path);
        if let Some(b) = &base {
            if b.rev > 0 && e.rev != u64::MAX && e.rev <= b.rev {
                return Ok(()); // already known (the echo of our own commit)
            }
        }
        let local = self.local_state(path)?;

        if self.db.conflict(path).is_some() {
            // a pending "ask" conflict: it settles itself if both sides now agree, else it learns the
            // newer host state
            let same = match (&local, e.deleted) {
                (None, true) => true,
                (Some(l), false) => l.kind == e.kind && (l.kind == EntryKind::Dir || l.hash == e.hash),
                _ => false,
            };
            if same {
                self.db.clear_conflict(path)?;
                self.adopt(e, local.as_ref())?;
            } else if e.rev != u64::MAX {
                self.db.set_conflict(e, now_secs())?;
            }
            return Ok(());
        }

        if e.deleted {
            match (&base, &local) {
                (_, None) => {
                    self.db.drop_base(path)?;
                }
                (None, Some(_)) => {} // a new local item the host never knew: it will be pushed
                (Some(b), Some(l)) if Self::clean(l, b) => {
                    if self.fe.remove(path, l.kind == EntryKind::Dir)? {
                        rep.removed_local += 1;
                        self.db.drop_base(path)?;
                    } else {
                        // a folder with new local files in it survives; it is pushed back
                        self.db.drop_base(path)?;
                    }
                }
                (Some(_), Some(l)) if l.kind == EntryKind::Dir => {
                    self.db.drop_base(path)?;
                }
                (Some(_), Some(_)) => {
                    // edited here, deleted there: the edit loses but is kept
                    self.conflict(e, rep)?;
                }
            }
            return Ok(());
        }

        match (&local, e.kind) {
            (None, EntryKind::Dir) => {
                self.fe.make_dir(path)?;
                self.db.set_base(&Base::from_entry(e))?;
                rep.downloaded += 1;
            }
            (None, EntryKind::File) => {
                self.install_file(e, rep)?;
            }
            (Some(l), EntryKind::Dir) if l.kind == EntryKind::Dir => {
                self.db.set_base(&Base::from_entry(e))?;
            }
            (Some(l), EntryKind::File) if l.kind == EntryKind::File => {
                if l.hash == e.hash {
                    self.adopt(e, Some(l))?;
                } else if base.as_ref().is_some_and(|b| b.kind == EntryKind::File && b.hash == e.hash) {
                    // the host's content is what we last had: only this side changed. Keep the edit
                    // and move the base to the host's revision so that the push is accepted.
                    self.db.set_base(&Base::from_entry(e))?;
                } else if base.as_ref().is_some_and(|b| Self::clean(l, b)) {
                    self.install_file(e, rep)?;
                } else {
                    self.conflict(e, rep)?;
                }
            }
            // a file here and a folder there, or the other way round
            (Some(_), _) => self.conflict(e, rep)?,
        }
        Ok(())
    }

    /// Both sides have the same content: only the base moves.
    fn adopt(&self, e: &Entry, local: Option<&Local>) -> Result<()> {
        if e.deleted {
            return self.db.drop_base(&e.path);
        }
        self.db.set_base(&Base::from_entry(e))?;
        if let Some(l) = local {
            if l.kind == EntryKind::File {
                self.db.set_local_cache(&e.path, l.size, l.mtime, &l.hash)?;
            }
        }
        Ok(())
    }

    fn fetch(&self, e: &Entry) -> Result<PathBuf> {
        if e.size == 0 {
            let p = self.tmp(&format!("{}.empty", random_id()));
            std::fs::File::create(&p)?;
            return Ok(p);
        }
        let part = self.tmp(&format!("{}.part", e.hash));
        for _ in 0..2 {
            match self.tr.download(&e.path, if e.rev == u64::MAX { 0 } else { e.rev }, &part)? {
                Fetch::Done => {
                    let (h, _) = hash_file(&part)?;
                    if h == e.hash {
                        return Ok(part);
                    }
                    // a resumed part that does not match: start over
                    let _ = std::fs::remove_file(&part);
                }
                Fetch::Stale | Fetch::NotFound => return Err(anyhow!(RemoteMoved(e.path.clone()))),
            }
        }
        bail!("{}: downloaded content does not match its hash", e.path)
    }

    /// Downloads, verifies and places a file, replacing a clean local one, and records the base.
    fn install_file(&self, e: &Entry, rep: &mut Report) -> Result<()> {
        let staged = self.fetch(e)?;
        self.fe.place_file(&e.path, &staged, e.mtime, e.exec)?;
        let abs = self.fe.local_path(&e.path);
        let m = std::fs::metadata(&abs)?;
        self.db.set_local_cache(&e.path, m.len(), mtime_ms(&m), &e.hash)?;
        self.db.set_base(&Base::from_entry(e))?;
        self.fe.set_status(&e.path, EntryStatus::InSync);
        rep.downloaded += 1;
        Ok(())
    }

    /// The host's version and the local one cannot both stay at this path (§5).
    fn conflict(&mut self, e: &Entry, rep: &mut Report) -> Result<()> {
        rep.conflicts += 1;
        match self.cfg.policy {
            ConflictPolicy::Ask => {
                self.fe.set_status(&e.path, EntryStatus::Conflict);
                self.db.set_conflict(e, now_secs())
            }
            ConflictPolicy::ServerWins => {
                self.keep_both(e, rep)?;
                Ok(())
            }
        }
    }

    /// Renames the local item to a conflict name (it becomes a new file, uploaded and visible to
    /// everyone) and installs the host's version at the original path.
    fn keep_both(&mut self, e: &Entry, rep: &mut Report) -> Result<String> {
        let copy = self.unique_conflict_name(&e.path)?;
        self.fe.rename(&e.path, &copy)?;
        if let Some(c) = self.db.local_cache(&e.path) {
            self.db.set_local_cache(&copy, c.size, c.mtime, &c.hash)?;
        }
        if self.cfg.access != Access::ReadWrite {
            self.db.add_local_only(&copy)?;
        }
        self.db.drop_base(&e.path)?;
        self.install_remote(e, rep)?;
        Ok(copy)
    }

    fn unique_conflict_name(&self, path: &str) -> Result<String> {
        let t = now_secs();
        for n in 0..1000 {
            let c = conflict_name(path, &self.cfg.device, t + n * 60);
            if !self.fe.local_path(&c).exists() && self.db.base(&c).is_none() {
                return Ok(c);
            }
        }
        bail!("no free conflict name for {path}")
    }

    /// Makes the local side equal the host's entry, discarding what is there.
    fn install_remote(&mut self, e: &Entry, rep: &mut Report) -> Result<()> {
        let local = self.local_state(&e.path)?;
        if e.deleted {
            if let Some(l) = &local {
                if self.fe.remove(&e.path, l.kind == EntryKind::Dir)? {
                    rep.removed_local += 1;
                }
            }
            return self.db.drop_base(&e.path);
        }
        match (&local, e.kind) {
            (Some(l), EntryKind::Dir) if l.kind == EntryKind::File => {
                self.fe.remove(&e.path, false)?;
            }
            (Some(l), EntryKind::File) if l.kind == EntryKind::Dir => {
                if !self.fe.remove(&e.path, true)? {
                    bail!("{}: a folder with files in it is in the way", e.path);
                }
            }
            _ => {}
        }
        match e.kind {
            EntryKind::Dir => {
                self.fe.make_dir(&e.path)?;
                self.db.set_base(&Base::from_entry(e))?;
                Ok(())
            }
            EntryKind::File => self.install_file(e, rep),
        }
    }

    // ---------------- conflicts (ask) ----------------

    /// Settles a pending conflict. Returns false if the host changed the file again meanwhile: the
    /// conflict stays, updated by the next pull, and can be resolved once more.
    pub fn resolve(&mut self, path: &str, choice: Choice) -> Result<bool> {
        match self.resolve_inner(path, choice) {
            Ok(()) => Ok(true),
            Err(e) if e.downcast_ref::<RemoteMoved>().is_some() => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn resolve_inner(&mut self, path: &str, choice: Choice) -> Result<()> {
        let Some(host) = self.db.conflict(path) else { bail!("{path} has no pending conflict") };
        let mut rep = Report::default();
        match choice {
            Choice::Server => {
                self.install_remote(&host, &mut rep)?;
            }
            Choice::Both => {
                if self.local_state(path)?.is_some() {
                    self.keep_both(&host, &mut rep)?;
                } else {
                    self.install_remote(&host, &mut rep)?;
                }
            }
            Choice::Mine => {
                // commit the local version on top of the host's current revision
                if host.deleted {
                    self.db.drop_base(path)?;
                } else {
                    let local = self.local_state(path)?;
                    let mut b = Base::from_entry(&host);
                    // the base hash must differ from the local content so that it is pushed
                    if let Some(l) = &local {
                        if l.kind == EntryKind::File && l.hash == b.hash {
                            self.db.clear_conflict(path)?;
                            return self.db.set_base(&b);
                        }
                    }
                    if b.kind == EntryKind::File && b.hash.is_empty() {
                        b.hash = "0".into();
                    }
                    self.db.set_base(&b)?;
                }
            }
        }
        self.db.clear_conflict(path)?;
        self.fe.set_status(path, EntryStatus::InSync);
        self.dirty = None;
        Ok(())
    }

    // ---------------- push ----------------

    fn scopes(&self) -> Vec<String> {
        match &self.dirty {
            None => vec![String::new()],
            Some(d) if d.is_empty() => vec![],
            Some(d) => {
                let mut v: Vec<String> = vec![];
                let mut all: Vec<&String> = d.iter().collect();
                all.sort();
                for p in all {
                    if !v.iter().any(|s| is_within(p, s)) {
                        v.push(p.clone());
                    }
                }
                v
            }
        }
    }

    fn push(&mut self, rep: &mut Report) -> Result<()> {
        let scopes = self.scopes();
        let full = scopes.iter().any(|s| s.is_empty());
        let now = now_ms();

        let mut items: Vec<LocalItem> = vec![];
        let mut bases: Vec<Base> = vec![];
        for s in &scopes {
            items.extend(self.fe.scan(s, &self.cfg.ignore)?);
            bases.extend(self.db.bases_under(s));
        }
        items.sort_by(|a, b| a.path.cmp(&b.path));
        items.dedup_by(|a, b| a.path == b.path);
        bases.sort_by(|a, b| a.path.cmp(&b.path));
        bases.dedup_by(|a, b| a.path == b.path);
        if full && items.is_empty() && self.db.file_count() > 0 {
            bail!("the sync folder is empty although it had {} files: paused (unplugged drive or failed mount?)", self.db.file_count());
        }

        let present: HashMap<&str, &LocalItem> = items.iter().map(|i| (i.path.as_str(), i)).collect();
        let mut clash_deletes: Vec<Change> = vec![];
        let mut mkdirs: Vec<Change> = vec![];
        let mut puts: Vec<Change> = vec![];
        let mut deletes: Vec<(Base, Change)> = vec![];

        for it in &items {
            let p = it.path.as_str();
            if self.db.conflict(p).is_some() || self.db.is_local_only(p) {
                continue;
            }
            let b = self.db.base(p);
            match it.kind {
                EntryKind::Dir => match &b {
                    None => mkdirs.push(Change::Mkdir { path: p.to_string() }),
                    Some(b) if b.kind == EntryKind::File => {
                        clash_deletes.push(Change::Delete { path: p.to_string(), base_rev: b.rev });
                        mkdirs.push(Change::Mkdir { path: p.to_string() });
                    }
                    Some(_) => {}
                },
                EntryKind::File => {
                    if now - it.mtime < self.cfg.settle_ms {
                        rep.retry = true; // still being written
                        continue;
                    }
                    let hash = match self.file_hash(p, it.size, it.mtime) {
                        Ok(h) => h,
                        Err(_) => continue, // vanished while scanning
                    };
                    if self.rejected.contains(&(p.to_string(), hash.clone())) {
                        continue;
                    }
                    match &b {
                        Some(b) if b.kind == EntryKind::File && b.hash == hash && b.exec == it.exec => {}
                        Some(b) if b.kind == EntryKind::File => {
                            puts.push(Change::Put { path: p.to_string(), hash, size: it.size, mtime: it.mtime, exec: it.exec, base_rev: b.rev });
                        }
                        Some(b) => {
                            clash_deletes.push(Change::Delete { path: p.to_string(), base_rev: b.rev });
                            puts.push(Change::Put { path: p.to_string(), hash, size: it.size, mtime: it.mtime, exec: it.exec, base_rev: 0 });
                        }
                        None => {
                            puts.push(Change::Put { path: p.to_string(), hash, size: it.size, mtime: it.mtime, exec: it.exec, base_rev: 0 });
                        }
                    }
                }
            }
        }
        for b in bases.iter().rev() {
            if present.contains_key(b.path.as_str()) || self.db.conflict(&b.path).is_some() {
                continue;
            }
            deletes.push((b.clone(), Change::Delete { path: b.path.clone(), base_rev: b.rev }));
        }

        // a deleted file and a new file with the same content are a move (history is kept, content
        // is not sent again)
        let mut moves: Vec<Change> = vec![];
        let mut used_puts: HashSet<usize> = HashSet::new();
        let mut moved_deletes: HashSet<usize> = HashSet::new();
        for (di, (b, _)) in deletes.iter().enumerate() {
            if b.kind != EntryKind::File || b.rev == 0 || b.hash.is_empty() {
                continue;
            }
            let found = puts.iter().enumerate().find(|(pi, c)| {
                !used_puts.contains(pi)
                    && matches!(c, Change::Put { hash, size, base_rev: 0, .. } if *hash == b.hash && *size == b.size)
            });
            if let Some((pi, Change::Put { path, .. })) = found {
                moves.push(Change::Move { from: b.path.clone(), to: path.clone(), base_rev: b.rev });
                used_puts.insert(pi);
                moved_deletes.insert(di);
            }
        }
        let puts: Vec<Change> = puts.into_iter().enumerate().filter(|(i, _)| !used_puts.contains(i)).map(|(_, c)| c).collect();
        let mut del_changes: Vec<Change> =
            deletes.into_iter().enumerate().filter(|(i, _)| !moved_deletes.contains(i)).map(|(_, (_, c))| c).collect();

        // mass delete guard
        let total = self.db.bases_under("").len();
        let n_del = del_changes.len() + clash_deletes.len();
        let held = n_del > MASS_DELETE_MAX || (n_del >= MASS_DELETE_MIN && n_del * 2 > total);
        if held {
            if self.db.meta("confirm_deletes").as_deref() == Some("1") {
                self.db.set_meta("confirm_deletes", "0")?;
            } else {
                rep.held_deletes = Some(n_del);
                del_changes.clear();
                clash_deletes.clear();
            }
        }

        let mut changes: Vec<Change> = vec![];
        changes.extend(clash_deletes);
        changes.extend(mkdirs);
        changes.extend(moves);
        changes.extend(puts);
        changes.extend(del_changes);
        if changes.is_empty() {
            if rep.held_deletes.is_none() && !rep.retry {
                self.dirty = Some(HashSet::new());
            }
            return Ok(());
        }

        let verdict = self.tr.propose(&changes)?;
        let mut sources: HashMap<&str, (&str, u64)> = HashMap::new();
        for c in &changes {
            if let Change::Put { path, hash, size, .. } = c {
                sources.insert(hash.as_str(), (path.as_str(), *size));
            }
        }
        for h in &verdict.need {
            let Some((p, size)) = sources.get(h.as_str()) else { continue };
            match self.tr.upload(h, &self.fe.local_path(p), *size)? {
                Upload::Done => rep.uploaded += 1,
                Upload::HashMismatch => {
                    rep.retry = true;
                    rep.errors.push(format!("{p}: changed while it was being uploaded"));
                }
            }
        }
        let results = self.tr.commit(&changes)?;
        if results.len() != changes.len() {
            bail!("the host answered {} results for {} changes", results.len(), changes.len());
        }
        let mut redo = false;
        for (ch, res) in changes.iter().zip(results) {
            redo |= self.handle_result(ch, res, rep)?;
        }
        if redo {
            self.dirty = None;
        } else if !rep.retry {
            self.dirty = Some(HashSet::new());
        }
        Ok(())
    }

    /// Returns true if the next round should look at everything again.
    fn handle_result(&mut self, ch: &Change, res: ChangeResult, rep: &mut Report) -> Result<bool> {
        match res {
            ChangeResult::Ok { rev } => {
                rep.pushed += 1;
                match ch {
                    Change::Put { path, hash, size, mtime, exec, .. } => {
                        self.db.set_base(&Base { path: path.clone(), kind: EntryKind::File, rev, hash: hash.clone(), size: *size, mtime: *mtime, exec: *exec })?;
                        self.db.set_local_cache(path, *size, *mtime, hash)?;
                        self.fe.set_status(path, EntryStatus::InSync);
                    }
                    Change::Mkdir { path } => {
                        self.db.set_base(&Base { path: path.clone(), kind: EntryKind::Dir, rev, hash: String::new(), size: 0, mtime: 0, exec: false })?;
                    }
                    Change::Delete { path, .. } => {
                        self.db.drop_base(path)?;
                    }
                    Change::Move { from, to, .. } => {
                        let moved = self.db.base(from);
                        self.db.drop_base(from)?;
                        if let (Some(b), Some(l)) = (moved, self.local_state(to)?) {
                            self.db.set_base(&Base { path: to.clone(), rev, mtime: l.mtime, exec: l.exec, ..b })?;
                            self.db.set_local_cache(to, l.size, l.mtime, &l.hash)?;
                        }
                    }
                }
                Ok(false)
            }
            ChangeResult::Conflict { reason, current } => {
                let path = ch.path().to_string();
                match (reason, current) {
                    (ConflictReason::NotEmpty, _) => {
                        // folder deleted here, files created in it there: the folder survives
                        self.fe.make_dir(&path)?;
                        Ok(true)
                    }
                    (ConflictReason::CaseCollision, Some(other)) => {
                        // the second of two names that differ only in case is kept under a conflict name
                        let copy = self.unique_conflict_name(&path)?;
                        self.fe.rename(&path, &copy)?;
                        self.db.drop_base(&path)?;
                        rep.conflicts += 1;
                        rep.errors.push(format!("{path}: differs from {} only in case, kept as {copy}", other.path));
                        Ok(true)
                    }
                    (_, Some(cur)) => {
                        self.apply_remote(&cur, rep)?;
                        Ok(true)
                    }
                    (_, None) => {
                        // the host has no record of what we believed it had
                        self.db.drop_base(&path)?;
                        if let Change::Move { from, .. } = ch {
                            self.db.drop_base(from)?;
                        }
                        Ok(true)
                    }
                }
            }
            ChangeResult::Invalid { message } => {
                if let Change::Put { path, hash, .. } = ch {
                    self.rejected.insert((path.clone(), hash.clone()));
                }
                rep.errors.push(format!("{}: rejected by the host: {message}", ch.path()));
                Ok(false)
            }
            ChangeResult::MissingContent => {
                rep.retry = true;
                Ok(true)
            }
            ChangeResult::Denied => {
                self.cfg.access = Access::ReadOnly;
                rep.errors.push("the host no longer allows changes from this device".into());
                Ok(false)
            }
        }
    }

    /// Read-only members: local edits are reverted from the host; the edited file is kept next to it.
    fn revert_local(&mut self, rep: &mut Report) -> Result<()> {
        for b in self.db.bases_under("") {
            if b.kind != EntryKind::File || self.db.conflict(&b.path).is_some() {
                continue;
            }
            let local = self.local_state(&b.path)?;
            let differs = match &local {
                None => true,
                Some(l) => l.kind != EntryKind::File || l.hash != b.hash,
            };
            if !differs {
                continue;
            }
            let e = Entry {
                path: b.path.clone(),
                kind: b.kind,
                size: b.size,
                mtime: b.mtime,
                hash: b.hash.clone(),
                exec: b.exec,
                rev: b.rev,
                deleted: false,
                author: String::new(),
            };
            let r = if local.is_some() { self.keep_both(&e, rep).map(|_| ()) } else { self.install_file(&e, rep) };
            if let Err(err) = r {
                rep.errors.push(format!("{}: {err:#}", b.path));
                rep.retry = true;
            } else {
                // keep_both dropped the base: put it back for the reinstalled file
                self.db.set_base(&b)?;
            }
        }
        Ok(())
    }

    // ---------------- running ----------------

    /// Runs rounds until `Wake::Stop` (or the channel closes): a round at start, then on every wake-up,
    /// and at least every `poll`.
    pub fn run(&mut self, wake: Receiver<Wake>, stop: &AtomicBool, poll: Duration, debounce: Duration, on_report: &dyn Fn(&Result<Report>)) {
        let mut backoff = Duration::from_secs(2);
        let mut next_wait;
        loop {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            let r = self.round();
            match &r {
                Ok(rep) => {
                    backoff = Duration::from_secs(2);
                    next_wait = if rep.retry { Duration::from_secs(2) } else { poll };
                }
                Err(_) => {
                    next_wait = backoff;
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                }
            }
            on_report(&r);
            match wake.recv_timeout(next_wait) {
                Ok(Wake::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                Ok(w) => {
                    self.take_wake(w);
                    // let a burst settle
                    while let Ok(w) = wake.recv_timeout(debounce) {
                        if matches!(w, Wake::Stop) {
                            return;
                        }
                        self.take_wake(w);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    // a safety look at everything now and then
                    self.dirty = None;
                }
            }
        }
    }

    fn take_wake(&mut self, w: Wake) {
        match w {
            Wake::Local(paths) => self.mark_dirty(&paths),
            Wake::Verify => {
                let _ = self.verify();
            }
            Wake::Command(f) => f(self),
            Wake::Remote | Wake::Stop => {}
        }
    }
}
