mod engine;
mod serve;
mod service;
mod state;

use std::io::BufRead;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, bail};
use bierpc::rpc::{RpcServer, ServerConfig};
use clap::{Parser, Subcommand};
use searchnow_proto::models::{MODEL_ID, Quality, model_set};
use searchnow_proto::{MAX_MESSAGE_BYTES, READY_PREFIX, ServerInfo};

use crate::engine::Engine;
use crate::service::{Auth, Service};
use crate::serve::{PairArgs, ServeArgs, StateArgs};

#[derive(Parser)]
#[command(name = "searchnow-server", version, about = "SearchNow embedding server (EmbeddingGemma 2 via llama.cpp)")]
struct Cli {
    /// Child mode: bind 127.0.0.1, read the auth token from stdin, print `READY <port>` when loaded.
    #[arg(long)]
    local: bool,
    /// Port for --local (0 = let the OS choose).
    #[arg(long, default_value_t = 0)]
    port: u16,
    #[command(flatten)]
    common: Common,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(clap::Args, Clone)]
pub(crate) struct Common {
    /// Directory containing the GGUF files.
    #[arg(long, env = "SEARCHNOW_MODEL_DIR", default_value = ".")]
    model_dir: PathBuf,
    #[arg(long, default_value = "normal")]
    quality: String,
    /// Layers to offload (0 = CPU). Default: all when built with Vulkan, else 0.
    #[arg(long)]
    gpu_layers: Option<i32>,
    /// Write a rolling log file into this directory (stderr is always used).
    #[arg(long)]
    log_dir: Option<PathBuf>,
    #[arg(long)]
    verbose_llama: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Remote mode: listen with TLS for paired clients.
    Serve(ServeArgs),
    /// Print a new one-time pairing code.
    Pair(PairArgs),
    /// Manage paired clients.
    Clients {
        #[command(subcommand)]
        cmd: ClientsCmd,
    },
    /// Embed a fixed set of items and print timings.
    Bench {
        /// Directory with sample media (cat_small.jpg, car_small.jpg, dog.wav, rain.wav).
        #[arg(long)]
        media_dir: PathBuf,
        #[arg(long, default_value_t = 3)]
        repeat: usize,
    },
}

#[derive(Subcommand)]
enum ClientsCmd {
    List(StateArgs),
    Revoke {
        id: String,
        #[command(flatten)]
        state: StateArgs,
    },
}

type Guard = tracing_appender::non_blocking::WorkerGuard;

fn init_logging(dir: &Option<PathBuf>, file: &str) -> Option<Guard> {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let stderr = fmt::layer().with_writer(std::io::stderr).with_ansi(false);
    match dir {
        Some(d) => {
            let app = tracing_appender::rolling::daily(d, file);
            let (nb, guard) = tracing_appender::non_blocking(app);
            let file_layer = fmt::layer().with_writer(nb).with_ansi(false);
            tracing_subscriber::registry().with(filter).with(stderr).with(file_layer).init();
            Some(guard)
        }
        None => {
            tracing_subscriber::registry().with(filter).with(stderr).init();
            None
        }
    }
}

pub(crate) fn backend_name() -> &'static str {
    if cfg!(feature = "vulkan") { "vulkan" } else { "cpu" }
}

fn gpu_layers(c: &Common) -> i32 {
    c.gpu_layers.unwrap_or(if cfg!(feature = "vulkan") { 999 } else { 0 })
}

pub(crate) fn start_engine(c: &Common) -> anyhow::Result<(Engine, Quality)> {
    let quality = Quality::parse(&c.quality).with_context(|| format!("unknown quality {:?}", c.quality))?;
    let set = model_set(quality);
    let model = c.model_dir.join(set.model.file_name);
    let mmproj = c.model_dir.join(set.mmproj.file_name);
    for p in [&model, &mmproj] {
        if !p.exists() {
            bail!("model file missing: {}", p.display());
        }
    }
    let t = Instant::now();
    let engine = Engine::start(&model, &mmproj, gpu_layers(c), c.verbose_llama).map_err(anyhow::Error::msg)?;
    tracing::info!("model {} ({}) ready in {} ms, backend {}", MODEL_ID, quality.as_str(), t.elapsed().as_millis(), backend_name());
    Ok((engine, quality))
}

pub(crate) fn server_info(engine: &Engine, quality: Quality) -> ServerInfo {
    ServerInfo {
        model_id: MODEL_ID.into(),
        quality: quality.as_str().into(),
        dims: searchnow_proto::DIMS as u32,
        audio_sample_rate: engine.audio_sample_rate,
        supports_audio: engine.supports_audio,
        backend: backend_name().into(),
    }
}

