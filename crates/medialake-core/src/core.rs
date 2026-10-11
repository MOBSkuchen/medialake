//! The `Core` facade: owns config, store, index, server connection, indexer and watcher.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use medialake_proto::TextKind;
use medialake_proto::TextItem;
use medialake_proto::models::{Model, Quality};
use serde::Serialize;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

use crate::client::{ClientError, EmbedClient, Lane};
use crate::config::{Config, IndexedDir, Paths, ServerMode};
use crate::events::Event;
use crate::indexer::Indexer;
use crate::remote::{RemoteBackend, RemoteConn};
use crate::search::{self, Ranking, SearchHit};
use crate::sync::{SyncManager, SyncPairResult};
use crate::store::Store;
use crate::supervisor::{Backend, ServerState, Supervisor, SupervisorConfig};
use crate::vindex::VectorIndex;
use crate::watcher::FileWatcher;

#[derive(Default, Clone)]
pub struct StartOptions {
    /// Path of `medialake-server` (with the platform executable suffix) (default: next to the current executable).
    pub server_exe: Option<PathBuf>,
    /// Overrides the models directory.
    pub model_dir: Option<PathBuf>,
    pub gpu_layers: Option<i32>,
    /// Do not watch the file system (tests, one-shot CLI runs).
    pub no_watch: bool,
    /// Number of indexing workers.
    pub workers: Option<usize>,
}

#[derive(Serialize, Clone, Debug)]
pub struct Status {
    pub files: u64,
    pub chunks: u64,
    pub vectors: usize,
    pub queued: usize,
    pub busy: bool,
    pub server_state: String,
    pub server_detail: String,
    pub backend: String,
    pub model: String,
    pub quality: String,
    pub mode: String,
    pub by_status: Vec<(String, u64)>,
}

#[derive(Serialize, Clone, Debug)]
pub struct PairResult {
    pub address: String,
    pub model: String,
    pub quality: String,
    pub backend: String,
    pub reindex: bool,
}

/// The currently active source of embeddings.
struct Active {
    supervisor: Option<Arc<Supervisor>>,
    remote: Option<Arc<RemoteBackend>>,
    rx: watch::Receiver<ServerState>,
    forwarder: Option<JoinHandle<()>>,
}

pub struct Core {
    pub paths: Paths,
    pub cfg: Arc<RwLock<Config>>,
    pub store: Arc<Mutex<Store>>,
    pub vindex: Arc<VectorIndex>,
    pub client: Arc<EmbedClient>,
    pub indexer: Arc<Indexer>,
    watcher: Mutex<Option<Arc<FileWatcher>>>,
    events: broadcast::Sender<Event>,
    active: Mutex<Active>,
    opts: StartOptions,
    model_dir: PathBuf,
    download_cancel: Arc<AtomicBool>,
    downloading: AtomicBool,
    sync: Mutex<Option<Arc<SyncManager>>>,
}

fn default_server_exe() -> PathBuf {
    if let Ok(p) = std::env::var("MEDIALAKE_SERVER_EXE") {
        return PathBuf::from(p);
    }
    let me = std::env::current_exe().unwrap_or_default();
    me.parent().unwrap_or(std::path::Path::new(".")).join(format!("medialake-server{}", std::env::consts::EXE_SUFFIX))
}

