//! How the engine talks to the host. `Transport` is blocking and per sync; `RemoteTransport` speaks
//! bierpc over TLS to a real host, `LocalTransport` calls a [`HostedSync`] directly (tests, and the
//! convergence test of S3 needs thousands of operations without sockets).

use std::io::{Seek, SeekFrom, Write};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bierpc::rpc::{ClientConfig, ClientStream, RpcClient};
use bierpc::serialize::Deserialize;
use medialake_proto::pairing::{PairingCode, hex, unhex};
use medialake_proto::sync::*;
use medialake_proto::{Action, MAX_MESSAGE_BYTES, Response, pinning};
use serde::{Deserialize as SerdeDe, Serialize as SerdeSer};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufStream};
use tokio::runtime::Handle;

use crate::host::hosted::{ChangesResult, DownloadSource, HostedSync};

pub enum ChangesReply {
    Reset { epoch: String },
    Page { epoch: String, seq: u64, entries: Vec<Entry>, next_cursor: u64, more: bool },
}

#[derive(Debug, PartialEq, Eq)]
pub enum Fetch {
    Done,
    Stale,
    NotFound,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Upload {
    Done,
    HashMismatch,
}

pub trait Transport: Send + Sync {
    fn changes(&self, epoch: &str, cursor: u64, limit: u32) -> Result<ChangesReply>;
    fn propose(&self, changes: &[Change]) -> Result<Verdict>;
    fn commit(&self, changes: &[Change]) -> Result<Vec<ChangeResult>>;
    /// Appends the content of `path` at `rev` to `part`, resuming at its current length.
    fn download(&self, path: &str, rev: u64, part: &Path) -> Result<Fetch>;
    fn upload(&self, hash: &str, src: &Path, size: u64) -> Result<Upload>;
    fn versions(&self, path: &str) -> Result<Vec<VersionInfo>>;
    fn snapshots(&self) -> Result<Vec<SnapshotInfo>>;
    fn restore(&self, what: RestoreWhat, as_copy: bool) -> Result<Vec<ChangeResult>>;
}

// ---------------- in-process ----------------

pub struct LocalTransport {
    pub host: Arc<HostedSync>,
    pub access: Access,
    pub author: String,
}

impl Transport for LocalTransport {
    fn changes(&self, epoch: &str, cursor: u64, limit: u32) -> Result<ChangesReply> {
        Ok(match self.host.changes(epoch, cursor, limit)? {
            ChangesResult::Reset { epoch } => ChangesReply::Reset { epoch },
            ChangesResult::Page { epoch, seq, entries, next_cursor, more } => ChangesReply::Page { epoch, seq, entries, next_cursor, more },
        })
    }

    fn propose(&self, changes: &[Change]) -> Result<Verdict> {
        self.host.propose(self.access, &self.author, changes)
    }

    fn commit(&self, changes: &[Change]) -> Result<Vec<ChangeResult>> {
        self.host.commit(self.access, &self.author, changes)
    }

    fn download(&self, path: &str, rev: u64, part: &Path) -> Result<Fetch> {
        match self.host.download_source(path, rev) {
            DownloadSource::NotFound => Ok(Fetch::NotFound),
            DownloadSource::Stale => {
                self.host.scan_paths(&[path.to_string()])?;
                Ok(Fetch::Stale)
            }
            DownloadSource::File { path: src, .. } => {
                let have = std::fs::metadata(part).map(|m| m.len()).unwrap_or(0);
                let mut r = std::fs::File::open(src)?;
                r.seek(SeekFrom::Start(have))?;
                let mut w = std::fs::OpenOptions::new().create(true).append(true).open(part)?;
                std::io::copy(&mut r, &mut w)?;
                Ok(Fetch::Done)
            }
        }
    }

    fn upload(&self, hash: &str, src: &Path, size: u64) -> Result<Upload> {
        let have = self.host.upload_have(hash, size);
        if have >= size {
            return Ok(Upload::Done);
        }
        let part = self.host.upload_part_path(hash);
        let mut r = std::fs::File::open(src)?;
        r.seek(SeekFrom::Start(have))?;
        let mut w = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(&part)?;
        w.set_len(have)?;
        w.seek(SeekFrom::Start(have))?;
        std::io::copy(&mut r, &mut w)?;
        w.flush()?;
        drop(w);
        Ok(if self.host.upload_finish(hash)? { Upload::Done } else { Upload::HashMismatch })
    }

    fn versions(&self, path: &str) -> Result<Vec<VersionInfo>> {
        Ok(self.host.versions(path))
    }

    fn snapshots(&self) -> Result<Vec<SnapshotInfo>> {
        Ok(self.host.snapshots())
    }

