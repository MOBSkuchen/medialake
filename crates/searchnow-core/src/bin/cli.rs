//! `searchnow-cli`: developer / diagnostics front end on top of searchnow-core.
//!
//!   searchnow-cli [--home DIR] [--model-dir DIR] [--server-exe EXE] [--gpu-layers N] <command>
//!     add <dir> [--flat]      add an indexed directory and wait for indexing to finish
//!     index                   reconcile all configured directories and wait
//!     search <query...>       embed the query and print ranked hits (raw cosine + confidence)
//!     status                  print counts and server state
//!     ping                    start the server and print its info

use std::path::PathBuf;
use std::time::{Duration, Instant};

use searchnow_core::config::Paths;
use searchnow_core::core::{Core, StartOptions};

fn usage() -> ! {
    eprintln!("usage: searchnow-cli [--home DIR] [--model-dir DIR] [--server-exe EXE] [--gpu-layers N] <add <dir> [--flat] | index | search <query> | status | ping>");
    std::process::exit(2)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn,searchnow_core=info".into()))
        .with_writer(std::io::stderr)
        .init();
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut home = Paths::default_root();
    let mut opts = StartOptions { no_watch: true, ..Default::default() };
    while args.first().is_some_and(|a| a.starts_with("--")) && args[0] != "--flat" {
        let k = args.remove(0);
        if args.is_empty() {
            usage();
        }
        let v = args.remove(0);
        match k.as_str() {
            "--home" => home = PathBuf::from(v),
            "--model-dir" => opts.model_dir = Some(PathBuf::from(v)),
            "--server-exe" => opts.server_exe = Some(PathBuf::from(v)),
            "--gpu-layers" => opts.gpu_layers = v.parse().ok(),
            _ => usage(),
        }
    }
    if args.is_empty() {
        usage();
    }
    let cmd = args.remove(0);
    let core = Core::start(Paths::new(home), opts).await?;
    let t = Instant::now();
    match cmd.as_str() {
        "ping" => {
            let info = core.client.hello().await?;
            println!("{info:#?}\nready after {:.1} s", t.elapsed().as_secs_f32());
        }
        "add" => {
            let dir = args.first().cloned().unwrap_or_else(|| usage());
            let recursive = !args.iter().any(|a| a == "--flat");
            core.add_dir(searchnow_core::config::normalize_path(&PathBuf::from(dir).canonicalize()?), recursive).await?;
            wait(&core).await;
        }
        "index" => {
            core.indexer.reconcile().await;
            wait(&core).await;
        }
        "search" => {
            let q = args.join(" ");
            let t = Instant::now();
            let hits = core.search(&q).await?;
            println!("{} hits in {} ms", hits.len(), t.elapsed().as_millis());
            for h in hits {
                println!("{:.2} (raw {:.3}) [{}/{}] {}", h.score, h.raw, h.modality, h.matched, h.path);
                if !h.snippet.is_empty() {
                    println!("       {}", h.snippet.chars().take(110).collect::<String>());
                }
            }
        }
        "status" => println!("{:#?}", core.status()),
        "extract" => {
            let p = PathBuf::from(args.first().cloned().unwrap_or_else(|| usage()));
            match searchnow_core::extract::classify(&p) {
                searchnow_core::extract::Modality::Document => {
                    println!("{:?}", searchnow_core::extract_ext::document_text(&p, 1 << 30).map(|t| t.chars().take(300).collect::<String>()))
                }
                searchnow_core::extract::Modality::Audio => match searchnow_core::extract_ext::prepare_audio(&p, 16000) {
                    Ok(a) => println!("audio: {} segments, {:.1} s", a.segments.len(), a.seconds),
                    Err(e) => println!("audio error: {e}"),
                },
                searchnow_core::extract::Modality::Video => match searchnow_core::video::prepare_video(&p) {
                    Ok(v) => println!("video: {} frames, first {} B", v.frames.len(), v.frames[0].len()),
                    Err(e) => println!("video error: {e}"),
                },
                m => println!("{m:?}"),
            }
        }
        "pair" => {
            let code = args.join(" ");
            let r = core.pair_remote(&code).await.map_err(anyhow::Error::msg)?;
            println!("paired: {r:?}");
            let info = core.client.hello().await?;
            println!("server: {info:?}");
        }
        "use-local" => {
            core.use_local().await.map_err(anyhow::Error::msg)?;
            println!("local mode; status: {:?}", core.status().server_state);
        }
        "files" => {
            let st = core.store.lock().unwrap();
            let mut rows: Vec<_> = st.all_seen()?.into_iter().collect();
            rows.sort();
            for (p, (id, size, _m, status)) in rows {
                let n = st.chunk_ids(id)?.len();
                println!("{status:<10} {n:>3} chunks {size:>10} B  {p}");
            }
        }
        "download" => {
            let q = args.first().and_then(|a| searchnow_proto::models::Quality::parse(a)).unwrap_or_else(|| usage());
            let mut rx = core.subscribe();
            let c = core.clone();
            let h = tokio::spawn(async move { c.download_quality(q).await });
            let printer = tokio::spawn(async move {
                while let Ok(ev) = rx.recv().await {
                    if let searchnow_core::events::Event::Download { done, total, .. } = ev {
                        eprint!("\r{} / {} MB", done / 1048576, total / 1048576);
                    }
                }
            });
            let r = h.await?;
            printer.abort();
            println!("\ndownload result: {r:?}; present: {}", searchnow_core::models::is_downloaded(core.model_dir(), q));
        }
        _ => usage(),
    }
    core.shutdown().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    Ok(())
}

async fn wait(core: &std::sync::Arc<Core>) {
    let t = Instant::now();
    if !core.wait_idle(Duration::from_secs(3600)).await {
        eprintln!("timed out waiting for indexing");
    }
    let s = core.status();
    println!("indexed in {:.1} s: {} files, {} vectors ({:?})", t.elapsed().as_secs_f32(), s.files, s.vectors, s.by_status);
}
