//! How a sync appears on a member. The engine is platform-free; a frontend answers "what is there",
//! "put this here" and "remove that". The plain mirror is a first-class frontend (CLI, tests, Linux,
//! fallback); Cloud Files and the macOS mount implement the same trait later.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use medialake_proto::sync::{EntryKind, is_ignored, normalize_path};

use crate::util::*;
use crate::watch::PathWatcher;

#[derive(Debug, Clone, PartialEq)]
pub struct LocalItem {
    pub path: String,
    pub kind: EntryKind,
    pub size: u64,
    pub mtime: i64,
    pub exec: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryStatus {
    InSync,
    Syncing,
    Conflict,
    Error,
}

pub trait Frontend: Send + Sync {
    /// Fails if the sync folder (or mount) is missing: the engine then pauses instead of pushing deletes.
    fn check_root(&self) -> Result<()>;
    /// Items at or below `prefix` (empty = everything), directories included, ignored names excluded.
    fn scan(&self, prefix: &str, ignore: &[String]) -> Result<Vec<LocalItem>>;
    /// Where the content of `rel` can be read (hydrating it first on on-demand frontends).
    fn local_path(&self, rel: &str) -> PathBuf;
    /// Where the metadata of `rel` (size, mtime, existence) can be read, without hydrating anything.
    /// On-demand frontends keep a full-size sparse file there.
    fn meta_path(&self, rel: &str) -> PathBuf {
        self.local_path(rel)
    }
    /// True for frontends that show files without their content and fetch it when it is read.
    fn lazy(&self) -> bool {
        false
    }
    /// The content hash of `rel` if the frontend knows it without reading the file: its content is
    /// untouched since [`Frontend::note_content`] / `place_placeholder` recorded it.
    fn content_hash(&self, _rel: &str) -> Option<String> {
        None
    }
    /// `rel` now holds content `hash` (host revision `rev`): after an install, a successful push, or
    /// when both sides turned out equal. `size` / `mtime` are what the engine saw; a frontend that
    /// tracks local writes must only treat the file as clean if it still has them.
    fn note_content(&self, _rel: &str, _hash: &str, _rev: u64, _size: u64, _mtime: i64) {}
    /// On-demand frontends: records the host's file at `rel` without its content (a placeholder with
    /// `size` and `mtime`). Returns false if the content is wanted now (pinned): the engine then
    /// downloads it and calls `place_file`.
    fn place_placeholder(&self, _rel: &str, _size: u64, _mtime: i64, _exec: bool, _hash: &str, _rev: u64) -> Result<bool> {
        Ok(false)
    }
    /// Puts a downloaded file at `rel` atomically and applies mtime and the executable bit.
    fn place_file(&self, rel: &str, staged: &Path, mtime: i64, exec: bool) -> Result<()>;
    fn make_dir(&self, rel: &str) -> Result<()>;
    /// Removes a file, or a folder if it is empty. Returns false if a folder was not empty.
    fn remove(&self, rel: &str, is_dir: bool) -> Result<bool>;
    fn rename(&self, from: &str, to: &str) -> Result<()>;
    fn set_status(&self, _rel: &str, _status: EntryStatus) {}
    /// Calls `on_batch` with debounced dirty paths while the returned handle lives.
    fn watch(&self, _debounce: Duration, _on_batch: Box<dyn Fn(Vec<String>) + Send>) -> Option<Box<dyn std::any::Any + Send>> {
        None
    }
}

/// A normal folder with full copies.
pub struct Mirror {
    root: PathBuf,
}

impl Mirror {
    pub fn new(root: &Path) -> Mirror {
        Mirror { root: root.to_path_buf() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn abs(&self, rel: &str) -> PathBuf {
        if rel.is_empty() { self.root.clone() } else { self.root.join(rel) }
    }

    fn walk(&self, rel: &str, out: &mut Vec<LocalItem>, ignore: &[String]) {
        let abs = self.abs(rel);
        let Ok(m) = std::fs::symlink_metadata(&abs) else { return };
        if m.file_type().is_symlink() {
            return; // skipped and never followed
        }
        if !rel.is_empty() {
            if is_ignored(rel, ignore) {
                return;
            }
            out.push(LocalItem {
                path: rel.to_string(),
                kind: if m.is_dir() { EntryKind::Dir } else { EntryKind::File },
                size: if m.is_dir() { 0 } else { m.len() },
                mtime: mtime_ms(&m),
                exec: !m.is_dir() && is_exec(&m),
            });
        }
        if !m.is_dir() {
            return;
        }
        let Ok(rd) = std::fs::read_dir(&abs) else { return };
        let mut names: Vec<String> = rd.flatten().filter_map(|e| e.file_name().to_str().map(str::to_string)).collect();
        names.sort();
        for mut name in names {
            // macOS APIs may hand out decomposed names: the sync uses NFC, so rename on disk
            if let Ok(nfc) = normalize_path(&name) {
                if nfc != name && std::fs::rename(abs.join(&name), abs.join(&nfc)).is_ok() {
                    name = nfc;
                }
            }
            let child = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            self.walk(&child, out, ignore);
        }
    }
}

impl Frontend for Mirror {
    fn check_root(&self) -> Result<()> {
        if !self.root.is_dir() {
            bail!("sync folder {} is missing (unplugged drive?)", self.root.display());
        }
        Ok(())
    }

    fn scan(&self, prefix: &str, ignore: &[String]) -> Result<Vec<LocalItem>> {
        self.check_root()?;
        let mut out = vec![];
        self.walk(prefix, &mut out, ignore);
        Ok(out)
    }

    fn local_path(&self, rel: &str) -> PathBuf {
        self.abs(rel)
    }

    fn place_file(&self, rel: &str, staged: &Path, mtime: i64, exec: bool) -> Result<()> {
        let dest = self.abs(rel);
        if let Some(p) = dest.parent() {
            std::fs::create_dir_all(p)?;
        }
        if dest.is_dir() {
            bail!("a folder is in the way at {}", dest.display());
        }
        let tmp = dest.with_file_name(format!(".{}.medialake.part", random_id()));
        if std::fs::rename(staged, &tmp).is_err() {
            // staging area on another volume
            std::fs::copy(staged, &tmp).with_context(|| format!("copying into {}", tmp.display()))?;
            let _ = std::fs::remove_file(staged);
        }
        if mtime > 0 {
            set_mtime_ms(&tmp, mtime)?;
        }
        set_exec(&tmp, exec)?;
        std::fs::rename(&tmp, &dest).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
        Ok(())
    }

    fn make_dir(&self, rel: &str) -> Result<()> {
        std::fs::create_dir_all(self.abs(rel))?;
        Ok(())
    }

    fn remove(&self, rel: &str, is_dir: bool) -> Result<bool> {
        let abs = self.abs(rel);
        let r = if is_dir { std::fs::remove_dir(&abs) } else { std::fs::remove_file(&abs) };
        match r {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
            Err(_) if is_dir => {
                // only ignorable junk left (.DS_Store …) counts as empty
                let only_junk = std::fs::read_dir(&abs)
                    .map(|rd| rd.flatten().all(|e| e.file_name().to_str().is_some_and(|n| is_ignored(n, &[]))))
                    .unwrap_or(false);
                if only_junk {
                    std::fs::remove_dir_all(&abs)?;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Err(e) => Err(e.into()),
        }
    }

    fn rename(&self, from: &str, to: &str) -> Result<()> {
        let dest = self.abs(to);
        if let Some(p) = dest.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::rename(self.abs(from), dest)?;
        Ok(())
    }

    fn watch(&self, debounce: Duration, on_batch: Box<dyn Fn(Vec<String>) + Send>) -> Option<Box<dyn std::any::Any + Send>> {
        PathWatcher::start(&self.root, debounce, |rel| is_ignored(rel, &[]), on_batch).ok().map(|w| Box::new(w) as Box<dyn std::any::Any + Send>)
    }
}