    fn restore(&self, what: RestoreWhat, as_copy: bool) -> Result<Vec<ChangeResult>> {
        if self.access != Access::ReadWrite {
            bail!("read-only access");
        }
        self.host.restore(&self.author, &what, as_copy)
    }
}

// ---------------- over the network ----------------

/// What a paired host looks like on disk (the CLI keeps these in `hosts.json`; the app puts the secret
/// into the Keychain / DPAPI instead).
#[derive(SerdeSer, SerdeDe, Clone, Debug)]
pub struct HostCreds {
    pub id: String,
    pub name: String,
    pub address: String,
    /// SHA-256 of the host's certificate, hex.
    pub fingerprint: String,
    pub client_id: String,
    pub secret_hex: String,
}

impl HostCreds {
    pub fn auth(&self) -> String {
        format!("{}:{}", self.client_id, self.secret_hex)
    }

    fn fp(&self) -> Result<[u8; 32]> {
        let v = unhex(&self.fingerprint).filter(|v| v.len() == 32).ok_or_else(|| anyhow!("corrupt fingerprint"))?;
        let mut a = [0u8; 32];
        a.copy_from_slice(&v);
        Ok(a)
    }
}

pub async fn resolve(address: &str) -> Result<SocketAddr> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(address).await.with_context(|| format!("cannot resolve {address}"))?.collect();
    addrs.iter().find(|a| a.is_ipv4()).or(addrs.first()).copied().ok_or_else(|| anyhow!("no address for {address}"))
}

type Rpc = RpcClient<Action, StreamAction>;

async fn connect(addr: SocketAddr, fp: [u8; 32], transfer: bool) -> Result<Rpc> {
    let cfg = ClientConfig {
        max_message_bytes: MAX_MESSAGE_BYTES,
        tls: Some(pinning::client_tls(fp)),
        connect_timeout: Duration::from_secs(10),
        call_timeout: Some(Duration::from_secs(60)),
        // transfers and the notification stream see data at least every heartbeat
        read_timeout: if transfer { Some(Duration::from_secs(120)) } else { None },
        write_timeout: if transfer { Some(Duration::from_secs(120)) } else { None },
        ..Default::default()
    };
    Rpc::new_with_config(addr, cfg).await.map_err(|e| anyhow!("cannot connect to {addr}: {e:?}"))
}

/// What pairing found out.
pub struct Paired {
    pub creds: HostCreds,
    /// The host's embedding server info if this code also grants embedding.
    pub embedding: Option<medialake_proto::ServerInfo>,
}

/// Redeems a pairing code (pinned certificate, one-time token) and checks that the host speaks sync.
pub async fn pair(code_text: &str, device_name: &str) -> Result<HostCreds> {
    Ok(pair_ex(code_text, device_name).await?.creds)
}

/// Like [`pair`], and also probes whether the same credential may use the embedding server (a code
/// from `medialake-server pair --embed --sync ...` fills both).
pub async fn pair_ex(code_text: &str, device_name: &str) -> Result<Paired> {
    let code = PairingCode::decode(code_text).map_err(|e| anyhow!("{e}"))?;
    let addr = resolve(&code.address).await?;
    let mut c = connect(addr, code.fingerprint, false).await?;
    let r: Response = c
        .call(Action::Pair { token: hex(&code.token), client_name: device_name.to_string() })
        .await
        .map_err(|e| anyhow!("pairing failed: {e:?}"))?;
    let client_id = match r {
        Response::Paired { client_id, .. } => client_id,
        Response::Rejected(m) => bail!("the host refused pairing: {m}"),
        other => bail!("unexpected answer to pairing: {other:?}"),
    };
    let mut creds = HostCreds {
        id: String::new(),
        name: String::new(),
        address: code.address.clone(),
        fingerprint: hex(&code.fingerprint),
        client_id,
        secret_hex: hex(&code.secret),
    };
    let embedding = match c.call::<Response>(Action::Hello { proto_version: medialake_proto::PROTO_VERSION, auth: creds.auth() }).await {
        Ok(Response::Info(i)) => Some(i),
        _ => None,
    };
    let client = HostClient::new(Handle::current(), creds.clone(), device_name);
    let (id, name) = client.hello().await?;
    creds.id = id;
    creds.name = name;
    Ok(Paired { creds, embedding })
}

pub struct HostClient {
    handle: Handle,
    creds: HostCreds,
    device: String,
    control: tokio::sync::Mutex<Option<Rpc>>,
}

impl HostClient {
    pub fn new(handle: Handle, creds: HostCreds, device: &str) -> Arc<HostClient> {
        Arc::new(HostClient { handle, creds, device: device.to_string(), control: tokio::sync::Mutex::new(None) })
    }

    pub fn creds(&self) -> &HostCreds {
        &self.creds
    }

