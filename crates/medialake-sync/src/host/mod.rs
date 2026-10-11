//! The host side of sync: hosted folders, shared state, and the bierpc service on top of them.

pub mod config;
pub mod hosted;
pub mod state;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

use bierpc::error::{RpcError, RpcResult};
use bierpc::rpc::{PersistentRpcServerHandler, RpcServerHandler};
use medialake_proto::{Action, Response};
use bierpc::serialize::Serialize;
use medialake_proto::sync::*;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::broadcast;

use self::config::{HostedConfig, SyncsFile};
use self::hosted::{ChangesResult, DownloadSource, HostedSync, Notifier};
use self::state::StateDir;
use crate::watch::PathWatcher;

struct Live {
    sync: Arc<HostedSync>,
    _watcher: Option<PathWatcher>,
}

/// All syncs a host serves, plus authentication and notification fan-out.
pub struct SyncHost {
    state: Arc<StateDir>,
    host_name: String,
    syncs: RwLock<HashMap<String, Live>>,
    cfg_mtime: Mutex<Option<SystemTime>>,
    notices: broadcast::Sender<Notice>,
}

fn sanitize_device(d: &str, fallback: &str) -> String {
    let s: String = d.chars().filter(|c| !c.is_control()).take(64).collect();
    if s.trim().is_empty() { fallback.to_string() } else { s }
}

impl SyncHost {
    /// Opens every configured sync (initial scan included) and starts watching their folders.
    pub fn new(state: Arc<StateDir>, host_name: &str) -> Arc<SyncHost> {
        let (notices, _) = broadcast::channel(256);
        let host = Arc::new(SyncHost {
            state,
            host_name: host_name.to_string(),
            syncs: RwLock::new(HashMap::new()),
            cfg_mtime: Mutex::new(None),
            notices,
        });
        host.reload(true);
        host
    }

    pub fn state(&self) -> &Arc<StateDir> {
        &self.state
    }

    pub fn is_empty(&self) -> bool {
        self.syncs.read().unwrap().is_empty()
    }

    pub fn sync(&self, id: &str) -> Option<Arc<HostedSync>> {
        self.syncs.read().unwrap().get(id).map(|l| l.sync.clone())
    }

    pub fn all(&self) -> Vec<Arc<HostedSync>> {
        self.syncs.read().unwrap().values().map(|l| l.sync.clone()).collect()
    }

    fn notifier(&self) -> Notifier {
        let tx = self.notices.clone();
        Arc::new(move |sync: &str, seq: u64| {
            let _ = tx.send(Notice { sync: sync.to_string(), seq });
        })
    }

    fn config_mtime(&self) -> Option<SystemTime> {
        std::fs::metadata(SyncsFile::path(self.state.dir())).and_then(|m| m.modified()).ok()
    }

    /// Re-reads `syncs.json` when it changed (or `force`). Opens new syncs, applies changed settings,
    /// drops removed ones.
    pub fn reload(&self, force: bool) {
        let mtime = self.config_mtime();
        {
            let mut g = self.cfg_mtime.lock().unwrap();
            if !force && *g == mtime {
                return;
            }
            *g = mtime;
        }
        let file = SyncsFile::load(self.state.dir());
        let wanted: HashMap<String, HostedConfig> = file.syncs.into_iter().map(|c| (c.id.clone(), c)).collect();
        self.syncs.write().unwrap().retain(|id, _| wanted.contains_key(id));
        for (id, cfg) in wanted {
            if let Some(l) = self.syncs.read().unwrap().get(&id) {
                if l.sync.config() != cfg {
                    l.sync.set_config(cfg);
                }
                continue;
            }
            match HostedSync::open(cfg.clone(), Some(self.notifier())) {
                Ok(hs) => {
                    if let Err(e) = hs.scan() {
                        tracing::warn!("sync {}: initial scan failed: {e:#}", cfg.name);
                    }
                    if let Err(e) = hs.preserve_current().and_then(|_| hs.maintenance()) {
                        tracing::warn!("sync {}: maintenance failed: {e:#}", cfg.name);
                    }
                    let watcher = start_watcher(&hs);
                    tracing::info!("hosting sync {} ({}) at {}", cfg.name, cfg.id, cfg.root.display());
                    self.syncs.write().unwrap().insert(id, Live { sync: hs, _watcher: watcher });
                }
                Err(e) => tracing::error!("cannot host sync {}: {e:#}", cfg.name),
            }
        }
    }