impl Core {
    pub async fn start(paths: Paths, opts: StartOptions) -> anyhow::Result<Arc<Core>> {
        paths.ensure()?;
        let cfg = Config::load(&paths.config_file)?;
        let search_on = cfg.search_enabled;
        let store = Store::open(&paths.db_file)?;

        // changing model or quality invalidates every vector
        let (model, quality) = (cfg.model.as_str(), cfg.quality.as_str());
        let (old_model, old_quality) = (store.meta("model_id"), store.meta("quality"));
        if old_model.as_deref().is_some_and(|m| m != model) || old_quality.as_deref().is_some_and(|q| q != quality) {
            tracing::warn!("model/quality changed {old_model:?}/{old_quality:?} -> {model}/{quality}: clearing the index");
            store.clear_all()?;
            store.set_meta("index_clean", "0")?;
        }
        store.set_meta("model_id", model)?;
        store.set_meta("quality", quality)?;

        let vindex = Arc::new(VectorIndex::open(&paths.index_file, &store, cfg.model.dims()).map_err(anyhow::Error::msg)?);
        let store = Arc::new(Mutex::new(store));
        let (events, _) = broadcast::channel(256);

        let model_dir = opts
            .model_dir
            .clone()
            .or_else(|| std::env::var_os("MEDIALAKE_MODEL_DIR").map(PathBuf::from))
            .unwrap_or_else(|| paths.models_dir.clone());

        // inactive placeholder channel until the first backend is attached
        let (_tx, idle_rx) = watch::channel(ServerState::Idle);
        let client = EmbedClient::new(idle_rx.clone(), None);
        client.expect_model(cfg.model);
        let cfg = Arc::new(RwLock::new(cfg));

        let indexer = Indexer::new(store.clone(), vindex.clone(), client.clone(), cfg.clone(), events.clone());
        // workers wait for work: they are always there, search "off" just keeps them paused
        indexer.start_workers(opts.workers.unwrap_or(2));
        if !search_on {
            indexer.set_paused(true);
        }

        let watcher = if opts.no_watch || !search_on {
            None
        } else {
            let w = FileWatcher::start(indexer.clone(), cfg.clone())?;
            w.set_dirs(&cfg.read().unwrap().dirs);
            Some(w)
        };

        let core = Arc::new(Core {
            paths,
            cfg,
            store,
            vindex,
            client,
            indexer,
            watcher: Mutex::new(watcher),
            events,
            active: Mutex::new(Active { supervisor: None, remote: None, rx: idle_rx, forwarder: None }),
            opts,
            model_dir,
            download_cancel: Arc::new(AtomicBool::new(false)),
            downloading: AtomicBool::new(false),
            sync: Mutex::new(None),
        });

        // pick the server: a paired remote one if configured, otherwise the bundled local one
        let c = core.config();
        let mut started_remote = false;
        if search_on && c.server_mode == ServerMode::Remote {
            match c.remote.as_ref().map(RemoteConn::from_creds) {
                Some(Ok(conn)) => {
                    core.attach_remote(conn);
                    started_remote = true;
                }
                Some(Err(e)) => {
                    tracing::error!("remote credentials unusable: {e}");
                    let _ = core.events.send(Event::Error { message: format!("Remote server credentials unusable: {e}") });
                }
                None => {}
            }
        }
        if search_on && !started_remote {
            core.attach_local();
        }

        // catch up on whatever changed while we were not running
        if search_on {
            let me = core.clone();
            tokio::spawn(async move {
                me.indexer.reconcile().await;
            });
        }
        if core.config().sync.enabled {
            if let Err(e) = core.start_sync().await {
                tracing::error!("sync: {e}");
                let _ = core.events.send(Event::Error { message: format!("Sync could not start: {e}") });
            }
        }
        Ok(core)
    }

    // ---------- server selection ----------

    fn supervisor_config(&self) -> SupervisorConfig {
        let cfg = self.config();
        SupervisorConfig {
            exe: self.opts.server_exe.clone().unwrap_or_else(default_server_exe),
            fallback_exe: {
                let exe = self.opts.server_exe.clone().unwrap_or_else(default_server_exe);
                let cpu = exe.with_file_name(format!("medialake-server-cpu{}", std::env::consts::EXE_SUFFIX));
                cpu.exists().then_some(cpu)
            },
            model_dir: self.model_dir.clone(),
            log_dir: Some(self.paths.logs_dir.clone()),
            model: cfg.model,
            quality: cfg.quality,
            gpu_layers: self.opts.gpu_layers,
            idle_shutdown: Duration::from_secs(cfg.idle_shutdown_minutes as u64 * 60),
        }
    }

