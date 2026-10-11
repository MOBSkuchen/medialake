//! Remote mode: `serve` (TLS), `pair`, `clients`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, bail};
use bierpc::rpc::{RpcServer, ServerConfig};
use medialake_proto::MAX_MESSAGE_BYTES;
use medialake_proto::models::Model;
use medialake_proto::pairing::PairingCode;
use medialake_proto::pinning;
use medialake_proto::sync::Access;
use medialake_sync::host::SyncHost;
use medialake_sync::host::config::SyncsFile;
use medialake_sync::host::state::{Grants, StateDir, default_state_dir, parse_hostport};

use crate::service::{Auth, Service};
use crate::{Common, model_files_present, server_info, start_engine};

#[derive(clap::Args, Clone)]
pub struct StateArgs {
    /// Where the certificate, paired clients and pending pairings live.
    #[arg(long, env = "MEDIALAKE_SERVER_STATE")]
    pub state_dir: Option<PathBuf>,
}

impl StateArgs {
    pub fn open(&self) -> anyhow::Result<StateDir> {
        StateDir::open(&self.state_dir.clone().unwrap_or_else(default_state_dir))
    }
}

#[derive(clap::Args, Clone)]
pub struct ServeArgs {
    /// Address to listen on.
    #[arg(long, default_value = "0.0.0.0:47900")]
    pub bind: SocketAddr,
    /// `host:port` that clients should use (goes into pairing codes). Default: this computer's name.
    #[arg(long)]
    pub public_addr: Option<String>,
    /// Do not load the embedding model: a pure sync host. Embedding requests are rejected.
    #[arg(long)]
    pub no_embed: bool,
    #[command(flatten)]
    pub state: StateArgs,
}

#[derive(clap::Args, Clone)]
pub struct PairArgs {
    /// `host:port` that clients should use. Default: the address of the last `serve`.
    #[arg(long)]
    pub public_addr: Option<String>,
    /// Grant access to a sync: `Name:rw` or `Name:ro`. Repeatable. Without `--embed`, a code with
    /// `--sync` is for sync only.
    #[arg(long = "sync", value_name = "NAME:rw|ro")]
    pub syncs: Vec<String>,
    /// Also allow embedding (implied when no `--sync` is given).
    #[arg(long)]
    pub embed: bool,
    #[command(flatten)]
    pub state: StateArgs,
}

fn default_public(bind: SocketAddr) -> String {
    let host = medialake_proto::platform::host_name("localhost");
    format!("{host}:{}", bind.port())
}

pub fn print_code(code: &PairingCode, grants: &Grants, names: &[(String, String)]) {
    println!("Pairing code (valid for 10 minutes, usable once):\n\n  {}\n", code.encode());
    let mut what = vec![];
    if grants.embed {
        what.push("embedding".to_string());
    }
    for (id, access) in &grants.syncs {
        let name = names.iter().find(|(i, _)| i == id).map(|(_, n)| n.as_str()).unwrap_or(id);
        what.push(format!("sync {name} ({access})"));
    }
    println!("Grants: {}", what.join(", "));
    if grants.embed {
        println!("Paste it into MediaLake: Settings > Model > On a remote server.");
    }
    if !grants.syncs.is_empty() {
        println!("Paste it into MediaLake: Settings > Sync > Add a host (or `medialake-sync pair <code>`).");
    }
}