    /// Notification polling (another process may have committed), periodic maintenance and a safety
    /// rescan in case the watcher dropped events. Runs until the returned handle is aborted.
    pub fn start_background(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            let mut tick = 0u64;
            let mut iv = tokio::time::interval(Duration::from_secs(2));
            loop {
                iv.tick().await;
                tick += 1;
                let me2 = me.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    me2.reload(false);
                    for s in me2.all() {
                        s.poll_notify();
                        if tick % 30 == 0 {
                            if let Err(e) = s.maintenance() {
                                tracing::warn!("sync {}: maintenance: {e:#}", s.config().name);
                            }
                        }
                        if tick % 300 == 0 {
                            if let Err(e) = s.scan() {
                                tracing::warn!("sync {}: rescan: {e:#}", s.config().name);
                            }
                        }
                    }
                })
                .await;
            }
        })
    }

    pub fn persistent(self: &Arc<Self>) -> SyncStreams {
        SyncStreams { host: self.clone() }
    }

    // ---------------- single calls ----------------

    pub async fn handle(self: &Arc<Self>, auth: &str, device: &str, req: SyncRequest) -> SyncResponse {
        if self.config_mtime() != *self.cfg_mtime.lock().unwrap() {
            let me = self.clone();
            let _ = tokio::task::spawn_blocking(move || me.reload(false)).await;
        }
        let Some((client_id, rec)) = self.state.authenticate(auth) else {
            return SyncResponse::Denied("authentication failed".into());
        };
        let author = sanitize_device(device, &rec.name);
        let grant = |sync: &str| -> Result<(Arc<HostedSync>, Access), SyncResponse> {
            match (rec.access(sync), self.sync(sync)) {
                (Some(a), Some(s)) => Ok((s, a)),
                _ => Err(SyncResponse::Denied(format!("no access to sync {sync}"))),
            }
        };
        macro_rules! grant {
            ($id:expr) => {
                match grant($id) {
                    Ok(g) => g,
                    Err(r) => return r,
                }
            };
        }
        macro_rules! blocking {
            ($body:expr) => {
                match tokio::task::spawn_blocking($body).await {
                    Ok(Ok(r)) => r,
                    Ok(Err(e)) => return SyncResponse::Error(format!("{e:#}")),
                    Err(e) => return SyncResponse::Error(format!("internal error: {e}")),
                }
            };
        }
        match req {
            SyncRequest::Hello { version } => {
                if version != SYNC_PROTO_VERSION {
                    SyncResponse::Error(format!("sync protocol version {version} not supported (host speaks {SYNC_PROTO_VERSION})"))
                } else {
                    SyncResponse::Host { host_id: self.state.host_id(), name: self.host_name.clone(), version: SYNC_PROTO_VERSION }
                }
            }
            SyncRequest::ListSyncs => {
                let mut v = vec![];
                for s in self.all() {
                    if let Some(a) = rec.access(s.id()) {
                        let s2 = s.clone();
                        if let Ok(info) = tokio::task::spawn_blocking(move || s2.info(a)).await {
                            v.push(info);
                        }
                    }
                }
                v.sort_by(|a, b| a.name.cmp(&b.name));
                SyncResponse::Syncs(v)
            }
            SyncRequest::Changes { sync, epoch, cursor, limit } => {
                let (s, _) = grant!(&sync);
                match blocking!(move || s.changes(&epoch, cursor, limit)) {
                    ChangesResult::Reset { epoch } => SyncResponse::ResetRequired { epoch },
                    ChangesResult::Page { epoch, seq, entries, next_cursor, more } => SyncResponse::Changes { epoch, seq, entries, next_cursor, more },
                }
            }
            SyncRequest::Propose { sync, changes } => {
                let (s, a) = grant!(&sync);
                SyncResponse::Verdict(blocking!(move || s.propose(a, &author, &changes)))
            }
            SyncRequest::Commit { sync, changes } => {
                let (s, a) = grant!(&sync);
                tracing::debug!("commit from {client_id} ({author}): {} change(s)", changes.len());
                SyncResponse::Committed { results: blocking!(move || s.commit(a, &author, &changes)) }
            }
            SyncRequest::Versions { sync, path } => {
                let (s, _) = grant!(&sync);
                SyncResponse::Versions(blocking!(move || Ok::<_, anyhow::Error>(s.versions(&path))))
            }
            SyncRequest::Snapshots { sync } => {
                let (s, _) = grant!(&sync);
                SyncResponse::Snapshots(blocking!(move || Ok::<_, anyhow::Error>(s.snapshots())))
            }
            SyncRequest::Restore { sync, what, as_copy } => {
                let (s, a) = grant!(&sync);
                if a != Access::ReadWrite {
                    return SyncResponse::Denied("read-only access".into());
                }
                SyncResponse::Committed { results: blocking!(move || s.restore(&author, &what, as_copy)) }
            }
        }
    }
}