    /// Mirrors server state changes to the UI.
    fn forward_state(&self, mut rx: watch::Receiver<ServerState>) -> JoinHandle<()> {
        let ev = self.events.clone();
        let client = self.client.clone();
        let cfg = self.cfg.clone();
        tokio::spawn(async move {
            loop {
                let st = rx.borrow_and_update().clone();
                let detail = match &st {
                    ServerState::Failed(why) => why.clone(),
                    _ => String::new(),
                };
                let quality = cfg.read().unwrap().quality.as_str().to_string();
                let backend = client.server_info().map(|i| i.backend).unwrap_or_default();
                let _ = ev.send(Event::Server { state: st.label().into(), detail, backend, quality });
                if rx.changed().await.is_err() {
                    break;
                }
            }
        })
    }

    fn deactivate(&self) {
        let mut a = self.active.lock().unwrap();
        if let Some(f) = a.forwarder.take() {
            f.abort();
        }
        if let Some(s) = a.supervisor.take() {
            tokio::spawn(async move { s.shutdown().await });
        }
        if let Some(r) = a.remote.take() {
            r.stop();
        }
    }

    fn attach_local(&self) {
        self.deactivate();
        let (sup, rx) = Supervisor::start(self.supervisor_config());
        let backend: Arc<dyn Backend> = sup.clone();
        self.client.switch(rx.clone(), Some(backend));
        let fwd = self.forward_state(rx.clone());
        *self.active.lock().unwrap() = Active { supervisor: Some(sup), remote: None, rx, forwarder: Some(fwd) };
    }

    fn attach_remote(&self, conn: RemoteConn) {
        self.deactivate();
        let (rb, rx) = RemoteBackend::start(conn);
        let backend: Arc<dyn Backend> = rb.clone();
        self.client.switch(rx.clone(), Some(backend));
        let fwd = self.forward_state(rx.clone());
        *self.active.lock().unwrap() = Active { supervisor: None, remote: Some(rb), rx, forwarder: Some(fwd) };
    }

    /// Redeems a pairing code and switches to the remote server.
    pub async fn pair_remote(&self, code: &str) -> Result<PairResult, String> {
        let name = medialake_proto::platform::host_name("MediaLake client");
        let paired = crate::remote::pair(code, &name).await?;
        let Some(server_model) = Model::parse(&paired.info.model_id) else {
            crate::remote::discard(&paired.creds.secret_protected);
            return Err(format!("the server runs a model this version does not know: {}", paired.info.model_id));
        };
        let server_quality = Quality::parse(&paired.info.quality);
        let mut reindex = false;
        {
            let mut c = self.cfg.write().unwrap();
            c.server_mode = ServerMode::Remote;
            c.set_model(server_model);
            if let Some(old) = c.remote.replace(paired.creds.clone()) {
                crate::remote::discard(&old.secret_protected);
            }
            if let Some(q) = server_quality {
                if c.quality != q {
                    c.quality = q;
                }
            }
        }
        self.save_config().map_err(|e| e.to_string())?;
        self.client.expect_model(server_model);
        let conn = RemoteConn::from_creds(&paired.creds)?;
        self.attach_remote(conn);
        if server_quality.is_some() {
            reindex = self.ensure_index_current().await;
        }
        Ok(PairResult {
            address: paired.creds.address,
            model: paired.info.model_id,
            quality: paired.info.quality,
            backend: paired.info.backend,
            reindex,
        })
    }

    /// Deletes the stored remote credential (Keychain item on macOS) and the paired server from the
    /// config. Used by the uninstall.
    pub fn forget_remote(&self) -> Result<(), String> {
        let old = {
            let mut c = self.cfg.write().unwrap();
            c.remote.take()
        };
        if let Some(old) = old {
            crate::remote::discard(&old.secret_protected);
        }
        self.save_config().map_err(|e| e.to_string())
    }

    /// Back to the bundled server.
    pub async fn use_local(&self) -> Result<(), String> {
        {
            let mut c = self.cfg.write().unwrap();
            c.server_mode = ServerMode::Local;
        }
        self.save_config().map_err(|e| e.to_string())?;
        self.attach_local();
        self.ensure_index_current().await;
        Ok(())
    }

