//! The mount as a sync frontend: the engine sees the cache tree (a full-size sparse file per entry) and
//! asks this type for the few things a plain folder cannot do.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use medialake_sync::member::frontend::{EntryStatus, Frontend, LocalItem, Mirror};

use crate::store::Store;

pub struct MountFrontend {
    store: Arc<Store>,
    tree: Mirror,
}

impl MountFrontend {
    pub fn new(store: Arc<Store>) -> MountFrontend {
        let tree = Mirror::new(store.tree());
        MountFrontend { store, tree }
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }
}

impl Frontend for MountFrontend {
    fn check_root(&self) -> Result<()> {
        self.tree.check_root()
    }

    fn scan(&self, prefix: &str, ignore: &[String]) -> Result<Vec<LocalItem>> {
        self.tree.scan(prefix, ignore)
    }

    /// Hydrates the whole file first: callers read its content (hashing, uploading).
    fn local_path(&self, rel: &str) -> PathBuf {
        match self.store.ensure_full(rel) {
            Ok(()) => self.store.abs(rel),
            Err(e) => {
                // a path that does not exist makes the caller fail instead of hashing zeros
                tracing::warn!("{rel}: {e:#}");
                self.store.tree().join(".medialake-unavailable").join(rel)
            }
        }
    }

    fn meta_path(&self, rel: &str) -> PathBuf {
        self.store.abs(rel)
    }

    fn lazy(&self) -> bool {
        true
    }

    fn place_placeholder(&self, rel: &str, size: u64, mtime: i64, exec: bool, hash: &str, rev: u64) -> Result<bool> {
        self.store.place_placeholder(rel, size, mtime, exec, hash, rev)
    }

    fn content_hash(&self, rel: &str) -> Option<String> {
        self.store.content_hash(rel)
    }

    fn note_content(&self, rel: &str, hash: &str, rev: u64, size: u64, mtime: i64) {
        self.store.note_content(rel, hash, rev, size, mtime)
    }

    fn place_file(&self, rel: &str, staged: &Path, mtime: i64, exec: bool) -> Result<()> {
        self.store.place_file(rel, staged, mtime, exec)
    }

    fn make_dir(&self, rel: &str) -> Result<()> {
        self.tree.make_dir(rel)
    }

    fn remove(&self, rel: &str, is_dir: bool) -> Result<bool> {
        let gone = self.tree.remove(rel, is_dir)?;
        if gone {
            let _ = self.store.index().remove_tree(rel);
        }
        Ok(gone)
    }

    fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.tree.rename(from, to)?;
        let _ = self.store.index().rename_tree(from, to);
        Ok(())
    }

    fn set_status(&self, _rel: &str, _status: EntryStatus) {}

    fn watch(&self, debounce: Duration, on_batch: Box<dyn Fn(Vec<String>) + Send>) -> Option<Box<dyn std::any::Any + Send>> {
        Some(Box::new(self.store.set_sink(debounce, on_batch)))
    }
}
