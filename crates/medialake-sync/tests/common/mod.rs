#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use medialake_proto::sync::{Access, is_ignored};
use medialake_sync::host::config::{BackupPolicy, HostedConfig};
use medialake_sync::host::hosted::HostedSync;
use medialake_sync::member::engine::{ConflictPolicy, Engine, EngineConfig, Report};
use medialake_sync::member::frontend::Mirror;
use medialake_sync::member::transport::LocalTransport;
use tempfile::TempDir;

pub struct Host {
    pub dir: TempDir,
    pub root: PathBuf,
    pub hs: Arc<HostedSync>,
}

pub fn host(backup: BackupPolicy) -> Host {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("host");
    std::fs::create_dir_all(&root).unwrap();
    let cfg = HostedConfig {
        id: medialake_sync::util::random_id(),
        name: "Test".into(),
        root: root.clone(),
        store: None,
        backup,
        ignore: vec![],
    };
    let hs = HostedSync::open(cfg, None).unwrap();
    Host { dir, root, hs }
}

pub struct Member {
    pub _dir: TempDir,
    pub root: PathBuf,
    pub engine: Engine,
    pub name: String,
}

pub fn member(h: &Host, name: &str, policy: ConflictPolicy, access: Access) -> Member {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = base.join("files");
    std::fs::create_dir_all(&root).unwrap();
    let cfg = EngineConfig {
        device: name.to_string(),
        policy,
        access,
        state_dir: base.join("state"),
        settle_ms: 0,
        ignore: vec![],
    };
    let tr = LocalTransport { host: h.hs.clone(), access, author: name.to_string() };
    let engine = Engine::new(cfg, Arc::new(Mirror::new(&root)), Arc::new(tr)).unwrap();
    Member { _dir: dir, root, engine, name: name.to_string() }
}

impl Member {
    /// Like the run loop: a round that had to retry is run again.
    pub fn sync(&mut self) -> Report {
        let mut total = Report::default();
        for _ in 0..6 {
            let r = self.engine.sync_once().unwrap();
            let retry = r.retry;
            total.pulled += r.pulled;
            total.downloaded += r.downloaded;
            total.pushed += r.pushed;
            total.uploaded += r.uploaded;
            total.removed_local += r.removed_local;
            total.conflicts += r.conflicts;
            total.held_deletes = r.held_deletes.or(total.held_deletes);
            total.errors.extend(r.errors);
            if !retry {
                total.retry = false;
                break;
            }
            total.retry = true;
        }
        total
    }

    pub fn write(&self, rel: &str, content: &str) {
        write(&self.root, rel, content);
    }

    pub fn read(&self, rel: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join(rel)).ok()
    }

    pub fn exists(&self, rel: &str) -> bool {
        self.root.join(rel).exists()
    }

    pub fn remove(&self, rel: &str) {
        let p = self.root.join(rel);
        if p.is_dir() {
            std::fs::remove_dir_all(p).unwrap();
        } else {
            std::fs::remove_file(p).unwrap();
        }
    }

    pub fn tree(&self) -> BTreeMap<String, String> {
        tree(&self.root)
    }
}

pub fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, content).unwrap();
    // distinct mtimes even for rapid rewrites with equal length
    static N: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
    let n = N.fetch_add(7, std::sync::atomic::Ordering::SeqCst);
    medialake_sync::util::set_mtime_ms(&p, medialake_sync::util::now_ms() - 600_000 + n + 1).unwrap();
}

/// path -> content for files, path -> "<dir>" for folders; sync metadata and junk excluded.
pub fn tree(root: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for e in walkdir::WalkDir::new(root).min_depth(1) {
        let e = e.unwrap();
        let rel = e.path().strip_prefix(root).unwrap().to_str().unwrap().replace('\\', "/");
        if is_ignored(&rel, &[]) {
            continue;
        }
        if e.file_type().is_dir() {
            out.insert(rel, "<dir>".to_string());
        } else {
            out.insert(rel, std::fs::read_to_string(e.path()).unwrap());
        }
    }
    out
}

/// Runs members (and a host scan) until a full pass changes nothing.
pub fn settle(h: &Host, members: &mut [&mut Member]) {
    for _ in 0..12 {
        let mut activity = h.hs.scan().unwrap();
        for m in members.iter_mut() {
            let r = m.sync();
            activity += r.downloaded + r.pushed + r.uploaded + r.removed_local + r.conflicts;
        }
        if activity == 0 {
            return;
        }
    }
    panic!("did not settle");
}
