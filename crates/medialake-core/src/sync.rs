//! `SyncManager`: the joined syncs (one engine per sync, each on its own thread), the hosts this device
//! is paired with, and — optionally — this device hosting folders for others. Configuration lives in
//! `config.toml` (§9); engine state in `<data dir>/sync/<sync id>/`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use bierpc::rpc::{RpcServer, ServerConfig};
use medialake_proto::MAX_MESSAGE_BYTES;
use medialake_proto::pairing::{hex, unhex};
use medialake_proto::pinning;
use medialake_proto::sync::{Access, ChangeResult, RestoreWhat, is_ignored, normalize_path};
use medialake_sync::host::config::{BackupPolicy, HostedConfig, SyncsFile};
use medialake_sync::host::state::{Grants, StateDir};
use medialake_sync::host::{SyncHost, SyncOnlyService};
use medialake_sync::member::db::MemberDb;
use medialake_sync::member::engine::{Choice, ConflictPolicy, Report};
use medialake_sync::member::frontend::{Frontend, Mirror};
use medialake_sync::member::transport::{HostClient, HostCreds, Transport, pair_ex};
use medialake_sync::member::{MemberConfig, MemberStore, RunningSync};
#[cfg(target_os = "macos")]
use medialake_mount::{MountConfig, MountSession, Store as MountStore, mountctl};
use serde::Serialize;
use tokio::runtime::Handle;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::config::{Config, SyncFolderCfg, SyncHostCfg};
use crate::events::Event;
use crate::remote;

#[derive(Serialize, Clone, Debug, Default)]
pub struct SyncStatus {
    pub sync_id: String,
    pub name: String,
    pub path: String,
    pub host_id: String,
    pub host_name: String,
    pub access: String,
    pub conflict_policy: String,
    /// The frontend actually in use: `mount` (files appear at full size and load when opened) or
    /// `mirror` (a plain folder with full copies).
    pub frontend: String,
    /// Something the user should know that is not an error (the mount fell back to a folder …).
    pub notice: String,
    /// Bytes of file content held on this device by a mount (0 for a mirror), and its limit.
    pub cached_bytes: u64,
    pub cache_limit_bytes: u64,
    /// Files changed here that the host has not received yet.
    pub unsynced_files: u64,
    pub paused: bool,
    /// `starting`, `idle`, `syncing`, `paused`, `conflict`, `held`, or an error state: `offline` (host
    /// unreachable), `denied` (access revoked), `disk_full`, `error` (anything else).
    pub state: String,
    pub detail: String,
    /// Unix seconds of the last finished round.
    pub last_sync: Option<i64>,
    pub downloaded: u64,
    pub uploaded: u64,
    pub pushed: u64,
    pub pending_conflicts: usize,
    pub held_deletes: Option<usize>,
}

#[derive(Serialize, Clone, Debug)]
pub struct SyncPairResult {
    pub host_id: String,
    pub name: String,
    pub address: String,
    /// The same code also granted embedding (the caller may fill the embedding server credentials).
    pub embedding: bool,
    pub quality: String,
    pub backend: String,
}

#[derive(Serialize, Clone, Debug)]
pub struct HostSummary {
    pub id: String,
    pub name: String,
    pub address: String,
}

#[derive(Serialize, Clone, Debug)]
pub struct AvailableSync {
    pub id: String,
    pub name: String,
    pub access: String,
    pub files: u64,
    pub bytes: u64,
    pub backup: String,
    pub joined: bool,
}

#[derive(Serialize, Clone, Debug)]
pub struct ConflictItem {
    pub sync_id: String,
    pub name: String,
    pub path: String,
    pub host_deleted: bool,
    pub since: i64,
}

#[derive(Serialize, Clone, Debug)]
pub struct BrowseItem {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64,
    /// `local` for a mirror (every file is a full copy). For a mount: `cloud`, `partial`, `cached`,
    /// `dirty`, `local` or `dir`.
    pub state: String,
    pub pinned: bool,
    pub cached_bytes: u64,
}

#[derive(Serialize, Clone, Debug)]
pub struct HostedClientInfo {
    pub id: String,
    pub name: String,
    pub created: u64,
    pub embed: bool,
    /// sync name -> `rw` / `ro`
    pub syncs: BTreeMap<String, String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct HostingStatus {
    pub running: bool,
    pub bind: String,
    pub public_addr: String,
    pub error: String,
    pub syncs: Vec<HostedSyncInfo>,
}

#[derive(Serialize, Clone, Debug)]
pub struct HostedSyncInfo {
    pub id: String,
    pub name: String,
    pub root: String,
    pub backup: String,
    pub files: u64,
    pub bytes: u64,
    pub seq: u64,
    pub issues: Vec<String>,
}

struct Hosting {
    host: Arc<SyncHost>,
    state: Arc<StateDir>,
    server: JoinHandle<()>,
    background: JoinHandle<()>,
    addr: std::net::SocketAddr,
}

pub struct SyncManager {
    cfg: Arc<RwLock<Config>>,
    config_file: PathBuf,
    root: PathBuf,
    events: broadcast::Sender<Event>,
    handle: Handle,
    store: MemberStore,
    clients: Mutex<HashMap<String, Arc<HostClient>>>,
    running: Mutex<HashMap<String, RunningSync>>,
    status: Arc<Mutex<HashMap<String, SyncStatus>>>,
    hosting: tokio::sync::Mutex<Option<Hosting>>,
    hosting_error: Mutex<String>,
    #[cfg(target_os = "macos")]
    mounts: tokio::sync::Mutex<HashMap<String, MountSession>>,
    #[cfg(target_os = "macos")]
    stores: Mutex<HashMap<String, Arc<MountStore>>>,
    #[cfg(target_os = "macos")]
    registry: mountctl::Registry,
}

/// Maps an engine / transport error message to the state the UI shows (§12 X2: host unreachable,
/// disk full, revoked access). The engine retries with backoff in every one of these states.
impl crate::indexer::ContentGate for SyncManager {
    fn would_download(&self, path: &Path) -> bool {
        SyncManager::would_download(self, path)
    }
}

pub fn classify_error(msg: &str) -> &'static str {
    let m = msg.to_lowercase();
    if m.contains("no space left") || m.contains("not enough space") || m.contains("disk full") || m.contains("quota exceeded") {
        "disk_full"
    } else if m.contains("access denied") || m.contains("authentication failed") || m.contains("no access to sync") || m.contains("no longer allows") {
        "denied"
    } else if m.contains("cannot connect")
        || m.contains("cannot resolve")
        || m.contains("connection refused")
        || m.contains("connection reset")
        || m.contains("timed out")
        || m.contains("is this host a medialake host")
        || m.contains("broken pipe")
    {
        "offline"
    } else {
        "error"
    }
}

