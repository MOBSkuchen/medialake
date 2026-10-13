//! W0 spike: an unpackaged program that registers a sync root, creates placeholders, serves FETCH_DATA from a
//! slow in-memory source, and reacts to pin / unpin. It answers the questions of `docs/w0-cloud-files.md`.
//!
//!   cargo run -p medialake-cloudapi --example spike -- C:\Users\you\MediaLakeSpike
//!
//! Then, in Explorer: open `hello.txt` and `big.bin` (watch the log: ranges, alignment, progress), right-click
//! > "Always keep on this device" / "Free up space", rename and delete files, copy a file in. Press Enter in
//! the console to disconnect; `unregister` is asked at the end.

#[cfg(not(windows))]
fn main() {
    eprintln!("The Cloud Files spike runs on Windows only (this host: type-check with --target x86_64-pc-windows-msvc).");
}

#[cfg(windows)]
fn main() {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use medialake_cloudapi::win::{MAX_CHUNK, WinCloud};
    use medialake_cloudapi::*;

    /// Content of every file: byte `i` is `i % 251`. Each chunk takes `delay` to "arrive".
    struct Slow {
        delay: Duration,
    }

    impl Provider for Slow {
        fn fetch(&self, req: &FetchRequest, out: &dyn Transfer) -> CloudResult<()> {
            let started = Instant::now();
            println!("FETCH {} required {}+{} of {} (identity {:?})", req.rel, req.offset, req.length, req.file_size, req.identity.as_ref().map(|i| i.rev));
            let pieces = plan_chunks(req.offset, req.length, req.file_size, MAX_CHUNK);
            let total: u64 = pieces.iter().map(|(_, l)| l).sum();
            let mut done = 0u64;
            for (offset, len) in pieces {
                std::thread::sleep(self.delay);
                let bytes: Vec<u8> = (offset..offset + len).map(|i| (i % 251) as u8).collect();
                out.data(offset, &bytes)?;
                done += len;
                out.progress(total, done);
                println!("  delivered {offset}+{len} ({done}/{total})");
            }
            println!("  done in {:?}", started.elapsed());
            Ok(())
        }

        fn cancel(&self, rel: &str) {
            println!("CANCEL {rel}");
        }
    }

    let root: PathBuf = std::env::args().nth(1).map(PathBuf::from).unwrap_or_else(|| {
        eprintln!("usage: spike <folder on an NTFS drive>");
        std::process::exit(2);
    });
    let api = WinCloud;
    if let Err(e) = api.available(&root) {
        eprintln!("not usable: {e}");
        std::process::exit(1);
    }
    std::fs::create_dir_all(&root).expect("create the folder");
    let reg = Registration {
        provider_name: "MediaLake Spike".into(),
        provider_version: "0.1".into(),
        root_identity: b"spike".to_vec(),
        provider_id: *b"MediaLakeSpike01",
    };
    api.register(&root, &reg).expect("register the sync root");
    let conn = api.connect(&root, Arc::new(Slow { delay: Duration::from_millis(250) })).expect("connect");
    println!("registered and connected: {}", root.display());

    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    let id = Some(Identity { rev: 1, hash: "00".into() });
    let file = |size: u64| PlaceholderInfo { is_dir: false, size, mtime_ms: now_ms - 3_600_000, read_only: false, identity: id.clone(), in_sync: true };
    let dir = PlaceholderInfo { is_dir: true, size: 0, mtime_ms: now_ms, read_only: false, identity: None, in_sync: true };
    for (parent, name, info) in [
        (root.clone(), "hello.txt", file(1024)),
        (root.clone(), "big.bin", file(64 << 20)),
        (root.clone(), "docs", dir),
        (root.join("docs"), "notes.txt", file(10_000)),
    ] {
        if parent.exists() && !parent.join(name).exists() {
            match api.create_placeholder(&parent, name, &info) {
                Ok(()) => println!("placeholder {}", parent.join(name).display()),
                Err(e) => println!("placeholder {name}: {e}"),
            }
        }
    }

    // pin / unpin: Explorer sets attributes, nobody calls us; poll them
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let s2 = stop.clone();
    let watcher_root = root.clone();
    let poller = std::thread::spawn(move || {
        let api = WinCloud;
        while !s2.load(std::sync::atomic::Ordering::SeqCst) {
            let mut stack = vec![watcher_root.clone()];
            while let Some(d) = stack.pop() {
                let Ok(rd) = std::fs::read_dir(&d) else { continue };
                for e in rd.flatten() {
                    let p = e.path();
                    let Ok(st) = api.state(&p) else { continue };
                    if st.is_dir {
                        stack.push(p);
                        continue;
                    }
                    match (st.pin, st.dehydrated, st.in_sync) {
                        (Pin::Pinned, true, _) => {
                            println!("PINNED   {} -> hydrating", p.display());
                            if let Err(e) = api.hydrate(&p) {
                                println!("  hydrate failed: {e}");
                            }
                        }
                        (Pin::Unpinned, false, true) => {
                            println!("UNPINNED {} -> freeing", p.display());
                            if let Err(e) = api.dehydrate(&p) {
                                println!("  dehydrate failed: {e}");
                            }
                        }
                        _ => {}
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    });

    println!("press Enter to stop");
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = poller.join();
    drop(conn);
    println!("disconnected. Unregister the sync root (placeholders stop working)? [y/N]");
    line.clear();
    let _ = std::io::stdin().read_line(&mut line);
    if line.trim().eq_ignore_ascii_case("y") {
        match api.unregister(&root) {
            Ok(()) => println!("unregistered"),
            Err(e) => println!("unregister: {e}"),
        }
    }
}