    async fn call_once(&self, req: SyncRequest) -> Result<SyncResponse> {
        let mut g = self.control.lock().await;
        if g.is_none() {
            let addr = resolve(&self.creds.address).await?;
            *g = Some(connect(addr, self.creds.fp()?, false).await?);
        }
        let c = g.as_mut().unwrap();
        let r: Result<Response, _> = c.call(Action::Sync { auth: self.creds.auth(), device: self.device.clone(), req }).await;
        match r {
            Ok(Response::Sync(s)) => Ok(s),
            Ok(Response::Rejected(m)) => bail!("the host refused the request: {m}"),
            Ok(other) => bail!("unexpected answer from the host: {other:?}"),
            Err(e) => {
                *g = None;
                bail!("{e:?}")
            }
        }
    }

    /// One call, retried once on a fresh connection (the old one may have idled out).
    pub async fn call(&self, mk: impl Fn() -> SyncRequest) -> Result<SyncResponse> {
        let r = match self.call_once(mk()).await {
            Ok(r) => r,
            Err(_) => self.call_once(mk()).await.map_err(|e| anyhow!("{e:#} (is this host a MediaLake host with sync enabled?)"))?,
        };
        match r {
            SyncResponse::Denied(m) => bail!("access denied: {m}"),
            SyncResponse::Error(m) => bail!("{m}"),
            r => Ok(r),
        }
    }

    pub async fn hello(&self) -> Result<(String, String)> {
        match self.call(|| SyncRequest::Hello { version: SYNC_PROTO_VERSION }).await? {
            SyncResponse::Host { host_id, name, .. } => Ok((host_id, name)),
            other => bail!("unexpected answer to hello: {other:?}"),
        }
    }

    pub async fn list_syncs(&self) -> Result<Vec<SyncInfo>> {
        match self.call(|| SyncRequest::ListSyncs).await? {
            SyncResponse::Syncs(v) => Ok(v),
            other => bail!("unexpected answer: {other:?}"),
        }
    }

    pub fn transport(self: &Arc<Self>, sync: &str) -> RemoteTransport {
        RemoteTransport { client: self.clone(), sync: sync.to_string() }
    }

