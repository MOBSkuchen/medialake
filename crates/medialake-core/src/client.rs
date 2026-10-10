//! Embedding client: two bierpc connections (queries / bulk indexing) to the current server.

use std::sync::Arc;
use std::time::Duration;

use bierpc::rpc::{ClientConfig, RpcClient};
use medialake_proto::{Action, Embedding, MAX_MESSAGE_BYTES, PROTO_VERSION, Response, ServerInfo, TextItem, TextKind, VideoFrames};
use tokio::sync::{Mutex, watch};

use crate::supervisor::{Backend, ConnTarget, ServerState};

type Conn = RpcClient<Action>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lane {
    Query,
    Bulk,
}

struct Slot {
    conn: Option<Conn>,
    generation: u64,
}

pub struct EmbedClient {
    state: std::sync::Mutex<watch::Receiver<ServerState>>,
    backend: std::sync::Mutex<Option<Arc<dyn Backend>>>,
    query: Mutex<Slot>,
    bulk: Mutex<Slot>,
    info: std::sync::Mutex<Option<ServerInfo>>,
}

#[derive(Debug, Clone)]
pub enum ClientError {
    /// The server is not available (starting, failed, model missing, ...). Worth retrying later.
    Unavailable(String),
    /// The server answered with an error for this request.
    Rejected(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Unavailable(s) => write!(f, "server unavailable: {s}"),
            ClientError::Rejected(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl EmbedClient {
    pub fn new(state: watch::Receiver<ServerState>, backend: Option<Arc<dyn Backend>>) -> Arc<EmbedClient> {
        Arc::new(EmbedClient {
            state: std::sync::Mutex::new(state),
            backend: std::sync::Mutex::new(backend),
            query: Mutex::new(Slot { conn: None, generation: 0 }),
            bulk: Mutex::new(Slot { conn: None, generation: 0 }),
            info: std::sync::Mutex::new(None),
        })
    }

    pub fn server_info(&self) -> Option<ServerInfo> {
        self.info.lock().unwrap().clone()
    }

    pub fn state(&self) -> ServerState {
        self.state.lock().unwrap().borrow().clone()
    }

    /// Points the client at another server (local <-> remote). Existing connections are dropped lazily.
    pub fn switch(&self, state: watch::Receiver<ServerState>, backend: Option<Arc<dyn Backend>>) {
        *self.state.lock().unwrap() = state;
        *self.backend.lock().unwrap() = backend;
        *self.info.lock().unwrap() = None;
    }

    fn backend(&self) -> Option<Arc<dyn Backend>> {
        self.backend.lock().unwrap().clone()
    }

    async fn wait_ready(&self, timeout: Duration) -> Result<Arc<ConnTarget>, ClientError> {
        if let Some(s) = self.backend() {
            s.ensure_running();
        }
        let mut rx = self.state.lock().unwrap().clone();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let st = rx.borrow_and_update().clone();
            match st {
                ServerState::Ready(t) => return Ok(t),
                ServerState::Failed(why) => return Err(ClientError::Unavailable(why)),
                ServerState::Idle | ServerState::Starting => {}
            }
            match tokio::time::timeout_at(deadline, rx.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => return Err(ClientError::Unavailable("server supervisor stopped".into())),
                Err(_) => return Err(ClientError::Unavailable("timed out waiting for the server".into())),
            }
        }
    }

    async fn connect(target: &ConnTarget) -> Result<(Conn, ServerInfo), String> {
        let cfg = ClientConfig {
            max_message_bytes: MAX_MESSAGE_BYTES,
            tls: target.tls.clone(),
            call_timeout: Some(Duration::from_secs(600)),
            ..Default::default()
        };
        let mut c = Conn::new_with_config(target.addr, cfg).await.map_err(|e| format!("connect {}: {e:?}", target.addr))?;
        match c.call::<Response>(Action::Hello { proto_version: PROTO_VERSION, auth: target.auth.clone() }).await {
            Ok(Response::Info(i)) => Ok((c, i)),
            Ok(Response::Rejected(m)) => Err(format!("server rejected handshake: {m}")),
            Ok(other) => Err(format!("unexpected handshake response: {other:?}")),
            Err(e) => Err(format!("handshake failed: {e:?}")),
        }
    }

    /// Sends one action built from the current auth token; reconnects/restarts on transport errors.
    async fn call(&self, lane: Lane, build: impl Fn(String) -> Action) -> Result<Response, ClientError> {
        let mut last_err = String::new();
        for attempt in 0..3 {
            let target = self.wait_ready(Duration::from_secs(240)).await?;
            if let Some(s) = self.backend() {
                s.touch();
            }
            let slot = match lane {
                Lane::Query => &self.query,
                Lane::Bulk => &self.bulk,
            };
            let mut g = slot.lock().await;
            if g.conn.is_none() || g.generation != target.generation || g.conn.as_ref().is_some_and(|c| c.is_poisoned()) {
                match Self::connect(&target).await {
                    Ok((c, info)) => {
                        *self.info.lock().unwrap() = Some(info);
                        g.conn = Some(c);
                        g.generation = target.generation;
                    }
                    Err(e) => {
                        last_err = e;
                        g.conn = None;
                        drop(g);
                        if attempt >= 1 {
                            if let Some(s) = self.backend() {
                                s.report_failure(target.generation);
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(300 * (attempt + 1))).await;
                        continue;
                    }
                }
            }
            let conn = g.conn.as_mut().unwrap();
            match conn.call::<Response>(build(target.auth.clone())).await {
                Ok(r) => return Ok(r),
                Err(e) => {
                    last_err = format!("{e:?}");
                    g.conn = None;
                    drop(g);
                    if let Some(s) = self.backend() {
                        s.report_failure(target.generation);
                    }
                    tokio::time::sleep(Duration::from_millis(300 * (attempt + 1))).await;
                }
            }
        }
        Err(ClientError::Unavailable(last_err))
    }

    pub async fn hello(&self) -> Result<ServerInfo, ClientError> {
        let r = self.call(Lane::Query, |auth| Action::Hello { proto_version: PROTO_VERSION, auth }).await?;
        match r {
            Response::Info(i) => {
                *self.info.lock().unwrap() = Some(i.clone());
                Ok(i)
            }
            Response::Rejected(m) => Err(ClientError::Rejected(m)),
            other => Err(ClientError::Rejected(format!("unexpected response {other:?}"))),
        }
    }

    fn embeddings(r: Response, expected: usize) -> Result<Vec<Embedding>, ClientError> {
        match r {
            Response::Embeddings(v) if v.len() == expected => Ok(v),
            Response::Embeddings(v) => Err(ClientError::Rejected(format!("server returned {} embeddings for {expected} inputs", v.len()))),
            Response::Rejected(m) => Err(ClientError::Rejected(m)),
            other => Err(ClientError::Rejected(format!("unexpected response {other:?}"))),
        }
    }

    pub async fn embed_text(&self, lane: Lane, kind: TextKind, items: Vec<TextItem>) -> Result<Vec<Embedding>, ClientError> {
        let n = items.len();
        let r = self.call(lane, |auth| Action::EmbedText { auth, kind, items: items.clone() }).await?;
        Self::embeddings(r, n)
    }

    pub async fn embed_images(&self, images: Vec<Vec<u8>>) -> Result<Vec<Embedding>, ClientError> {
        let n = images.len();
        let r = self.call(Lane::Bulk, |auth| Action::EmbedImages { auth, images: images.clone() }).await?;
        Self::embeddings(r, n)
    }

    pub async fn embed_audio(&self, segments: Vec<Vec<f32>>) -> Result<Vec<Embedding>, ClientError> {
        let n = segments.len();
        let r = self.call(Lane::Bulk, |auth| Action::EmbedAudio { auth, segments: segments.clone() }).await?;
        Self::embeddings(r, n)
    }

    pub async fn embed_video(&self, clips: Vec<Vec<Vec<u8>>>) -> Result<Vec<Embedding>, ClientError> {
        let n = clips.len();
        let r = self
            .call(Lane::Bulk, |auth| Action::EmbedVideo {
                auth,
                clips: clips.iter().map(|f| VideoFrames { frames: f.clone() }).collect(),
            })
            .await?;
        Self::embeddings(r, n)
    }
}