fn to_member(f: &SyncFolderCfg) -> MemberConfig {
    MemberConfig {
        sync_id: f.sync_id.clone(),
        host_id: f.host_id.clone(),
        name: f.name.clone(),
        path: f.path.clone(),
        conflict: f.conflict.clone(),
        access: f.access.clone(),
        paused: f.paused,
    }
}

impl SyncManager {
    pub fn new(cfg: Arc<RwLock<Config>>, config_file: PathBuf, root: PathBuf, events: broadcast::Sender<Event>) -> anyhow::Result<Arc<SyncManager>> {
        let store = MemberStore::open(&root.join("sync"))?;
        Ok(Arc::new(SyncManager {
            #[cfg(target_os = "macos")]
            registry: mountctl::Registry::new(root.join("sync").join("mounts.json")),
            cfg,
            config_file,
            root,
            events,
            handle: Handle::current(),
            store,
            clients: Mutex::new(HashMap::new()),
            running: Mutex::new(HashMap::new()),
            status: Arc::new(Mutex::new(HashMap::new())),
            hosting: tokio::sync::Mutex::new(None),
            hosting_error: Mutex::new(String::new()),
            #[cfg(target_os = "macos")]
            mounts: tokio::sync::Mutex::new(HashMap::new()),
            #[cfg(target_os = "macos")]
            stores: Mutex::new(HashMap::new()),
        }))
    }

    // ---------------- config plumbing ----------------

    fn device(&self) -> String {
        self.cfg.read().unwrap().sync.device()
    }

    fn update<R>(&self, f: impl FnOnce(&mut Config) -> R) -> Result<R, String> {
        let (r, snapshot) = {
            let mut c = self.cfg.write().unwrap();
            let r = f(&mut c);
            (r, c.clone())
        };
        snapshot.save(&self.config_file).map_err(|e| e.to_string())?;
        Ok(r)
    }

    fn folder(&self, sync_id: &str) -> Result<SyncFolderCfg, String> {
        let c = self.cfg.read().unwrap();
        c.sync
            .folders
            .iter()
            .find(|f| f.sync_id == sync_id || f.name == sync_id)
            .cloned()
            .ok_or_else(|| format!("no joined sync {sync_id:?}"))
    }

    fn client(&self, host_id: &str) -> Result<Arc<HostClient>, String> {
        if let Some(c) = self.clients.lock().unwrap().get(host_id) {
            return Ok(c.clone());
        }
        let h: SyncHostCfg = self
            .cfg
            .read()
            .unwrap()
            .sync
            .hosts
            .iter()
            .find(|h| h.id == host_id)
            .cloned()
            .ok_or_else(|| format!("host {host_id} is not paired"))?;
        let secret = remote::unprotect(&h.secret_protected)?;
        let creds = HostCreds {
            id: h.id.clone(),
            name: h.name.clone(),
            address: h.address.clone(),
            fingerprint: h.fingerprint.clone(),
            client_id: h.client_id.clone(),
            secret_hex: hex(&secret),
        };
        let c = HostClient::new(self.handle.clone(), creds, &self.device());
        {
            let cfg = self.cfg.read().unwrap();
            c.set_limits(cfg.sync.upload_limit_kbps, cfg.sync.download_limit_kbps);
        }
        self.clients.lock().unwrap().insert(host_id.to_string(), c.clone());
        Ok(c)
    }

    /// Applies changed bandwidth limits to the connections that exist.
    pub fn apply_limits(&self) {
        let (up, down) = {
            let c = self.cfg.read().unwrap();
            (c.sync.upload_limit_kbps, c.sync.download_limit_kbps)
        };
        for c in self.clients.lock().unwrap().values() {
            c.set_limits(up, down);
        }
    }

    /// Forgets cached connections (the device name or a credential changed).
    pub fn reset_clients(&self) {
        self.clients.lock().unwrap().clear();
    }

    fn emit(&self, e: Event) {
        let _ = self.events.send(e);
    }

    // ---------------- lifecycle ----------------

    /// Starts every joined sync that is not paused, and hosting if folders are shared.
    pub async fn start(self: &Arc<Self>) {
        // a crash or force-quit leaves mounts whose server is gone: remove them before mounting again
        #[cfg(target_os = "macos")]
        {
            let reg = mountctl::Registry::new(self.root.join("sync").join("mounts.json"));
            let gone = tokio::task::spawn_blocking(move || reg.cleanup_dead()).await.unwrap_or_default();
            for p in gone {
                tracing::info!("removed a stale mount at {}", p.display());
            }
        }
        let folders = self.cfg.read().unwrap().sync.folders.clone();
        for f in folders {
            self.init_status(&f);
            // a paused sync keeps its volume (files stay reachable); only the engine is off
            if f.paused {
                self.ensure_volume_only(&f).await;
            } else if let Err(e) = self.start_folder(&f).await {
                self.fail(&f, e);
            }
        }
        if let Err(e) = self.ensure_hosting().await {
            *self.hosting_error.lock().unwrap() = e.clone();
            self.emit(Event::SyncError { sync_id: String::new(), message: e });
        }
    }

    /// Stops everything (engines finish their round first).
    pub async fn stop(&self) {
        let runs: Vec<RunningSync> = self.running.lock().unwrap().drain().map(|(_, r)| r).collect();
        let _ = tokio::task::spawn_blocking(move || drop(runs)).await;
        #[cfg(target_os = "macos")]
        {
            let ids: Vec<String> = self.mounts.lock().await.keys().cloned().collect();
            for id in ids {
                self.teardown_mount(&id).await;
            }
        }
        if let Some(h) = self.hosting.lock().await.take() {
            h.server.abort();
            h.background.abort();
        }
    }

    fn init_status(&self, f: &SyncFolderCfg) {
        let host_name = self.cfg.read().unwrap().sync.hosts.iter().find(|h| h.id == f.host_id).map(|h| h.name.clone()).unwrap_or_default();
        // opening a missing database would create it, and "has a state database" means "has run before"
        let db = self.store.state_dir(&f.sync_id).join("state.db");
        let pending = if db.exists() { MemberDb::open(&db).map(|d| d.conflicts().len()).unwrap_or(0) } else { 0 };
        let mut m = self.status.lock().unwrap();
        let s = m.entry(f.sync_id.clone()).or_default();
        s.sync_id = f.sync_id.clone();
        s.name = f.name.clone();
        s.path = f.path.display().to_string();
        s.host_id = f.host_id.clone();
        s.host_name = host_name;
        s.access = f.access.clone();
        s.conflict_policy = f.conflict.clone();
        if s.frontend.is_empty() {
            s.frontend = self.resolved_frontend(f).into();
        }
        s.paused = f.paused;
        s.pending_conflicts = pending;
        s.state = if f.paused { "paused".into() } else { "starting".into() };
    }