    /// Keeps a notification stream open (reconnecting with backoff) and sends the id of every sync
    /// that advanced. Also sends `""` after each (re)connect: look at everything.
    pub fn subscribe(self: &Arc<Self>, items: Vec<SubscribeItem>, tx: std::sync::mpsc::Sender<String>) -> Subscription {
        let me = self.clone();
        let task = self.handle.spawn(async move {
            let mut backoff = Duration::from_secs(1);
            loop {
                let res: Result<()> = async {
                    let addr = resolve(&me.creds.address).await?;
                    let mut c = connect(addr, me.creds.fp()?, true).await?;
                    let _ = tx.send(String::new());
                    backoff = Duration::from_secs(1);
                    let tx2 = tx.clone();
                    let r: Result<StreamResponse, _> = c
                        .call_persistent(
                            StreamAction::Subscribe { auth: me.creds.auth(), syncs: items.clone() },
                            async |s: &mut BufStream<ClientStream>| {
                                loop {
                                    let n = Notice::deserialize(&mut *s).await?;
                                    if !n.sync.is_empty() && tx2.send(n.sync).is_err() {
                                        return Ok(());
                                    }
                                }
                            },
                        )
                        .await;
                    match r {
                        Ok(StreamResponse::Denied(m)) => bail!("access denied: {m}"),
                        Ok(_) => Ok(()),
                        Err(e) => bail!("{e:?}"),
                    }
                }
                .await;
                if let Err(e) = res {
                    tracing::debug!("notification stream: {e:#}");
                }
                if tx.send(String::new()).is_err() {
                    return;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        });
        Subscription { task }
    }
}

pub struct Subscription {
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub struct RemoteTransport {
    client: Arc<HostClient>,
    sync: String,
}

impl RemoteTransport {
    fn block<T>(&self, f: impl std::future::Future<Output = Result<T>>) -> Result<T> {
        self.client.handle.block_on(f)
    }
}

impl Transport for RemoteTransport {
    fn changes(&self, epoch: &str, cursor: u64, limit: u32) -> Result<ChangesReply> {
        let r = self.block(self.client.call(|| SyncRequest::Changes {
            sync: self.sync.clone(),
            epoch: epoch.to_string(),
            cursor,
            limit,
        }))?;
        match r {
            SyncResponse::Changes { epoch, seq, entries, next_cursor, more } => Ok(ChangesReply::Page { epoch, seq, entries, next_cursor, more }),
            SyncResponse::ResetRequired { epoch } => Ok(ChangesReply::Reset { epoch }),
            other => bail!("unexpected answer to changes: {other:?}"),
        }
    }

    fn propose(&self, changes: &[Change]) -> Result<Verdict> {
        match self.block(self.client.call(|| SyncRequest::Propose { sync: self.sync.clone(), changes: changes.to_vec() }))? {
            SyncResponse::Verdict(v) => Ok(v),
            other => bail!("unexpected answer to propose: {other:?}"),
        }
    }

    fn commit(&self, changes: &[Change]) -> Result<Vec<ChangeResult>> {
        match self.block(self.client.call(|| SyncRequest::Commit { sync: self.sync.clone(), changes: changes.to_vec() }))? {
            SyncResponse::Committed { results } => Ok(results),
            other => bail!("unexpected answer to commit: {other:?}"),
        }
    }

    fn download(&self, path: &str, rev: u64, part: &Path) -> Result<Fetch> {
        let c = &self.client;
        self.block(async {
            let have = tokio::fs::metadata(part).await.map(|m| m.len()).unwrap_or(0);
            let addr = resolve(&c.creds.address).await?;
            let mut rpc = connect(addr, c.creds.fp()?, true).await?;
            let mut file = tokio::fs::OpenOptions::new().create(true).append(true).open(part).await?;
            let mut status = 255u8;
            let resp: StreamResponse = rpc
                .call_persistent(
                    StreamAction::Download { auth: c.creds.auth(), sync: self.sync.clone(), path: path.to_string(), rev, offset: have, len: 0 },
                    async |s: &mut BufStream<ClientStream>| {
                        let mut st = [0u8; 1];
                        s.read_exact(&mut st).await?;
                        status = st[0];
                        if status != DL_OK {
                            return Ok(());
                        }
                        let _total = read_u64(s).await?;
                        let mut buf = Vec::new();
                        loop {
                            let n = read_frame(s, &mut buf).await?;
                            if n == 0 {
                                break;
                            }
                            file.write_all(&buf).await?;
                        }
                        Ok(())
                    },
                )
                .await
                .map_err(|e| anyhow!("download failed: {e:?}"))?;
            file.flush().await?;
            match (resp, status) {
                (StreamResponse::Done, DL_OK) => Ok(Fetch::Done),
                (StreamResponse::Stale, _) | (_, DL_STALE) => Ok(Fetch::Stale),
                (StreamResponse::NotFound, _) | (_, DL_NOT_FOUND) => Ok(Fetch::NotFound),
                (StreamResponse::Denied(m), _) => bail!("access denied: {m}"),
                (other, _) => bail!("download failed: {other:?}"),
            }
        })
    }

    fn upload(&self, hash: &str, src: &Path, size: u64) -> Result<Upload> {
        let c = &self.client;
        self.block(async {
            let addr = resolve(&c.creds.address).await?;
            let mut rpc = connect(addr, c.creds.fp()?, true).await?;
            let mut file = tokio::fs::File::open(src).await?;
            let resp: StreamResponse = rpc
                .call_persistent(
                    StreamAction::Upload { auth: c.creds.auth(), sync: self.sync.clone(), hash: hash.to_string(), size },
                    async |s: &mut BufStream<ClientStream>| {
                        let have = read_u64(s).await?;
                        file.seek(std::io::SeekFrom::Start(have)).await?;
                        let mut buf = vec![0u8; 1 << 20];
                        loop {
                            let n = file.read(&mut buf).await?;
                            if n == 0 {
                                break;
                            }
                            write_frame(s, &buf[..n]).await?;
                        }
                        write_frame(s, &[]).await?;
                        s.flush().await?;
                        Ok(())
                    },
                )
                .await
                .map_err(|e| anyhow!("upload failed: {e:?}"))?;
            match resp {
                StreamResponse::Done => Ok(Upload::Done),
                StreamResponse::HashMismatch => Ok(Upload::HashMismatch),
                StreamResponse::Denied(m) => bail!("access denied: {m}"),
                other => bail!("upload failed: {other:?}"),
            }
        })
    }

    fn versions(&self, path: &str) -> Result<Vec<VersionInfo>> {
        match self.block(self.client.call(|| SyncRequest::Versions { sync: self.sync.clone(), path: path.to_string() }))? {
            SyncResponse::Versions(v) => Ok(v),
            other => bail!("unexpected answer: {other:?}"),
        }
    }

    fn snapshots(&self) -> Result<Vec<SnapshotInfo>> {
        match self.block(self.client.call(|| SyncRequest::Snapshots { sync: self.sync.clone() }))? {
            SyncResponse::Snapshots(v) => Ok(v),
            other => bail!("unexpected answer: {other:?}"),
        }
    }

    fn restore(&self, what: RestoreWhat, as_copy: bool) -> Result<Vec<ChangeResult>> {
        match self.block(self.client.call(|| SyncRequest::Restore { sync: self.sync.clone(), what: what.clone(), as_copy }))? {
            SyncResponse::Committed { results } => Ok(results),
            other => bail!("unexpected answer: {other:?}"),
        }
    }
}