async fn run_local(cli: Cli) -> anyhow::Result<()> {
    // The supervisor hands us a per-launch token on the first stdin line; stdin closing means the client died.
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let token = line.trim().to_string();
    if token.len() < 16 {
        bail!("expected an auth token (>= 16 chars) on stdin");
    }
    let (engine, quality) = start_engine(&cli.common)?;
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let svc = Service {
        engine: engine.clone(),
        auth: Arc::new(Auth { local_token: Some(token), remote: None }),
        info: server_info(&engine, quality),
        shutdown: shutdown.clone(),
        pairing: None,
    };
    let addr = SocketAddr::from(([127, 0, 0, 1], cli.port));
    let server = RpcServer::new(addr, svc)
        .await
        .map_err(|e| anyhow::anyhow!("bind {addr}: {e:?}"))?
        .with_config(ServerConfig { max_message_bytes: MAX_MESSAGE_BYTES, ..Default::default() });
    let port = server.local_addr()?.port();
    println!("{READY_PREFIX}{port}");
    tracing::info!("listening on 127.0.0.1:{port}");

    let sd = shutdown.clone();
    std::thread::spawn(move || {
        let mut sink = String::new();
        while std::io::stdin().lock().read_line(&mut sink).map(|n| n > 0).unwrap_or(false) {
            sink.clear();
        }
        tracing::info!("stdin closed, exiting");
        sd.notify_one();
    });
    tokio::select! {
        r = server.run() => { tracing::error!("server stopped: {r:?}"); }
        _ = shutdown.notified() => { tracing::info!("shutdown requested"); }
    }
    engine.close();
    Ok(())
}

fn bench(common: &Common, media: PathBuf, repeat: usize) -> anyhow::Result<()> {
    use searchnow_llama::{Embedder, Input, cosine};
    let quality = Quality::parse(&common.quality).context("quality")?;
    let set = model_set(quality);
    let gpu = gpu_layers(common);
    let t = Instant::now();
    let mut e = Embedder::load(&common.model_dir.join(set.model.file_name), &common.model_dir.join(set.mmproj.file_name), gpu, common.verbose_llama)
        .map_err(anyhow::Error::msg)?;
    println!("quality={} backend={} gpu_layers={gpu} load={} ms", quality.as_str(), backend_name(), t.elapsed().as_millis());

    let cat = std::fs::read(media.join("cat_small.jpg"))?;
    let car = std::fs::read(media.join("car_small.jpg"))?;
    let read_wav = |name: &str| -> anyhow::Result<Vec<f32>> {
        let b = std::fs::read(media.join(name))?;
        // 16-bit PCM mono WAV with a 44-byte header (poc media)
        Ok(b[44..].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect())
    };
    let dog = read_wav("dog.wav")?;
    let rain = read_wav("rain.wav")?;
    let long_text = "The quick brown fox jumps over the lazy dog. ".repeat(100);

    let mut timed = |label: &str, input: Input| -> anyhow::Result<Vec<f32>> {
        let mut best = u128::MAX;
        let mut v = vec![];
        for _ in 0..repeat {
            let t = Instant::now();
            v = e.embed(&input).map_err(anyhow::Error::msg)?;
            best = best.min(t.elapsed().as_millis());
        }
        println!("{label:<28} {best:>6} ms (best of {repeat})");
        Ok(v)
    };
    timed("text short", Input::Text("task: search result | query: a red sports car"))?;
    timed("text ~1000 tokens", Input::Text(&long_text))?;
    let a = timed("image cat", Input::Images(&[&cat]))?;
    let b = timed("image car", Input::Images(&[&car]))?;
    timed("video 4 frames", Input::Images(&[&cat, &car, &cat, &car]))?;
    let d = timed("audio dog", Input::Audio(&dog))?;
    let r = timed("audio rain", Input::Audio(&rain))?;
    println!("sanity: cos(cat,car)={:.3} cos(dog,rain)={:.3} cos(cat,cat)={:.3}", cosine(&a, &b), cosine(&d, &r), cosine(&a, &a));
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let _guard = init_logging(&cli.common.log_dir, "server.log");
    match &cli.cmd {
        Some(Cmd::Bench { media_dir, repeat }) => bench(&cli.common, media_dir.clone(), *repeat),
        Some(Cmd::Serve(a)) => tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(serve::serve(&cli.common, a.clone())),
        Some(Cmd::Pair(a)) => serve::pair(a.clone()),
        Some(Cmd::Clients { cmd: ClientsCmd::List(s) }) => serve::clients_list(s.clone()),
        Some(Cmd::Clients { cmd: ClientsCmd::Revoke { id, state } }) => serve::clients_revoke(state.clone(), id),
        None if cli.local => tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(run_local(cli)),
        None => bail!("specify --local (child mode) or a subcommand: serve, pair, clients, bench"),
    }
}