pub async fn serve(common: &Common, a: ServeArgs) -> anyhow::Result<()> {
    let state = Arc::new(a.state.open()?);
    let ident = state.identity()?;
    let public = a.public_addr.clone().unwrap_or_else(|| default_public(a.bind));
    state.set_public_addr(&public);

    let configured = SyncsFile::load(state.dir()).syncs.len();
    let embed = if a.no_embed {
        false
    } else if model_files_present(common) {
        true
    } else if configured > 0 {
        tracing::warn!("no model files in {}: starting as a sync-only host", common.model_dir.display());
        false
    } else {
        true // start_engine reports which file is missing
    };
    let (engine, info, model) = if embed {
        let (engine, set) = start_engine(common)?;
        let info = server_info(&engine, &set);
        (Some(engine), Some(info), set.model_id)
    } else {
        (None, None, Model::parse(&common.model).unwrap_or_default())
    };

    let tls = pinning::server_tls(ident.cert_der.clone(), ident.key_der.clone()).map_err(anyhow::Error::msg)?;
    let hook_state = state.clone();
    let host_name = medialake_proto::platform::host_name("MediaLake");
    let sync_state = state.clone();
    let sync = tokio::task::spawn_blocking(move || SyncHost::new(sync_state, &host_name)).await.context("starting sync host")?;
    let background = sync.start_background();
    let svc = Service {
        engine: engine.clone(),
        model,
        auth: Arc::new(Auth { local_token: None, remote: Some(state.clone()) }),
        info,
        sync: Some(sync.clone()),
        shutdown: Arc::new(tokio::sync::Notify::new()),
        pairing: Some(Arc::new(move |token: &str, name: &str| hook_state.consume_pairing(token, name).map(|id| (id, String::new())))),
    };
    let server = RpcServer::new(a.bind, svc)
        .await
        .map_err(|e| anyhow::anyhow!("bind {}: {e:?}", a.bind))?
        .with_persistence(sync.persistent())
        .with_config(ServerConfig { max_message_bytes: MAX_MESSAGE_BYTES, tls: Some(tls), ..Default::default() });

    println!("MediaLake server listening on {} (public address {public})", a.bind);
    println!("Certificate fingerprint: {}", medialake_proto::pairing::hex(&ident.fingerprint()));
    println!("Embedding: {}", if engine.is_some() { "on" } else { "off" });
    for s in sync.all() {
        let c = s.config();
        println!("Sync {} ({}) at {} backup {}", c.name, c.id, c.root.display(), c.backup.describe());
    }
    if state.list_clients().is_empty() && engine.is_none() {
        println!("No paired clients yet. Invite a device with `medialake-server pair --sync <name>:rw`.");
    } else if state.list_clients().is_empty() {
        println!("No paired clients yet.\n");
        let grants = Grants::default();
        print_code(&state.new_pairing(&public, grants.clone())?, &grants, &[]);
    } else {
        println!("{} paired client(s). Run `medialake-server pair` to add another.", state.list_clients().len());
    }
    tokio::select! {
        _ = server.run() => {}
        _ = tokio::signal::ctrl_c() => { tracing::info!("ctrl-c, shutting down"); }
    }
    background.abort();
    if let Some(e) = engine {
        e.close();
    }
    Ok(())
}

pub fn pair(a: PairArgs) -> anyhow::Result<()> {
    let state = a.state.open()?;
    let addr = a.public_addr.clone().or_else(|| state.public_addr());
    let Some(addr) = addr else {
        bail!("no address known yet: pass --public-addr host:port, or start `medialake-server serve` once");
    };
    parse_hostport(&addr).context("--public-addr")?;

    let file = SyncsFile::load(state.dir());
    let mut grants = Grants { embed: a.syncs.is_empty() || a.embed, syncs: Default::default() };
    for spec in &a.syncs {
        let (name, access) = spec.rsplit_once(':').unwrap_or((spec.as_str(), "rw"));
        let Some(access) = Access::parse(access) else { bail!("{spec:?}: access must be rw or ro") };
        let Some(cfg) = file.find(name) else { bail!("no sync called {name:?} (see `medialake-server sync list`)") };
        grants.syncs.insert(cfg.id.clone(), access.as_str().to_string());
    }
    let names: Vec<(String, String)> = file.syncs.iter().map(|s| (s.id.clone(), s.name.clone())).collect();
    print_code(&state.new_pairing(&addr, grants.clone())?, &grants, &names);
    Ok(())
}

pub fn clients_list(s: StateArgs) -> anyhow::Result<()> {
    let state = s.open()?;
    let file = SyncsFile::load(state.dir());
    let clients = state.list_clients();
    if clients.is_empty() {
        println!("no paired clients");
    }
    for (id, c) in clients {
        let mut grants: Vec<String> = vec![];
        if c.embed {
            grants.push("embed".into());
        }
        for (sid, acc) in &c.syncs {
            let name = file.syncs.iter().find(|s| &s.id == sid).map(|s| s.name.as_str()).unwrap_or(sid);
            grants.push(format!("{name}:{acc}"));
        }
        println!("{id}  {}  (paired {})  [{}]", c.name, c.created, grants.join(", "));
    }
    Ok(())
}

pub fn clients_revoke(s: StateArgs, id: &str) -> anyhow::Result<()> {
    if s.open()?.revoke(id)? {
        println!("revoked {id}");
        Ok(())
    } else {
        bail!("no client with id {id}")
    }
}
