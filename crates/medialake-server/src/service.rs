//! bierpc handler: validates auth, applies prompts, and routes work to the engine.

use std::sync::Arc;

use bierpc::error::RpcResult;
use bierpc::rpc::RpcServerHandler;
use medialake_proto::models::Model;
use medialake_proto::{Action, Embedding, PROTO_VERSION, Response, ServerInfo, TextKind, format_text};

use medialake_sync::host::SyncHost;
use medialake_sync::host::state::StateDir;

use crate::engine::{Engine, Priority, Work};

/// Who may talk to this server.
#[derive(Default)]
pub struct Auth {
    /// Local child mode: the per-launch token handed over on stdin.
    pub local_token: Option<String>,
    /// Remote mode: paired clients (`client_id:secret_hex`).
    pub remote: Option<Arc<StateDir>>,
}

impl Auth {
    pub fn check(&self, presented: &str) -> bool {
        if let Some(t) = &self.local_token {
            return ct_eq(t.as_bytes(), presented.as_bytes());
        }
        self.remote.as_ref().is_some_and(|s| s.verify(presented))
    }
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
pub type PairingHook = Arc<dyn Fn(&str, &str) -> Option<(String, String)> + Send + Sync>;

#[derive(Clone)]
pub struct Service {
    /// `None` on a sync-only host (`serve --no-embed`).
    pub engine: Option<Engine>,
    /// Decides the prompt format of text inputs.
    pub model: Model,
    pub auth: Arc<Auth>,
    pub info: Option<ServerInfo>,
    /// File sync (remote mode only).
    pub sync: Option<Arc<SyncHost>>,
    /// Notified when a client asks to `Shutdown` (local mode).
    pub shutdown: Arc<tokio::sync::Notify>,
    /// Remote mode: pairing hook (`token, client_name -> (client_id, secret)`).
    pub pairing: Option<PairingHook>,
}

impl RpcServerHandler for Service {
    type Action = Action;
    type Response = Response;

    async fn handle(&self, action: Action) -> RpcResult<Response> {
        let deny = || Response::Rejected("authentication failed".into());
        let no_embed = || Response::Rejected("embedding is not enabled on this server".into());
        Ok(match action {
            Action::Hello { proto_version, auth } => {
                if !self.auth.check(&auth) {
                    deny()
                } else if proto_version != PROTO_VERSION {
                    Response::Rejected(format!("protocol version {proto_version} not supported (server speaks {PROTO_VERSION})"))
                } else if let Some(info) = &self.info {
                    Response::Info(info.clone())
                } else {
                    no_embed()
                }
            }
            Action::EmbedText { auth, kind, items } => {
                if !self.auth.check(&auth) {
                    return Ok(deny());
                }
                let Some(engine) = &self.engine else { return Ok(no_embed()) };
                let prio = if kind == TextKind::Query { Priority::Query } else { Priority::Bulk };
                let work = items.iter().map(|i| Work::Text(format_text(self.model, kind, i))).collect();
                Response::Embeddings(engine.embed(prio, work).await)
            }
            Action::EmbedImages { auth, images } => {
                if !self.auth.check(&auth) {
                    return Ok(deny());
                }
                let Some(engine) = &self.engine else { return Ok(no_embed()) };
                let work = images.into_iter().map(|i| Work::Images(vec![i])).collect();
                Response::Embeddings(engine.embed(Priority::Bulk, work).await)
            }
            Action::EmbedAudio { auth, segments } => {
                if !self.auth.check(&auth) {
                    return Ok(deny());
                }
                let (Some(engine), Some(info)) = (&self.engine, &self.info) else { return Ok(no_embed()) };
                if !info.supports_audio {
                    let msg = "audio not supported by this model".to_string();
                    return Ok(Response::Embeddings(vec![Embedding::Failed(msg); segments.len()]));
                }
                let work = segments.into_iter().map(Work::Audio).collect();
                Response::Embeddings(engine.embed(Priority::Bulk, work).await)
            }
            Action::EmbedVideo { auth, clips } => {
                if !self.auth.check(&auth) {
                    return Ok(deny());
                }
                let Some(engine) = &self.engine else { return Ok(no_embed()) };
                let work = clips.into_iter().map(|c| Work::Images(c.frames)).collect();
                Response::Embeddings(engine.embed(Priority::Bulk, work).await)
            }
            Action::Pair { token, client_name } => match &self.pairing {
                Some(p) => match p(&token, &client_name) {
                    Some((client_id, secret)) => Response::Paired { client_id, secret },
                    None => Response::Rejected("invalid or expired pairing token".into()),
                },
                None => Response::Rejected("this server does not accept pairing".into()),
            },
            Action::Shutdown { auth } => {
                if !self.auth.check(&auth) || self.auth.local_token.is_none() {
                    return Ok(deny());
                }
                self.shutdown.notify_one();
                Response::Ok
            }
            Action::Sync { auth, device, req } => match &self.sync {
                Some(host) => Response::Sync(host.handle(&auth, &device, req).await),
                None => Response::Rejected("this server does not host syncs".into()),
            },
        })
    }
}