    /// The index may only contain vectors of the active model and quality; rebuilds when it does not.
    async fn ensure_index_current(&self) -> bool {
        let cfg = self.config();
        let differs = {
            let st = self.store.lock().unwrap();
            st.meta("model_id").as_deref() != Some(cfg.model.as_str()) || st.meta("quality").as_deref() != Some(cfg.quality.as_str())
        };
        if differs {
            self.reindex_all().await;
        }
        differs
    }

    // ---------- accessors ----------

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub fn events(&self) -> broadcast::Sender<Event> {
        self.events.clone()
    }

    pub fn config(&self) -> Config {
        self.cfg.read().unwrap().clone()
    }

    fn save_config(&self) -> anyhow::Result<()> {
        let c = self.cfg.read().unwrap().clone();
        c.save(&self.paths.config_file)
    }

    pub fn mode(&self) -> ServerMode {
        self.config().server_mode
    }

    pub fn options(&self) -> &StartOptions {
        &self.opts
    }

    // ---------- search ----------

    pub async fn search(&self, query: &str) -> Result<Vec<SearchHit>, ClientError> {
        if !self.config().search_enabled {
            return Err(ClientError::Rejected("search is turned off".into()));
        }
        let query = query.trim();
        if query.is_empty() {
            return Ok(vec![]);
        }
        let cfg = self.config();
        let item = TextItem { title: String::new(), text: query.to_string() };
        let mut e = self.client.embed_text(Lane::Query, TextKind::Query, vec![item]).await?;
        let v = e.remove(0).into_result().map_err(ClientError::Rejected)?;
        let hits = self.vindex.search(&v, 200);
        let ids: Vec<i64> = hits.iter().map(|h| h.0).collect();
        let info = self.store.lock().unwrap().chunk_info(&ids).unwrap_or_default();
        let r = Ranking { floor: cfg.score_floor, ceiling: cfg.score_ceiling, threshold: cfg.threshold, relative_cut: cfg.relative_cut, max_results: cfg.max_results };
        Ok(search::rank(&hits, &info, &r))
    }

    // ---------- directories / settings ----------

    pub async fn add_dir(&self, path: PathBuf, recursive: bool) -> anyhow::Result<()> {
        {
            let mut c = self.cfg.write().unwrap();
            if let Some(d) = c.dirs.iter_mut().find(|d| d.path == path) {
                d.recursive = recursive;
            } else {
                c.dirs.push(IndexedDir { path, recursive });
            }
        }
        self.save_config()?;
        self.apply_dirs().await;
        Ok(())
    }

    pub async fn remove_dir(&self, path: &std::path::Path) -> anyhow::Result<()> {
        self.cfg.write().unwrap().dirs.retain(|d| d.path != path);
        self.save_config()?;
        self.apply_dirs().await;
        Ok(())
    }

    /// Search switched on or off while running: server, watcher and indexer follow. Off leaves the index
    /// on disk untouched.
    async fn apply_search_toggle(&self, on: bool) {
        if on {
            self.indexer.set_paused(false);
            if !self.opts.no_watch && self.watcher.lock().unwrap().is_none() {
                match FileWatcher::start(self.indexer.clone(), self.cfg.clone()) {
                    Ok(w) => {
                        w.set_dirs(&self.cfg.read().unwrap().dirs);
                        *self.watcher.lock().unwrap() = Some(w);
                        self.refresh_mount_watch();
                    }
                    Err(e) => {
                        let _ = self.events.send(Event::Error { message: format!("Cannot watch folders: {e}") });
                    }
                }
            }
            let c = self.config();
            let remote = if c.server_mode == ServerMode::Remote { c.remote.as_ref().map(RemoteConn::from_creds) } else { None };
            match remote {
                Some(Ok(conn)) => self.attach_remote(conn),
                Some(Err(e)) => {
                    let _ = self.events.send(Event::Error { message: format!("Remote server credentials unusable: {e}") });
                    self.attach_local();
                }
                None => self.attach_local(),
            }
            self.indexer.reconcile().await;
        } else {
            self.indexer.set_paused(true);
            self.indexer.save_if_dirty();
            *self.watcher.lock().unwrap() = None;
            self.deactivate();
            let (tx, rx) = watch::channel(ServerState::Idle);
            self.client.switch(rx.clone(), None);
            let mut a = self.active.lock().unwrap();
            a.rx = rx;
            drop(tx);
        }
    }

