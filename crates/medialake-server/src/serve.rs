//! Remote mode: `serve` (TLS), `pair`, `clients`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, bail};
use bierpc::rpc::{RpcServer, ServerConfig};
use medialake_proto::MAX_MESSAGE_BYTES;
use medialake_proto::pairing::PairingCode;
use medialake_proto::pinning;

use crate::service::{Auth, Service};
use crate::state::{StateDir, default_state_dir};
use crate::{Common, server_info, start_engine};

#[derive(clap::Args, Clone)]
pub struct StateArgs {
    /// Where the certificate, paired clients and pending pairings live.
    #[arg(long, env = "MEDIALAKE_SERVER_STATE")]
    pub state_dir: Option<PathBuf>,
}

impl StateArgs {
    fn open(&self) -> anyhow::Result<StateDir> {
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
    #[command(flatten)]
    pub state: StateArgs,
}

#[derive(clap::Args, Clone)]
pub struct PairArgs {
    /// `host:port` that clients should use. Default: the address of the last `serve`.
    #[arg(long)]
    pub public_addr: Option<String>,
    #[command(flatten)]
    pub state: StateArgs,
}

fn default_public(bind: SocketAddr) -> String {
    let host = medialake_proto::platform::host_name("localhost");
    format!("{host}:{}", bind.port())
}

pub fn print_code(code: &PairingCode) {
    println!("Pairing code (valid for 10 minutes, usable once):\n\n  {}\n", code.encode());
    println!("Paste it into MediaLake: Settings > Model > On a remote server.");
}

pub async fn serve(common: &Common, a: ServeArgs) -> anyhow::Result<()> {
    let state = Arc::new(a.state.open()?);
    let ident = state.identity()?;
    let public = a.public_addr.clone().unwrap_or_else(|| default_public(a.bind));
    state.set_public_addr(&public);

    let (engine, quality) = start_engine(common)?;
    let tls = pinning::server_tls(ident.cert_der.clone(), ident.key_der.clone()).map_err(anyhow::Error::msg)?;
    let hook_state = state.clone();
    let svc = Service {
        engine: engine.clone(),
        auth: Arc::new(Auth { local_token: None, remote: Some(state.clone()) }),
        info: server_info(&engine, quality),
        shutdown: Arc::new(tokio::sync::Notify::new()),
        pairing: Some(Arc::new(move |token: &str, name: &str| hook_state.consume_pairing(token, name).map(|id| (id, String::new())))),
    };
    let server = RpcServer::new(a.bind, svc)
        .await
        .map_err(|e| anyhow::anyhow!("bind {}: {e:?}", a.bind))?
        .with_config(ServerConfig { max_message_bytes: MAX_MESSAGE_BYTES, tls: Some(tls), ..Default::default() });

    println!("MediaLake server listening on {} (public address {public})", a.bind);
    println!("Certificate fingerprint: {}", medialake_proto::pairing::hex(&ident.fingerprint()));
    if state.list_clients().is_empty() {
        println!("No paired clients yet.\n");
        print_code(&state.new_pairing(&public)?);
    } else {
        println!("{} paired client(s). Run `medialake-server pair` to add another.", state.list_clients().len());
    }
    tokio::select! {
        _ = server.run() => {}
        _ = tokio::signal::ctrl_c() => { tracing::info!("ctrl-c, shutting down"); }
    }
    engine.close();
    Ok(())
}

pub fn pair(a: PairArgs) -> anyhow::Result<()> {
    let state = a.state.open()?;
    let addr = a.public_addr.clone().or_else(|| state.public_addr());
    let Some(addr) = addr else {
        bail!("no address known yet: pass --public-addr host:port, or start `medialake-server serve` once");
    };
    crate::state::parse_hostport(&addr).context("--public-addr")?;
    print_code(&state.new_pairing(&addr)?);
    Ok(())
}

pub fn clients_list(s: StateArgs) -> anyhow::Result<()> {
    let state = s.open()?;
    let clients = state.list_clients();
    if clients.is_empty() {
        println!("no paired clients");
    }
    for (id, c) in clients {
        println!("{id}  {}  (paired {})", c.name, c.created);
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