fn start_watcher(hs: &Arc<HostedSync>) -> Option<PathWatcher> {
    let root = hs.root().to_path_buf();
    let store = hs.store_dir().to_path_buf();
    let skip_hs = hs.clone();
    let hs2 = hs.clone();
    let store_rel = store.strip_prefix(&root).ok().and_then(|p| p.to_str().map(|s| s.replace('\\', "/")));
    let outside_store = !store.starts_with(&root);
    let _ = outside_store;
    PathWatcher::start(
        &root,
        Duration::from_millis(500),
        move |rel| {
            medialake_proto::sync::is_ignored(rel, &skip_hs.config().ignore)
                || store_rel.as_deref().is_some_and(|s| is_within(rel, s))
        },
        move |paths| {
            if let Err(e) = hs2.scan_paths(&paths) {
                tracing::warn!("sync {}: scan failed: {e:#}", hs2.config().name);
            }
        },
    )
    .map_err(|e| tracing::warn!("cannot watch {}: {e}", root.display()))
    .ok()
}

/// The single-call handler of a host that serves sync only (the desktop app hosting a folder; the
/// headless server has its own `Service` that adds embedding). Pairing goes through the shared state.
pub struct SyncOnlyService {
    pub host: Arc<SyncHost>,
}

impl RpcServerHandler for SyncOnlyService {
    type Action = Action;
    type Response = Response;

    async fn handle(&self, action: Action) -> RpcResult<Response> {
        Ok(match action {
            Action::Pair { token, client_name } => match self.host.state().consume_pairing(&token, &client_name) {
                // the secret is in the pairing code itself
                Some(id) => Response::Paired { client_id: id, secret: String::new() },
                None => Response::Rejected("invalid or expired pairing token".into()),
            },
            Action::Sync { auth, device, req } => Response::Sync(self.host.handle(&auth, &device, req).await),
            _ => Response::Rejected("embedding is not enabled on this host".into()),
        })
    }
}

// ---------------- persistent calls ----------------

pub struct SyncStreams {
    host: Arc<SyncHost>,
}

fn io_err(e: std::io::Error) -> RpcError {
    RpcError::IoError { err: e }
}

impl PersistentRpcServerHandler for SyncStreams {
    type Action = StreamAction;
    type Response = StreamResponse;