    /// Re-syncs the watcher and the store with the configured directories.
    async fn apply_dirs(&self) {
        if let Some(w) = self.watcher.lock().unwrap().as_ref() {
            w.set_dirs(&self.cfg.read().unwrap().dirs);
        }
        self.indexer.reconcile().await;
    }

    /// Applies a new configuration. Returns true when a full reindex was triggered.
    pub async fn set_config(&self, new: Config) -> anyhow::Result<bool> {
        let old = self.config();
        // mode and credentials are only changed by pair_remote / use_local
        let mut new = new;
        new.server_mode = old.server_mode;
        new.remote = old.remote.clone();
        // hosts, joined and hosted syncs are changed by their own calls (they hold secrets and state)
        new.sync.hosts = old.sync.hosts.clone();
        new.sync.folders = old.sync.folders.clone();
        new.sync.hosted = old.sync.hosted.clone();
        // a remote server dictates the model; locally the score mapping follows the model
        let wanted = if new.server_mode == ServerMode::Local { new.model } else { old.model };
        new.model = old.model;
        new.set_model(wanted);
        *self.cfg.write().unwrap() = new.clone();
        self.save_config()?;
        self.apply_sync_settings(&old, &new).await;
        if old.search_enabled != new.search_enabled {
            self.apply_search_toggle(new.search_enabled).await;
            return Ok(false);
        }
        if !new.search_enabled {
            return Ok(false);
        }
        let mut reindex = false;
        if (old.model != new.model || old.quality != new.quality) && new.server_mode == ServerMode::Local {
            self.client.expect_model(new.model);
            let sup = self.active.lock().unwrap().supervisor.clone();
            if let Some(s) = sup {
                s.reconfigure(|c| {
                    c.model = new.model;
                    c.quality = new.quality;
                })
                .await;
            }
            self.reindex_all().await;
            reindex = true;
        }
        if old.idle_shutdown_minutes != new.idle_shutdown_minutes {
            let sup = self.active.lock().unwrap().supervisor.clone();
            if let Some(s) = sup {
                s.reconfigure(|c| c.idle_shutdown = Duration::from_secs(new.idle_shutdown_minutes as u64 * 60)).await;
            }
        }
        if old.video_enabled != new.video_enabled {
            self.apply_video_toggle().await;
        }
        if old.dirs != new.dirs {
            self.apply_dirs().await;
        } else if old.max_file_bytes != new.max_file_bytes && !reindex {
            // size limit moved: files may cross it in either direction
            self.requeue_all();
        }
        Ok(reindex)
    }

    async fn apply_video_toggle(&self) {
        // forget the content vectors; the re-queue decides again based on the new setting
        let ids = self.store.lock().unwrap().delete_content_chunks_of_modality("video").unwrap_or_default();
        for id in ids {
            self.vindex.remove(id);
        }
        let paths = self.store.lock().unwrap().paths_of_modality("video").unwrap_or_default();
        for p in paths {
            self.indexer.enqueue(p);
        }
    }

    fn requeue_all(&self) {
        let seen = self.store.lock().unwrap().all_seen().unwrap_or_default();
        for p in seen.into_keys() {
            self.indexer.enqueue(p);
        }
    }

    /// Drops every vector and indexes everything again.
    pub async fn reindex_all(&self) {
        {
            let cfg = self.config();
            let st = self.store.lock().unwrap();
            let _ = st.clear_all();
            let _ = st.set_meta("model_id", cfg.model.as_str());
            let _ = st.set_meta("quality", cfg.quality.as_str());
            let _ = st.set_meta("index_clean", "0");
            self.vindex.set_dims(cfg.model.dims());
            let _ = self.vindex.rebuild(&st);
        }
        self.indexer.reconcile().await;
    }

    // ---------- models ----------

    pub fn models_status(&self, model: Model) -> Vec<crate::models::QualityStatus> {
        crate::models::status(&self.model_dir, model)
    }