    fn fail(&self, f: &SyncFolderCfg, msg: String) {
        {
            let mut m = self.status.lock().unwrap();
            if let Some(s) = m.get_mut(&f.sync_id) {
                s.state = "error".into();
                s.detail = msg.clone();
            }
        }
        self.emit(Event::SyncState { sync_id: f.sync_id.clone(), name: f.name.clone(), state: "error".into(), detail: msg.clone() });
        self.emit(Event::SyncError { sync_id: f.sync_id.clone(), message: msg });
    }

    /// What `frontend = "auto"` means for this sync: what it ran with before (so a sync never changes
    /// where its files are by itself), a mirror for syncs that predate mounts, else the platform's best.
    fn resolved_frontend(&self, f: &SyncFolderCfg) -> &'static str {
        match f.frontend.as_str() {
            "mirror" => "mirror",
            "mount" => "mount",
            _ => {
                let dir = self.store.state_dir(&f.sync_id);
                match std::fs::read_to_string(dir.join("frontend")).ok().as_deref().map(str::trim) {
                    Some("mount") => "mount",
                    Some("mirror") => "mirror",
                    _ if dir.join("state.db").exists() => "mirror",
                    _ if cfg!(target_os = "macos") => "mount",
                    _ => "mirror",
                }
            }
        }
    }

    /// Remembers which frontend a sync ran with. A different one starts from a clean engine state: the
    /// files are somewhere else now, and "everything vanished" must not be read as "everything deleted".
    fn note_frontend(&self, sync_id: &str, kind: &str) {
        let dir = self.store.state_dir(sync_id);
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("frontend");
        let before = std::fs::read_to_string(&file).ok().map(|s| s.trim().to_string());
        let changed = match &before {
            Some(b) => b != kind,
            None => dir.join("state.db").exists() && kind != "mirror",
        };
        if changed {
            for n in ["state.db", "state.db-wal", "state.db-shm"] {
                let _ = std::fs::remove_file(dir.join(n));
            }
            let _ = std::fs::remove_dir_all(dir.join("tmp"));
        }
        if before.as_deref() != Some(kind) {
            let _ = std::fs::write(&file, kind);
        }
    }

    async fn frontend_for(&self, f: &SyncFolderCfg, tr: Arc<dyn Transport>) -> (Arc<dyn Frontend>, &'static str, String) {
        let kind = self.resolved_frontend(f);
        let _ = &tr;
        #[cfg(target_os = "macos")]
        if kind == "mount" {
            return match self.ensure_mount(f, tr).await {
                Ok((fe, note)) => (fe, "mount", note.unwrap_or_default()),
                Err(e) => {
                    tracing::warn!("mount of {} failed: {e}", f.name);
                    (Arc::new(Mirror::new(&f.path)), "mirror", format!("Could not mount {}: {e}. It is shown as a normal folder instead.", f.path.display()))
                }
            };
        }
        #[cfg(not(target_os = "macos"))]
        if kind == "mount" {
            return (Arc::new(Mirror::new(&f.path)), "mirror", "Mounting is only available on macOS; showing a normal folder.".into());
        }
        (Arc::new(Mirror::new(&f.path)), "mirror", String::new())
    }

    /// The volume of a paused sync stays up.
    async fn ensure_volume_only(&self, f: &SyncFolderCfg) {
        #[cfg(target_os = "macos")]
        if self.resolved_frontend(f) == "mount" {
            if let Ok(client) = self.client(&f.host_id) {
                let tr: Arc<dyn Transport> = Arc::new(client.transport(&f.sync_id));
                let _ = self.ensure_mount(f, tr).await;
            }
        }
        #[cfg(not(target_os = "macos"))]
        let _ = f;
    }

    /// A cache is only trusted together with the engine state it was filled under. If the state database
    /// is gone (reset, reinstall, a new device with an old cache folder) the engine would read every
    /// cached file as a local addition and push stale content, so the old tree is set aside instead.
    /// Returns where it went, if it did.
    #[cfg(target_os = "macos")]
    fn prepare_cache(&self, sync_id: &str) -> Option<PathBuf> {
        let state = self.store.state_dir(sync_id);
        let cache = self.cache_dir(sync_id);
        let read = |p: PathBuf| std::fs::read_to_string(p).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let (want, have) = (read(state.join("cache-id")), read(cache.join("owner")));
        if state.join("state.db").exists() && want.is_some() && want == have {
            return None;
        }
        let mut aside = None;
        let tree = cache.join("tree");
        let has_files = std::fs::read_dir(&tree).map(|mut r| r.next().is_some()).unwrap_or(false);
        if has_files {
            let dest = cache.join(format!("set-aside-{}", medialake_sync::util::now_secs()));
            if std::fs::create_dir_all(&dest).is_ok() && std::fs::rename(&tree, dest.join("tree")).is_ok() {
                aside = Some(dest);
            } else {
                let _ = std::fs::remove_dir_all(&tree);
            }
        }
        for n in ["index.db", "index.db-wal", "index.db-shm"] {
            let _ = std::fs::remove_file(cache.join(n));
        }
        let id = medialake_sync::util::random_id();
        let _ = std::fs::create_dir_all(&state);
        let _ = std::fs::create_dir_all(&cache);
        let _ = std::fs::write(state.join("cache-id"), &id);
        let _ = std::fs::write(cache.join("owner"), &id);
        aside
    }

    #[cfg(target_os = "macos")]
    async fn ensure_mount(&self, f: &SyncFolderCfg, tr: Arc<dyn Transport>) -> Result<(Arc<dyn Frontend>, Option<String>), String> {
        let mut mounts = self.mounts.lock().await;
        if let Some(s) = mounts.get(&f.sync_id) {
            return Ok((s.frontend(), None));
        }
        let aside = self.prepare_cache(&f.sync_id);
        let cfg = MountConfig {
            sync_id: f.sync_id.clone(),
            name: f.name.clone(),
            point: f.path.clone(),
            cache_dir: self.cache_dir(&f.sync_id),
            read_only: f.access != "rw",
            limit_bytes: f.cache_limit_gb.saturating_mul(1 << 30),
        };
        let session = MountSession::start(cfg, tr).await.map_err(|e| format!("{e:#}"))?;
        self.registry.record(&f.path);
        let events = self.events.clone();
        let sync_id = f.sync_id.clone();
        session.store().set_problem_handler(Some(Arc::new(move |m| {
            let _ = events.send(Event::SyncError { sync_id: sync_id.clone(), message: m });
        })));
        self.stores.lock().unwrap().insert(f.sync_id.clone(), session.store().clone());
        let fe: Arc<dyn Frontend> = session.frontend();
        mounts.insert(f.sync_id.clone(), session);
        let note = aside.map(|d| format!("Files left over from an earlier session were set aside in {}.", d.display()));
        Ok((fe, note))
    }

    #[cfg(target_os = "macos")]
    fn cache_dir(&self, sync_id: &str) -> PathBuf {
        // the system's cache folder for a normal install; a custom data folder (tests, MEDIALAKE_HOME)
        // keeps its cache with it
        let base = if self.root == medialake_proto::platform::data_dir() { medialake_proto::platform::cache_dir() } else { self.root.join("cache") };
        base.join("sync").join(sync_id)
    }

    #[cfg(target_os = "macos")]
    async fn teardown_mount(&self, sync_id: &str) -> Option<PathBuf> {
        let session = self.mounts.lock().await.remove(sync_id)?;
        self.stores.lock().unwrap().remove(sync_id);
        let point = session.point().to_path_buf();
        session.stop().await;
        self.registry.forget(&point);
        Some(point)
    }

    async fn start_folder(&self, f: &SyncFolderCfg) -> Result<(), String> {
        if self.running.lock().unwrap().contains_key(&f.sync_id) {
            return Ok(());
        }
        let client = self.client(&f.host_id)?;
        let m = to_member(f);
        let tr: Arc<dyn Transport> = Arc::new(client.transport(&f.sync_id));
        self.note_frontend(&f.sync_id, self.resolved_frontend(f));
        let (fe, kind, notice) = self.frontend_for(f, tr.clone()).await;
        self.note_frontend(&f.sync_id, kind);
        {
            let mut st = self.status.lock().unwrap();
            if let Some(s) = st.get_mut(&f.sync_id) {
                s.frontend = kind.into();
                s.notice = notice;
            }
        }
        let engine = self.store.engine_with(&m, &self.device(), fe.clone(), tr).map_err(|e| format!("{e:#}"))?;
        let cb = self.report_callback(f);
        let rs = RunningSync::start(engine, fe, &client, &f.sync_id, cb);
        self.running.lock().unwrap().insert(f.sync_id.clone(), rs);
        Ok(())
    }

    fn report_callback(&self, f: &SyncFolderCfg) -> impl Fn(&anyhow::Result<Report>) + Send + 'static {
        let status = self.status.clone();
        let events = self.events.clone();
        let (sync_id, name) = (f.sync_id.clone(), f.name.clone());
        let db_path = self.store.state_dir(&f.sync_id).join("state.db");
        let notified: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        move |r| {
            let pending: Vec<(String, bool, i64)> =
                MemberDb::open(&db_path).map(|d| d.conflicts().into_iter().map(|(e, s)| (e.path, e.deleted, s)).collect()).unwrap_or_default();
            let (state, detail) = match r {
                Err(e) => {
                    let msg = format!("{e:#}");
                    (classify_error(&msg).to_string(), msg)
                }
                Ok(rep) if rep.held_deletes.is_some() => {
                    ("held".to_string(), format!("{} deletes are waiting for confirmation", rep.held_deletes.unwrap_or(0)))
                }
                Ok(_) if !pending.is_empty() => ("conflict".to_string(), format!("{} conflict(s) need a decision", pending.len())),
                Ok(rep) if !rep.errors.is_empty() => {
                    let msg = rep.errors.join("; ");
                    (classify_error(&msg).to_string(), msg)
                }
                Ok(_) => ("idle".to_string(), String::new()),
            };
            let changed = {
                let mut m = status.lock().unwrap();
                let s = m.entry(sync_id.clone()).or_default();
                let changed = s.state != state || s.detail != detail;
                s.state = state.clone();
                s.detail = detail.clone();
                s.pending_conflicts = pending.len();
                if let Ok(rep) = r {
                    s.last_sync = Some(medialake_sync::util::now_secs());
                    s.downloaded = rep.downloaded as u64;
                    s.uploaded = rep.uploaded as u64;
                    s.pushed = rep.pushed as u64;
                    s.held_deletes = rep.held_deletes;
                }
                changed
            };
            if let Ok(rep) = r {
                if rep.downloaded + rep.uploaded + rep.pushed > 0 {
                    let _ = events.send(Event::SyncProgress {
                        sync_id: sync_id.clone(),
                        downloaded: rep.downloaded as u64,
                        uploaded: rep.uploaded as u64,
                        pushed: rep.pushed as u64,
                    });
                }
            }
            if changed {
                let _ = events.send(Event::SyncState { sync_id: sync_id.clone(), name: name.clone(), state: state.clone(), detail: detail.clone() });
                if matches!(state.as_str(), "error" | "offline" | "denied" | "disk_full") {
                    let _ = events.send(Event::SyncError { sync_id: sync_id.clone(), message: detail });
                }
            }
            let mut seen = notified.lock().unwrap();
            for (p, _, _) in &pending {
                if seen.insert(p.clone()) {
                    let _ = events.send(Event::SyncConflict { sync_id: sync_id.clone(), name: name.clone(), path: p.clone() });
                }
            }
            seen.retain(|p| pending.iter().any(|(q, _, _)| q == p));
        }
    }

    /// Stops the engine (the volume of a mount stays).
    async fn stop_folder(&self, sync_id: &str) {
        let rs = self.running.lock().unwrap().remove(sync_id);
        if let Some(rs) = rs {
            let _ = tokio::task::spawn_blocking(move || drop(rs)).await;
        }
    }

    // ---------------- hosts ----------------

    pub fn hosts(&self) -> Vec<HostSummary> {
        self.cfg.read().unwrap().sync.hosts.iter().map(|h| HostSummary { id: h.id.clone(), name: h.name.clone(), address: h.address.clone() }).collect()
    }

    /// Redeems a pairing code for sync. If the code also grants embedding, `embedding` is set so the
    /// caller can fill the embedding server credentials from the same secret.
    pub async fn pair(&self, code: &str) -> Result<(SyncPairResult, SyncHostCfg), String> {
        let device = self.device();
        let paired = pair_ex(code, &device).await.map_err(|e| format!("{e:#}"))?;
        let secret = unhex(&paired.creds.secret_hex).ok_or("corrupt secret")?;
        let host = SyncHostCfg {
            id: paired.creds.id.clone(),
            name: paired.creds.name.clone(),
            address: paired.creds.address.clone(),
            fingerprint: paired.creds.fingerprint.clone(),
            client_id: paired.creds.client_id.clone(),
            secret_protected: remote::protect(&secret)?,
        };
        let old = self.update(|c| {
            let old = c.sync.hosts.iter().position(|h| h.id == host.id).map(|i| c.sync.hosts.remove(i));
            c.sync.hosts.push(host.clone());
            old
        })?;
        if let Some(o) = old {
            remote::discard(&o.secret_protected);
        }
        self.reset_clients();
        let (quality, backend) = paired.embedding.as_ref().map(|i| (i.quality.clone(), i.backend.clone())).unwrap_or_default();
        Ok((
            SyncPairResult {
                host_id: host.id.clone(),
                name: host.name.clone(),
                address: host.address.clone(),
                embedding: paired.embedding.is_some(),
                quality,
                backend,
            },
            host,
        ))
    }

    pub async fn forget_host(&self, host_id: &str) -> Result<(), String> {
        if self.cfg.read().unwrap().sync.folders.iter().any(|f| f.host_id == host_id) {
            return Err("syncs of this host are still joined; leave them first".into());
        }
        let old = self.update(|c| c.sync.hosts.iter().position(|h| h.id == host_id).map(|i| c.sync.hosts.remove(i)))?;
        match old {
            Some(o) => {
                remote::discard(&o.secret_protected);
                self.reset_clients();
                Ok(())
            }
            None => Err(format!("host {host_id} is not paired")),
        }
    }

    pub async fn available(&self, host_id: &str) -> Result<Vec<AvailableSync>, String> {
        let client = self.client(host_id)?;
        let syncs = client.list_syncs().await.map_err(|e| format!("{e:#}"))?;
        let joined: HashSet<String> = self.cfg.read().unwrap().sync.folders.iter().map(|f| f.sync_id.clone()).collect();
        Ok(syncs
            .into_iter()
            .map(|s| AvailableSync {
                joined: joined.contains(&s.id),
                id: s.id,
                name: s.name,
                access: s.access.as_str().into(),
                files: s.files,
                bytes: s.bytes,
                backup: s.backup,
            })
            .collect())
    }

    // ---------------- membership ----------------

    pub async fn join(self: &Arc<Self>, host_id: &str, sync_id: &str, path: &Path, frontend: &str, conflict: &str) -> Result<SyncFolderCfg, String> {
        if ConflictPolicy::parse(conflict).is_none() {
            return Err("conflict policy must be server_wins or ask".into());
        }
        let frontend = match frontend {
            "" | "auto" => "auto",
            "mirror" => "mirror",
            "mount" if cfg!(target_os = "macos") => "mount",
            "mount" => return Err("mounting is only available on macOS".into()),
            other => return Err(format!("unknown frontend {other:?}")),
        };
        let client = self.client(host_id)?;
        let syncs = client.list_syncs().await.map_err(|e| format!("{e:#}"))?;
        let info = syncs
            .iter()
            .find(|s| s.id == sync_id || s.name == sync_id)
            .ok_or_else(|| format!("this host does not offer {sync_id:?} to this device"))?
            .clone();
        if self.cfg.read().unwrap().sync.folders.iter().any(|f| f.sync_id == info.id) {
            return Err(format!("{:?} is already joined", info.name));
        }
        std::fs::create_dir_all(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let path = crate::config::normalize_path(&path.canonicalize().map_err(|e| e.to_string())?);
        if self.cfg.read().unwrap().sync.folders.iter().any(|f| f.path == path) {
            return Err("another sync already uses this folder".into());
        }
        let f = SyncFolderCfg {
            sync_id: info.id.clone(),
            host_id: host_id.to_string(),
            name: info.name.clone(),
            path,
            frontend: frontend.into(),
            conflict: conflict.into(),
            access: info.access.as_str().into(),
            ..Default::default()
        };
        self.update(|c| c.sync.folders.push(f.clone()))?;
        self.init_status(&f);
        self.start_folder(&f).await?;
        Ok(f)
    }

    pub async fn leave(&self, sync_id: &str, keep_files: bool) -> Result<(), String> {
        let f = self.folder(sync_id)?;
        self.stop_folder(&f.sync_id).await;
        // a mount goes away; what was downloaded is kept as plain files if asked for
        #[cfg(target_os = "macos")]
        {
            let store = self.stores.lock().unwrap().get(&f.sync_id).cloned();
            if self.teardown_mount(&f.sync_id).await.is_some() {
                if let Some(store) = store {
                    if keep_files {
                        let (dest, s2) = (f.path.clone(), store.clone());
                        let r = tokio::task::spawn_blocking(move || s2.export_to(&dest)).await.map_err(|e| e.to_string())?;
                        r.map_err(|e| format!("keeping the downloaded files: {e:#}"))?;
                    }
                }
                let _ = std::fs::remove_dir_all(self.cache_dir(&f.sync_id));
            }
        }
        self.update(|c| c.sync.folders.retain(|x| x.sync_id != f.sync_id))?;
        self.status.lock().unwrap().remove(&f.sync_id);
        let _ = std::fs::remove_dir_all(self.store.state_dir(&f.sync_id));
        if !keep_files {
            std::fs::remove_dir_all(&f.path).map_err(|e| format!("removing {}: {e}", f.path.display()))?;
        }
        Ok(())
    }

    /// Changes `conflict`, `paused`, `cache_limit_gb` or `index_for_search` of a joined sync.
    pub async fn set_folder(&self, sync_id: &str, patch: &serde_json::Value) -> Result<SyncFolderCfg, String> {
        let f = self.folder(sync_id)?;
        let id = f.sync_id.clone();
        if let Some(p) = patch["conflict"].as_str() {
            if ConflictPolicy::parse(p).is_none() {
                return Err("conflict policy must be server_wins or ask".into());
            }
        }
        if let Some(p) = patch["frontend"].as_str() {
            if !matches!(p, "auto" | "mirror" | "mount") {
                return Err("frontend must be auto, mirror or mount".into());
            }
            if p != f.frontend {
                return Err("changing how a sync is shown means leaving and joining it again".into());
            }
        }
        let engine_change = patch["conflict"].as_str().is_some_and(|p| p != f.conflict) || patch["paused"].as_bool().is_some_and(|p| p != f.paused);
        let updated = self.update(|c| {
            let x = c.sync.folders.iter_mut().find(|x| x.sync_id == id).unwrap();
            if let Some(p) = patch["conflict"].as_str() {
                x.conflict = p.to_string();
            }
            if let Some(b) = patch["paused"].as_bool() {
                x.paused = b;
            }
            if let Some(n) = patch["cache_limit_gb"].as_u64() {
                x.cache_limit_gb = n;
            }
            if let Some(b) = patch["index_for_search"].as_bool() {
                x.index_for_search = b;
            }
            x.clone()
        })?;
        self.init_status(&updated);
        #[cfg(target_os = "macos")]
        if let Some(s) = self.stores.lock().unwrap().get(&id) {
            s.set_limit(updated.cache_limit_gb.saturating_mul(1 << 30));
            s.enforce_limit();
        }
        if !engine_change {
            return Ok(updated);
        }
        // a changed policy or pause state needs a fresh engine
        self.stop_folder(&id).await;
        if !updated.paused {
            self.start_folder(&updated).await?;
        } else {
            self.emit(Event::SyncState { sync_id: id, name: updated.name.clone(), state: "paused".into(), detail: String::new() });
        }
        Ok(updated)
    }

    pub async fn set_all_paused(&self, paused: bool) -> Result<(), String> {
        let ids: Vec<String> = self.cfg.read().unwrap().sync.folders.iter().map(|f| f.sync_id.clone()).collect();
        for id in ids {
            self.set_folder(&id, &serde_json::json!({ "paused": paused })).await?;
        }
        Ok(())
    }

    /// Looks at everything again from the host's first revision.
    pub fn verify(&self, sync_id: &str) -> Result<(), String> {
        let f = self.folder(sync_id)?;
        match self.running.lock().unwrap().get(&f.sync_id) {
            Some(r) => {
                r.verify();
                Ok(())
            }
            None => Err("the sync is paused".into()),
        }
    }

    pub fn status(&self) -> Vec<SyncStatus> {
        let folders = self.cfg.read().unwrap().sync.folders.clone();
        let m = self.status.lock().unwrap();
        #[allow(unused_mut)]
        let mut out: Vec<SyncStatus> = folders.iter().filter_map(|f| m.get(&f.sync_id).cloned()).collect();
        #[cfg(target_os = "macos")]
        {
            let stores = self.stores.lock().unwrap();
            for s in out.iter_mut() {
                if let Some(st) = stores.get(&s.sync_id) {
                    let n = st.stats();
                    s.cached_bytes = n.cached_bytes;
                    s.cache_limit_bytes = n.limit_bytes;
                    s.unsynced_files = n.dirty_files;
                }
            }
        }
        out
    }

    // ---------------- conflicts ----------------

    pub fn conflicts(&self) -> Vec<ConflictItem> {
        let folders = self.cfg.read().unwrap().sync.folders.clone();
        let mut out = vec![];
        for f in folders {
            if let Ok(db) = MemberDb::open(&self.store.state_dir(&f.sync_id).join("state.db")) {
                for (e, since) in db.conflicts() {
                    out.push(ConflictItem { sync_id: f.sync_id.clone(), name: f.name.clone(), path: e.path, host_deleted: e.deleted, since });
                }
            }
        }
        out
    }

    pub async fn resolve(&self, sync_id: &str, path: &str, choice: &str) -> Result<bool, String> {
        let f = self.folder(sync_id)?;
        let c = Choice::parse(choice).ok_or("choice must be mine, server or both")?;
        let path = path.to_string();
        let me_running = self.running.lock().unwrap().contains_key(&f.sync_id);
        if !me_running {
            return Err("the sync is paused: resume it to resolve conflicts".into());
        }
        let id = f.sync_id.clone();
        // the engine thread owns the engine: hand the work to it
        let r = {
            let g = self.running.lock().unwrap();
            let rs = g.get(&id).ok_or("the sync is not running")?;
            let rx = rs
                .call_async(move |e| e.resolve(&path, c).map_err(|e| format!("{e:#}")))
                .map_err(|e| format!("{e:#}"))?;
            drop(g);
            rx
        };
        tokio::task::spawn_blocking(move || r.recv_timeout(std::time::Duration::from_secs(120)).map_err(|_| "the sync did not answer".to_string())?)
            .await
            .map_err(|e| e.to_string())?
    }

    pub fn confirm_deletes(&self, sync_id: &str) -> Result<(), String> {
        let f = self.folder(sync_id)?;
        let g = self.running.lock().unwrap();
        let rs = g.get(&f.sync_id).ok_or("the sync is paused")?;
        rs.call_async(|e| {
            let _ = e.confirm_deletes();
        })
        .map_err(|e| format!("{e:#}"))?;
        Ok(())
    }

    // ---------------- files ----------------

    /// Entries of a folder, with what each one costs on this device (mount) or `local` (mirror).
    pub fn browse(&self, sync_id: &str, rel: &str) -> Result<Vec<BrowseItem>, String> {
        let f = self.folder(sync_id)?;
        let rel = rel.trim_matches('/');
        if !rel.is_empty() {
            normalize_path(rel).map_err(|e| e.to_string())?;
        }
        #[cfg(target_os = "macos")]
        if let Some(store) = self.stores.lock().unwrap().get(&f.sync_id).cloned() {
            let items = store.browse(rel).map_err(|e| format!("{e:#}"))?;
            return Ok(items
                .into_iter()
                .map(|i| BrowseItem {
                    name: i.name,
                    path: i.path,
                    is_dir: i.is_dir,
                    size: i.size,
                    mtime: i.mtime,
                    state: i.state.into(),
                    pinned: i.pinned,
                    cached_bytes: i.cached_bytes,
                })
                .collect());
        }
        let dir = if rel.is_empty() { f.path.clone() } else { f.path.join(rel) };
        let mut out = vec![];
        for ent in std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?.flatten() {
            let Some(name) = ent.file_name().to_str().map(str::to_string) else { continue };
            let child = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            if is_ignored(&child, &[]) {
                continue;
            }
            let Ok(m) = ent.metadata() else { continue };
            out.push(BrowseItem {
                name,
                path: child,
                is_dir: m.is_dir(),
                size: if m.is_dir() { 0 } else { m.len() },
                mtime: medialake_sync::util::mtime_ms(&m),
                state: "local".into(),
                pinned: false,
                cached_bytes: if m.is_dir() { 0 } else { m.len() },
            });
        }
        out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        Ok(out)
    }

    /// "Keep on this device": fetches `rel` (a file, a folder, or `""` for everything) now and keeps it
    /// out of cache eviction. Mounts only.
    pub fn pin(&self, sync_id: &str, rel: &str) -> Result<(), String> {
        let f = self.folder(sync_id)?;
        let rel = rel.trim_matches('/');
        if !rel.is_empty() {
            normalize_path(rel).map_err(|e| e.to_string())?;
        }
        #[cfg(target_os = "macos")]
        if let Some(store) = self.stores.lock().unwrap().get(&f.sync_id).cloned() {
            return store.pin(rel).map_err(|e| format!("{e:#}"));
        }
        let _ = f;
        Err("this sync is a plain folder: every file is always kept on this device".into())
    }

    /// "Free up space": the content may be removed from the cache again (it stays listed).
    pub fn unpin(&self, sync_id: &str, rel: &str, evict_now: bool) -> Result<(), String> {
        let f = self.folder(sync_id)?;
        let rel = rel.trim_matches('/');
        #[cfg(target_os = "macos")]
        if let Some(store) = self.stores.lock().unwrap().get(&f.sync_id).cloned() {
            store.unpin(rel).map_err(|e| format!("{e:#}"))?;
            if evict_now {
                let rel = rel.to_string();
                std::thread::spawn(move || {
                    // a folder: every file below it; a file: itself
                    let mut stack = vec![rel];
                    while let Some(p) = stack.pop() {
                        match store.browse(&p) {
                            Ok(items) => {
                                for i in items {
                                    if i.is_dir {
                                        stack.push(i.path);
                                    } else {
                                        let _ = store.evict(&i.path);
                                    }
                                }
                            }
                            Err(_) => {
                                let _ = store.evict(&p);
                            }
                        }
                    }
                });
            }
            return Ok(());
        }
        let _ = (f, evict_now);
        Err("this sync is a plain folder: every file is always kept on this device".into())
    }

    /// (cache tree, mount point) of every mounted sync: the local tree is what can be watched.
    #[cfg(target_os = "macos")]
    pub fn mount_trees(&self) -> Vec<(PathBuf, PathBuf)> {
        let folders = self.cfg.read().unwrap().sync.folders.clone();
        let stores = self.stores.lock().unwrap();
        folders.iter().filter_map(|f| stores.get(&f.sync_id).map(|s| (s.tree().to_path_buf(), f.path.clone()))).collect()
    }

    #[cfg(not(target_os = "macos"))]
    pub fn mount_trees(&self) -> Vec<(PathBuf, PathBuf)> {
        vec![]
    }

    /// True if reading `path` (somewhere inside a mounted sync) would have to download it. The search
    /// indexer asks so that indexing never downloads anything.
    pub fn would_download(&self, path: &Path) -> bool {
        #[cfg(target_os = "macos")]
        {
            let folders = self.cfg.read().unwrap().sync.folders.clone();
            let stores = self.stores.lock().unwrap();
            for f in folders {
                if let (Some(store), Ok(rel)) = (stores.get(&f.sync_id), path.strip_prefix(&f.path)) {
                    return rel.to_str().is_some_and(|r| store.is_dehydrated(r));
                }
            }
        }
        let _ = path;
        false
    }

    // ---------------- history (on the host) ----------------

    fn transport(&self, sync_id: &str) -> Result<impl Transport + use<>, String> {
        let f = self.folder(sync_id)?;
        Ok(self.client(&f.host_id)?.transport(&f.sync_id))
    }

    pub async fn versions(&self, sync_id: &str, path: &str) -> Result<Vec<medialake_proto::sync::VersionInfo>, String> {
        let t = self.transport(sync_id)?;
        let path = path.to_string();
        tokio::task::spawn_blocking(move || t.versions(&path).map_err(|e| format!("{e:#}"))).await.map_err(|e| e.to_string())?
    }

    pub async fn snapshots(&self, sync_id: &str) -> Result<Vec<medialake_proto::sync::SnapshotInfo>, String> {
        let t = self.transport(sync_id)?;
        tokio::task::spawn_blocking(move || t.snapshots().map_err(|e| format!("{e:#}"))).await.map_err(|e| e.to_string())?
    }

    /// `what`: `{"file": {"path", "rev"}}` or `{"snapshot": {"id", "path", "prune"}}`. A restore is a normal new
    /// commit on the host, so it reaches every member (this one at its next round).
    pub async fn restore(&self, sync_id: &str, what: &serde_json::Value, as_copy: bool) -> Result<usize, String> {
        let w = if let Some(f) = what.get("file") {
            RestoreWhat::File { path: f["path"].as_str().ok_or("missing path")?.to_string(), rev: f["rev"].as_u64().ok_or("missing rev")? }
        } else if let Some(s) = what.get("snapshot") {
            RestoreWhat::Snapshot {
                id: s["id"].as_u64().ok_or("missing id")?,
                path: s["path"].as_str().unwrap_or_default().to_string(),
                prune: s["prune"].as_bool().unwrap_or(false),
            }
        } else {
            return Err("what must be {file:{path,rev}} or {snapshot:{id,path}}".into());
        };
        let t = self.transport(sync_id)?;
        let results = tokio::task::spawn_blocking(move || t.restore(w, as_copy).map_err(|e| format!("{e:#}"))).await.map_err(|e| e.to_string())??;
        if let Some(r) = self.running.lock().unwrap().get(&self.folder(sync_id)?.sync_id) {
            r.nudge();
        }
        Ok(results.iter().filter(|r| matches!(r, ChangeResult::Ok { .. })).count())
    }

    // ---------------- hosting ----------------

    fn host_state_dir(&self) -> PathBuf {
        self.root.join("host")
    }

    fn public_addr(&self, bind: &std::net::SocketAddr) -> String {
        let c = self.cfg.read().unwrap();
        if c.sync.hosting.public_addr.trim().is_empty() {
            if bind.ip().is_loopback() {
                // a loopback listener cannot be reached under the computer's name
                format!("{}:{}", bind.ip(), bind.port())
            } else {
                format!("{}:{}", medialake_proto::platform::host_name("localhost"), bind.port())
            }
        } else {
            c.sync.hosting.public_addr.trim().to_string()
        }
    }

    /// Starts the listener when folders are shared, stops it when none are, and keeps `syncs.json`
    /// (which a running host re-reads) in step with the configuration.
    pub async fn ensure_hosting(self: &Arc<Self>) -> Result<(), String> {
        let hosted: Vec<HostedConfig> = self.cfg.read().unwrap().sync.hosted.clone();
        let dir = self.host_state_dir();
        let mut g = self.hosting.lock().await;
        if hosted.is_empty() {
            if let Some(h) = g.take() {
                h.server.abort();
                h.background.abort();
            }
            return Ok(());
        }
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        SyncsFile { syncs: hosted }.save(&dir).map_err(|e| format!("{e:#}"))?;
        if g.is_some() {
            *self.hosting_error.lock().unwrap() = String::new();
            return Ok(());
        }
        let bind: std::net::SocketAddr =
            self.cfg.read().unwrap().sync.hosting.bind.parse().map_err(|e| format!("bad hosting address: {e}"))?;
        let state = Arc::new(StateDir::open(&dir).map_err(|e| format!("{e:#}"))?);
        let ident = state.identity().map_err(|e| format!("{e:#}"))?;
        let tls = pinning::server_tls(ident.cert_der.clone(), ident.key_der.clone())?;
        let host_name = medialake_proto::platform::host_name("MediaLake");
        let st2 = state.clone();
        let host = tokio::task::spawn_blocking(move || SyncHost::new(st2, &host_name)).await.map_err(|e| e.to_string())?;
        let server = RpcServer::new(bind, SyncOnlyService { host: host.clone() })
            .await
            .map_err(|e| format!("cannot listen on {bind}: {e:?}"))?
            .with_persistence(host.persistent())
            .with_config(ServerConfig { max_message_bytes: MAX_MESSAGE_BYTES, tls: Some(tls), ..Default::default() });
        let addr = server.local_addr().map_err(|e| e.to_string())?;
        state.set_public_addr(&self.public_addr(&addr));
        let background = host.start_background();
        let task = tokio::spawn(async move { server.run().await });
        *g = Some(Hosting { host, state, server: task, background, addr });
        *self.hosting_error.lock().unwrap() = String::new();
        Ok(())
    }

    pub async fn hosting_status(&self) -> HostingStatus {
        let cfg = self.cfg.read().unwrap().sync.clone();
        let g = self.hosting.lock().await;
        let mut syncs = vec![];
        if let Some(h) = g.as_ref() {
            for s in h.host.all() {
                let info = {
                    let s2 = s.clone();
                    tokio::task::spawn_blocking(move || s2.info(Access::ReadWrite)).await.ok()
                };
                let c = s.config();
                syncs.push(HostedSyncInfo {
                    id: c.id,
                    name: c.name,
                    root: c.root.display().to_string(),
                    backup: c.backup.describe(),
                    files: info.as_ref().map(|i| i.files).unwrap_or(0),
                    bytes: info.as_ref().map(|i| i.bytes).unwrap_or(0),
                    seq: info.as_ref().map(|i| i.seq).unwrap_or(0),
                    issues: s.issues(),
                });
            }
        }
        let addr = g.as_ref().map(|h| h.addr);
        HostingStatus {
            running: g.is_some(),
            bind: cfg.hosting.bind.clone(),
            public_addr: addr.map(|a| self.public_addr(&a)).unwrap_or_default(),
            error: self.hosting_error.lock().unwrap().clone(),
            syncs,
        }
    }

    pub async fn host_add(self: &Arc<Self>, name: &str, root: &Path, backup: &str, store: Option<PathBuf>) -> Result<HostedConfig, String> {
        let policy = if backup.is_empty() { BackupPolicy::default() } else { BackupPolicy::parse(backup)? };
        let mut file = SyncsFile { syncs: self.cfg.read().unwrap().sync.hosted.clone() };
        let cfg = file.add(name, root, store, policy).map_err(|e| format!("{e:#}"))?;
        self.update(|c| c.sync.hosted = file.syncs.clone())?;
        self.ensure_hosting().await?;
        Ok(cfg)
    }

    pub async fn host_set(self: &Arc<Self>, sync: &str, backup: Option<&str>, ignore: Option<Vec<String>>) -> Result<HostedConfig, String> {
        let policy = backup.map(BackupPolicy::parse).transpose()?;
        let r = self.update(|c| {
            let h = c.sync.hosted.iter_mut().find(|h| h.id == sync || h.name == sync)?;
            if let Some(p) = policy {
                h.backup = p;
            }
            if let Some(i) = ignore {
                h.ignore = i;
            }
            Some(h.clone())
        })?;
        let cfg = r.ok_or_else(|| format!("no hosted sync {sync:?}"))?;
        self.ensure_hosting().await?;
        Ok(cfg)
    }

    pub async fn host_remove(self: &Arc<Self>, sync: &str) -> Result<(), String> {
        let id = self
            .update(|c| c.sync.hosted.iter().position(|h| h.id == sync || h.name == sync).map(|i| c.sync.hosted.remove(i).id))?
            .ok_or_else(|| format!("no hosted sync {sync:?}"))?;
        if let Ok(state) = StateDir::open(&self.host_state_dir()) {
            let _ = state.drop_sync_grants(&id);
        }
        self.ensure_hosting().await
    }

    /// A pairing code for a new device: `grants` is `[{"sync": name or id, "access": "rw"|"ro"}]`.
    pub async fn invite(&self, grants: &[(String, String)]) -> Result<String, String> {
        let g = self.hosting.lock().await;
        let Some(h) = g.as_ref() else { return Err("nothing is shared yet".into()) };
        let hosted = self.cfg.read().unwrap().sync.hosted.clone();
        let mut gr = Grants { embed: false, syncs: BTreeMap::new() };
        for (sync, access) in grants {
            let acc = Access::parse(access).ok_or_else(|| format!("{access:?}: expected rw or ro"))?;
            let cfg = hosted.iter().find(|x| x.id == *sync || x.name == *sync).ok_or_else(|| format!("no hosted sync {sync:?}"))?;
            gr.syncs.insert(cfg.id.clone(), acc.as_str().to_string());
        }
        if gr.syncs.is_empty() {
            return Err("choose at least one sync".into());
        }
        let code = h.state.new_pairing(&self.public_addr(&h.addr), gr).map_err(|e| format!("{e:#}"))?;
        Ok(code.encode())
    }

    pub async fn clients(&self) -> Vec<HostedClientInfo> {
        let hosted = self.cfg.read().unwrap().sync.hosted.clone();
        let Ok(state) = StateDir::open(&self.host_state_dir()) else { return vec![] };
        state
            .list_clients()
            .into_iter()
            .map(|(id, c)| HostedClientInfo {
                id,
                name: c.name,
                created: c.created,
                embed: c.embed,
                syncs: c
                    .syncs
                    .into_iter()
                    .map(|(sid, a)| (hosted.iter().find(|h| h.id == sid).map(|h| h.name.clone()).unwrap_or(sid), a))
                    .collect(),
            })
            .collect()
    }

    /// `access`: `rw`, `ro`, or `none` to take it away.
    pub async fn grant(&self, client: &str, sync: &str, access: &str) -> Result<(), String> {
        let hosted = self.cfg.read().unwrap().sync.hosted.clone();
        let cfg = hosted.iter().find(|x| x.id == sync || x.name == sync).ok_or_else(|| format!("no hosted sync {sync:?}"))?;
        let state = StateDir::open(&self.host_state_dir()).map_err(|e| format!("{e:#}"))?;
        let id = state.find_client(client).ok_or_else(|| format!("no client {client:?}"))?;
        let acc = match access {
            "none" => None,
            a => Some(Access::parse(a).ok_or("access must be rw, ro or none")?),
        };
        state.set_grant(&id, &cfg.id, acc).map_err(|e| format!("{e:#}"))
    }

    /// Removes a paired device altogether (its notification stream closes within a heartbeat).
    pub async fn revoke_client(&self, client: &str) -> Result<(), String> {
        let state = StateDir::open(&self.host_state_dir()).map_err(|e| format!("{e:#}"))?;
        let id = state.find_client(client).ok_or_else(|| format!("no client {client:?}"))?;
        state.revoke(&id).map_err(|e| format!("{e:#}"))?;
        Ok(())
    }

    /// Takes a snapshot of a hosted sync now (time-based strategy only).
    pub async fn host_snapshot(&self, sync: &str) -> Result<u64, String> {
        let g = self.hosting.lock().await;
        let h = g.as_ref().ok_or("nothing is shared yet")?;
        let cfg = {
            let hosted = self.cfg.read().unwrap().sync.hosted.clone();
            hosted.into_iter().find(|x| x.id == sync || x.name == sync).ok_or_else(|| format!("no hosted sync {sync:?}"))?
        };
        let hs = h.host.sync(&cfg.id).ok_or("the sync is not running")?;
        drop(g);
        tokio::task::spawn_blocking(move || hs.snapshot_now().map_err(|e| format!("{e:#}"))).await.map_err(|e| e.to_string())?
    }
}
