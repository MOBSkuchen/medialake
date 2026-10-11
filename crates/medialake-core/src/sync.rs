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
    /// The frontend actually in use. Only `mirror` exists until the platform frontends land.
    pub frontend: String,
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
    /// `local` for a mirror: every file is a full copy.
    pub state: String,
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
}

/// Maps an engine / transport error message to the state the UI shows (§12 X2: host unreachable,
/// disk full, revoked access). The engine retries with backoff in every one of these states.
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
        let folders = self.cfg.read().unwrap().sync.folders.clone();
        for f in folders {
            self.init_status(&f);
            if !f.paused {
                if let Err(e) = self.start_folder(&f) {
                    self.fail(&f, e);
                }
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
        if let Some(h) = self.hosting.lock().await.take() {
            h.server.abort();
            h.background.abort();
        }
    }

    fn init_status(&self, f: &SyncFolderCfg) {
        let host_name = self.cfg.read().unwrap().sync.hosts.iter().find(|h| h.id == f.host_id).map(|h| h.name.clone()).unwrap_or_default();
        let pending = MemberDb::open(&self.store.state_dir(&f.sync_id).join("state.db")).map(|d| d.conflicts().len()).unwrap_or(0);
        let mut m = self.status.lock().unwrap();
        let s = m.entry(f.sync_id.clone()).or_default();
        s.sync_id = f.sync_id.clone();
        s.name = f.name.clone();
        s.path = f.path.display().to_string();
        s.host_id = f.host_id.clone();
        s.host_name = host_name;
        s.access = f.access.clone();
        s.conflict_policy = f.conflict.clone();
        s.frontend = "mirror".into();
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

    fn start_folder(&self, f: &SyncFolderCfg) -> Result<(), String> {
        if self.running.lock().unwrap().contains_key(&f.sync_id) {
            return Ok(());
        }
        let client = self.client(&f.host_id)?;
        let m = to_member(f);
        let engine = self.store.engine(&m, &client, &self.device()).map_err(|e| format!("{e:#}"))?;
        let fe: Arc<dyn Frontend> = Arc::new(Mirror::new(&f.path));
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
        self.start_folder(&f)?;
        Ok(f)
    }

    pub async fn leave(&self, sync_id: &str, keep_files: bool) -> Result<(), String> {
        let f = self.folder(sync_id)?;
        self.stop_folder(&f.sync_id).await;
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
        // a changed policy or pause state needs a fresh engine
        self.stop_folder(&id).await;
        if !updated.paused {
            self.start_folder(&updated)?;
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
        folders.iter().filter_map(|f| m.get(&f.sync_id).cloned()).collect()
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

    /// Entries of a folder of a mirror (every file is a full copy on this device).
    pub fn browse(&self, sync_id: &str, rel: &str) -> Result<Vec<BrowseItem>, String> {
        let f = self.folder(sync_id)?;
        let rel = rel.trim_matches('/');
        if !rel.is_empty() {
            normalize_path(rel).map_err(|e| e.to_string())?;
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
            });
        }
        out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        Ok(out)
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