    async fn handle<S: AsyncRead + AsyncWrite + Unpin + Send>(&self, action: StreamAction, s: &mut S) -> RpcResult<StreamResponse> {
        let host = &self.host;
        match action {
            StreamAction::Download { auth, sync, path, rev, offset, len } => {
                let Some((_, rec)) = host.state.authenticate(&auth) else { return Ok(StreamResponse::Denied("authentication failed".into())) };
                let (Some(_), Some(hs)) = (rec.access(&sync), host.sync(&sync)) else {
                    return Ok(StreamResponse::Denied(format!("no access to sync {sync}")));
                };
                let p = path.clone();
                let hs2 = hs.clone();
                let src = tokio::task::spawn_blocking(move || hs2.download_source(&p, rev)).await.unwrap_or(DownloadSource::NotFound);
                match src {
                    DownloadSource::NotFound => {
                        s.write_all(&[DL_NOT_FOUND]).await.map_err(io_err)?;
                        s.flush().await.map_err(io_err)?;
                        Ok(StreamResponse::NotFound)
                    }
                    DownloadSource::Stale => {
                        let p = path.clone();
                        tokio::task::spawn_blocking(move || hs.scan_paths(&[p])).await.ok();
                        s.write_all(&[DL_STALE]).await.map_err(io_err)?;
                        s.flush().await.map_err(io_err)?;
                        Ok(StreamResponse::Stale)
                    }
                    DownloadSource::File { path: file, size } => {
                        s.write_all(&[DL_OK]).await.map_err(io_err)?;
                        write_u64(s, size).await.map_err(io_err)?;
                        let mut f = tokio::fs::File::open(&file).await.map_err(io_err)?;
                        f.seek(std::io::SeekFrom::Start(offset)).await.map_err(io_err)?;
                        let mut remaining = if len == 0 { size.saturating_sub(offset) } else { len.min(size.saturating_sub(offset)) };
                        let mut buf = vec![0u8; MAX_FRAME.min(1 << 20)];
                        while remaining > 0 {
                            let want = (buf.len() as u64).min(remaining) as usize;
                            let n = f.read(&mut buf[..want]).await.map_err(io_err)?;
                            if n == 0 {
                                break;
                            }
                            write_frame(s, &buf[..n]).await.map_err(io_err)?;
                            remaining -= n as u64;
                        }
                        write_frame(s, &[]).await.map_err(io_err)?;
                        s.flush().await.map_err(io_err)?;
                        Ok(StreamResponse::Done)
                    }
                }
            }
            StreamAction::Upload { auth, sync, hash, size } => {
                let Some((_, rec)) = host.state.authenticate(&auth) else { return Ok(StreamResponse::Denied("authentication failed".into())) };
                let (Some(Access::ReadWrite), Some(hs)) = (rec.access(&sync), host.sync(&sync)) else {
                    return Ok(StreamResponse::Denied("no write access to this sync".into()));
                };
                if !is_hash(&hash) {
                    return Ok(StreamResponse::Error("bad hash".into()));
                }
                let have = hs.upload_have(&hash, size);
                write_u64(s, have).await.map_err(io_err)?;
                s.flush().await.map_err(io_err)?;
                let complete = have >= size;
                let part = hs.upload_part_path(&hash);
                let mut file = if complete {
                    None
                } else {
                    let f = tokio::fs::OpenOptions::new().create(true).write(true).open(&part).await.map_err(io_err)?;
                    f.set_len(have).await.map_err(io_err)?;
                    let mut f = f;
                    f.seek(std::io::SeekFrom::Start(have)).await.map_err(io_err)?;
                    Some(f)
                };
                let mut total = have;
                let mut buf = Vec::new();
                loop {
                    let n = read_frame(s, &mut buf).await.map_err(io_err)?;
                    if n == 0 {
                        break;
                    }
                    total += n as u64;
                    if let Some(f) = file.as_mut() {
                        if total > size {
                            drop(file.take());
                            let _ = tokio::fs::remove_file(&part).await;
                            return Ok(StreamResponse::Error("more data than announced".into()));
                        }
                        f.write_all(&buf).await.map_err(io_err)?;
                    }
                }
                if complete {
                    return Ok(StreamResponse::Done);
                }
                if let Some(mut f) = file {
                    f.flush().await.map_err(io_err)?;
                }
                if total != size {
                    // keep the partial file: the next upload resumes from it
                    return Ok(StreamResponse::Error(format!("upload ended at {total} of {size} bytes")));
                }
                let h = hash.clone();
                let ok = tokio::task::spawn_blocking(move || hs.upload_finish(&h)).await.ok().and_then(|r| r.ok()).unwrap_or(false);
                Ok(if ok { StreamResponse::Done } else { StreamResponse::HashMismatch })
            }
            StreamAction::Subscribe { auth, syncs } => {
                let Some((cid, rec)) = host.state.authenticate(&auth) else { return Ok(StreamResponse::Denied("authentication failed".into())) };
                // subscribe before looking at the current numbers so that nothing falls in between
                let mut rx = host.notices.subscribe();
                for item in &syncs {
                    if let (Some(_), Some(hs)) = (rec.access(&item.sync), host.sync(&item.sync)) {
                        let seq = tokio::task::spawn_blocking(move || hs.seq()).await.unwrap_or(0);
                        if seq > item.cursor {
                            Notice { sync: item.sync.clone(), seq }.serialize(&mut *s).await.map_err(io_err)?;
                        }
                    }
                }
                s.flush().await.map_err(io_err)?;
                let mut hb = tokio::time::interval(Duration::from_secs(HEARTBEAT_SECS));
                hb.tick().await;
                loop {
                    tokio::select! {
                        n = rx.recv() => match n {
                            Ok(n) => {
                                if rec.access(&n.sync).is_some() && host.state.authenticate(&auth).is_some_and(|(i, _)| i == cid) {
                                    n.serialize(&mut *s).await.map_err(io_err)?;
                                    s.flush().await.map_err(io_err)?;
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => {
                                // tell the member to look at everything
                                for item in &syncs {
                                    if rec.access(&item.sync).is_some() {
                                        Notice { sync: item.sync.clone(), seq: u64::MAX }.serialize(&mut *s).await.map_err(io_err)?;
                                    }
                                }
                                s.flush().await.map_err(io_err)?;
                            }
                            Err(broadcast::error::RecvError::Closed) => return Ok(StreamResponse::Done),
                        },
                        _ = hb.tick() => {
                            // revocation closes the stream
                            if host.state.authenticate(&auth).is_none_or(|(i, _)| i != cid) {
                                return Ok(StreamResponse::Denied("access revoked".into()));
                            }
                            Notice { sync: String::new(), seq: 0 }.serialize(&mut *s).await.map_err(io_err)?;
                            s.flush().await.map_err(io_err)?;
                        }
                    }
                }
            }
        }
    }
}
