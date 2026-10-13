mod engine;
mod serve;
mod service;
mod syncs;

use std::io::BufRead;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, bail};
use bierpc::rpc::{RpcServer, ServerConfig};
use clap::{Parser, Subcommand};
use medialake_proto::models::{Model, ModelSet, Quality, model_set};
use medialake_proto::{MAX_MESSAGE_BYTES, READY_PREFIX, ServerInfo};

use crate::engine::Engine;
use crate::service::{Auth, Service};
use crate::serve::{PairArgs, ServeArgs, StateArgs};
use crate::syncs::SyncCmd;

#[derive(Parser)]
#[command(name = "medialake-server", version, about = "MediaLake embedding server (EmbeddingGemma 2 / Qwen3-VL-Embedding via llama.cpp)")]
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
    #[arg(long, env = "MEDIALAKE_MODEL_DIR", default_value = ".")]
    model_dir: PathBuf,
    /// embeddinggemma-2, qwen3-vl-embedding-2b or qwen3-vl-embedding-8b.
    #[arg(long, default_value = "embeddinggemma-2")]
    model: String,
    #[arg(long, default_value = "normal")]
    quality: String,
    /// Layers to offload (0 = CPU). Default: all on macOS (Metal) or when built with Vulkan, else 0.
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
    /// Manage hosted syncs (shared folders): add, grant, snapshots, restore.
    Sync {
        #[command(subcommand)]
        cmd: SyncCmd,
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
    if cfg!(target_os = "macos") {
        "metal"
    } else if cfg!(feature = "vulkan") {
        "vulkan"
    } else {
        "cpu"
    }
}

fn gpu_layers(c: &Common) -> i32 {
    c.gpu_layers.unwrap_or(if cfg!(any(target_os = "macos", feature = "vulkan")) { 999 } else { 0 })
}

fn selected_set(c: &Common) -> anyhow::Result<ModelSet> {
    let model = Model::parse(&c.model).with_context(|| format!("unknown model {:?}", c.model))?;
    let quality = Quality::parse(&c.quality).with_context(|| format!("unknown quality {:?}", c.quality))?;
    Ok(model_set(model, quality))
}

pub(crate) fn start_engine(c: &Common) -> anyhow::Result<(Engine, ModelSet)> {
    let set = selected_set(c)?;
    let model = c.model_dir.join(set.model.file_name);
    let mmproj = c.model_dir.join(set.mmproj.file_name);
    for p in [&model, &mmproj] {
        if !p.exists() {
            bail!("model file missing: {}", p.display());
        }
    }
    let t = Instant::now();
    let engine = Engine::start(set.model_id, &model, &mmproj, gpu_layers(c), c.verbose_llama).map_err(anyhow::Error::msg)?;
    tracing::info!("model {} ({}) ready in {} ms, backend {}", set.model_id.as_str(), set.quality.as_str(), t.elapsed().as_millis(), backend_name());
    Ok((engine, set))
}

/// True if both model files of the configured model and quality exist.
pub(crate) fn model_files_present(c: &Common) -> bool {
    let Ok(set) = selected_set(c) else { return false };
    [set.model.file_name, set.mmproj.file_name].iter().all(|f| c.model_dir.join(f).exists())
}

pub(crate) fn server_info(engine: &Engine, set: &ModelSet) -> ServerInfo {
    ServerInfo {
        model_id: set.model_id.as_str().into(),
        quality: set.quality.as_str().into(),
        dims: set.model_id.dims() as u32,
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
    let (engine, set) = start_engine(&cli.common)?;
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let svc = Service {
        engine: Some(engine.clone()),
        model: set.model_id,
        auth: Arc::new(Auth { local_token: Some(token), remote: None }),
        info: Some(server_info(&engine, &set)),
        sync: None,
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
    use medialake_llama::{Embedder, Input, cosine};
    use medialake_proto::{TextItem, TextKind, format_text};
    let set = selected_set(common)?;
    let gpu = gpu_layers(common);
    let t = Instant::now();
    let params = engine::llama_params(set.model_id);
    let mut e = Embedder::load(&common.model_dir.join(set.model.file_name), &common.model_dir.join(set.mmproj.file_name), gpu, common.verbose_llama, params)
        .map_err(anyhow::Error::msg)?;
    let audio = e.supports_audio();
    println!(
        "model={} quality={} dims={} backend={} gpu_layers={gpu} load={} ms",
        set.model_id.as_str(),
        set.quality.as_str(),
        e.dims(),
        backend_name(),
        t.elapsed().as_millis()
    );
    let text = |kind, text: &str| format_text(set.model_id, kind, &TextItem { title: String::new(), text: text.into() });

    let cat = std::fs::read(media.join("cat_small.jpg"))?;
    let car = std::fs::read(media.join("car_small.jpg"))?;
    let read_wav = |name: &str| -> anyhow::Result<Vec<f32>> {
        let b = std::fs::read(media.join(name))?;
        // 16-bit PCM mono WAV with a 44-byte header (poc media)
        Ok(b[44..].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect())
    };
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
    let q = timed("text short", Input::Text(&text(TextKind::Query, "a red sports car")))?;
    timed("text ~1000 tokens", Input::Text(&text(TextKind::Document, &long_text)))?;
    let a = timed("image cat", Input::Images(&[&cat]))?;
    let b = timed("image car", Input::Images(&[&car]))?;
    timed("video 4 frames", Input::Images(&[&cat, &car, &cat, &car]))?;
    println!("sanity: cos(cat,car)={:.3} cos(cat,cat)={:.3}", cosine(&a, &b), cosine(&a, &a));
    println!("query 'a red sports car': cos(q,car)={:.3} cos(q,cat)={:.3}", cosine(&q, &b), cosine(&q, &a));
    if audio {
        let d = timed("audio dog", Input::Audio(&read_wav("dog.wav")?))?;
        let r = timed("audio rain", Input::Audio(&read_wav("rain.wav")?))?;
        println!("sanity: cos(dog,rain)={:.3}", cosine(&d, &r));
    } else {
        println!("audio: not supported by this model");
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let _guard = init_logging(&cli.common.log_dir, "server.log");
    match &cli.cmd {
        Some(Cmd::Bench { media_dir, repeat }) => bench(&cli.common, media_dir.clone(), *repeat),
        Some(Cmd::Serve(a)) => tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(serve::serve(&cli.common, a.clone())),
        Some(Cmd::Pair(a)) => serve::pair(a.clone()),
        Some(Cmd::Sync { cmd }) => syncs::run(cmd.clone()),
        Some(Cmd::Clients { cmd: ClientsCmd::List(s) }) => serve::clients_list(s.clone()),
        Some(Cmd::Clients { cmd: ClientsCmd::Revoke { id, state } }) => serve::clients_revoke(state.clone(), id),
        None if cli.local => tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(run_local(cli)),
        None => bail!("specify --local (child mode) or a subcommand: serve, pair, clients, bench"),
    }
}