    pub fn model_dir(&self) -> &std::path::Path {
        &self.model_dir
    }

    pub fn cancel_download(&self) {
        self.download_cancel.store(true, Ordering::Relaxed);
    }

    /// Downloads the files of a model's quality tier (no-op when present). Wakes the server afterwards.
    pub async fn download_quality(&self, model: Model, q: Quality) -> Result<(), String> {
        if crate::models::is_downloaded(&self.model_dir, model, q) {
            return Ok(());
        }
        if self.downloading.swap(true, Ordering::SeqCst) {
            return Err("a download is already running".into());
        }
        self.download_cancel.store(false, Ordering::Relaxed);
        let dir = self.model_dir.clone();
        let cancel = self.download_cancel.clone();
        let ev = self.events.clone();
        let set = medialake_proto::models::model_set(model, q);
        let r = tokio::task::spawn_blocking(move || crate::models::download_blocking(&dir, set, cancel, ev))
            .await
            .map_err(|e| e.to_string())?;
        self.downloading.store(false, Ordering::SeqCst);
        match &r {
            Err(e) => {
                let _ = self.events.send(Event::Error { message: e.clone() });
            }
            Ok(()) => {
                let sup = self.active.lock().unwrap().supervisor.clone();
                if let Some(s) = sup {
                    s.ensure_running();
                }
            }
        }
        r
    }

    // ---------- sync ----------

    /// The sync manager, or why there is none.
    pub fn sync(&self) -> Result<Arc<SyncManager>, String> {
        self.sync.lock().unwrap().clone().ok_or_else(|| "sync is turned off (Settings > Sync)".to_string())
    }

    async fn start_sync(&self) -> Result<(), String> {
        if self.sync.lock().unwrap().is_some() {
            return Ok(());
        }
        let m = SyncManager::new(self.cfg.clone(), self.paths.config_file.clone(), self.paths.root.clone(), self.events.clone())
            .map_err(|e| format!("{e:#}"))?;
        *self.sync.lock().unwrap() = Some(m.clone());
        m.start().await;
        self.indexer.set_gate(Some(m.clone()));
        self.refresh_mount_watch();
        // folders of joined syncs are indexed like any other folder
        if self.config().search_enabled {
            let folders = self.config().sync.folders;
            for f in folders.into_iter().filter(|f| f.index_for_search) {
                let _ = self.add_dir(f.path, true).await;
            }
        }
        Ok(())
    }

    /// Starts or stops sync when `sync.enabled` changed, and tells a running manager about new settings.
    async fn apply_sync_settings(&self, old: &Config, new: &Config) {
        if old.sync.enabled != new.sync.enabled {
            if new.sync.enabled {
                if let Err(e) = self.start_sync().await {
                    let _ = self.events.send(Event::Error { message: format!("Sync could not start: {e}") });
                }
            } else {
                let s = self.sync.lock().unwrap().take();
                self.indexer.set_gate(None);
                if let Some(s) = s {
                    s.stop().await;
                }
                self.refresh_mount_watch();
            }
        } else if old.sync.upload_limit_kbps != new.sync.upload_limit_kbps || old.sync.download_limit_kbps != new.sync.download_limit_kbps {
            if let Ok(s) = self.sync() {
                s.apply_limits();
            }
        }
        if old.sync.enabled == new.sync.enabled
            && (old.sync.device_name != new.sync.device_name || old.sync.hosting != new.sync.hosting)
        {
            if let Ok(s) = self.sync() {
                s.reset_clients();
                if let Err(e) = s.ensure_hosting().await {
                    let _ = self.events.send(Event::Error { message: e });
                }
            }
        }
    }

    /// Pairs with a host for sync. A code that also grants embedding fills the embedding server
    /// credentials too (without switching to it).
    pub async fn sync_pair(&self, code: &str) -> Result<SyncPairResult, String> {
        let m = self.sync()?;
        let (result, host) = m.pair(code).await?;
        if result.embedding && self.cfg.read().unwrap().remote.is_none() {
            if let Ok(sec) = crate::remote::unprotect(&host.secret_protected) {
                if let Ok(prot) = crate::remote::protect(&sec) {
                    self.cfg.write().unwrap().remote = Some(crate::config::RemoteCreds {
                        address: host.address.clone(),
                        fingerprint: host.fingerprint.clone(),
                        client_id: host.client_id.clone(),
                        secret_protected: prot,
                    });
                    let _ = self.save_config();
                }
            }
        }
        Ok(result)
    }

    /// Tells the file watcher which cache trees stand behind mounted sync volumes (FSEvents does not
    /// report changes inside an NFS mount).
    fn refresh_mount_watch(&self) {
        #[cfg(target_os = "macos")]
        {
            let pairs = self.sync().map(|m| m.mount_trees()).unwrap_or_default();
            if let Some(w) = self.watcher.lock().unwrap().as_ref() {
                w.set_mounts(&pairs);
            }
        }
    }

    pub async fn sync_join(&self, host_id: &str, sync_id: &str, path: &std::path::Path, frontend: &str, conflict: &str) -> Result<crate::config::SyncFolderCfg, String> {
        let m = self.sync()?;
        let f = m.join(host_id, sync_id, path, frontend, conflict).await?;
        self.refresh_mount_watch();
        if f.index_for_search && self.config().search_enabled {
            let _ = self.add_dir(f.path.clone(), true).await;
        }
        Ok(f)
    }

    pub async fn sync_leave(&self, sync_id: &str, keep_files: bool) -> Result<(), String> {
        let m = self.sync()?;
        let path = self.config().sync.folders.iter().find(|f| f.sync_id == sync_id || f.name == sync_id).map(|f| f.path.clone());
        m.leave(sync_id, keep_files).await?;
        self.refresh_mount_watch();
        if let Some(p) = path {
            if self.config().dirs.iter().any(|d| d.path == p) {
                let _ = self.remove_dir(&p).await;
            }
        }
        Ok(())
    }

    // ---------- status / lifecycle ----------

    pub fn status(&self) -> Status {
        let (files, chunks, by_status) = {
            let st = self.store.lock().unwrap();
            let (f, c) = st.counts().unwrap_or((0, 0));
            (f, c, st.count_by_status().unwrap_or_default())
        };
        let state = self.client.state();
        let info = self.client.server_info();
        Status {
            files,
            chunks,
            vectors: self.vindex.len(),
            queued: self.indexer.queue_len(),
            busy: self.indexer.is_busy(),
            server_state: state.label().into(),
            server_detail: match &state {
                ServerState::Failed(w) => w.clone(),
                _ => String::new(),
            },
            backend: info.as_ref().map(|i| i.backend.clone()).unwrap_or_default(),
            model: self.config().model.as_str().into(),
            quality: self.config().quality.as_str().into(),
            mode: match self.mode() {
                ServerMode::Local => "local".into(),
                ServerMode::Remote => "remote".into(),
            },
            by_status,
        }
    }

    pub async fn shutdown(&self) {
        let sync = self.sync.lock().unwrap().take();
        if let Some(s) = sync {
            s.stop().await;
        }
        self.indexer.set_paused(true);
        self.indexer.save_if_dirty();
        let sup = self.active.lock().unwrap().supervisor.clone();
        if let Some(s) = sup {
            s.shutdown().await;
        }
        let remote = self.active.lock().unwrap().remote.clone();
        if let Some(r) = remote {
            r.stop();
        }
    }

    /// Waits until the indexing queue is empty (CLI / tests).
    pub async fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        // give reconcile a moment to enqueue
        tokio::time::sleep(Duration::from_millis(500)).await;
        let mut quiet = 0;
        while tokio::time::Instant::now() < deadline {
            if self.indexer.is_busy() {
                quiet = 0;
            } else {
                quiet += 1;
                if quiet >= 4 {
                    self.indexer.save_if_dirty();
                    return true;
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        false
    }

    #[allow(dead_code)]
    fn active_rx(&self) -> watch::Receiver<ServerState> {
        self.active.lock().unwrap().rx.clone()
    }
}
