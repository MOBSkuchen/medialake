//! End-to-end tests against the real server and model. Run with:
//!   MEDIALAKE_MODEL_DIR=<dir with gguf files> cargo test --release -p medialake-core --test e2e -- --ignored --nocapture --test-threads=1

use std::path::{Path, PathBuf};
use std::time::Duration;

use medialake_core::config::{Config, Paths};
use medialake_core::core::{Core, StartOptions};

fn server_exe() -> PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop(); // deps
    p.pop();
    p.join("medialake-server.exe")
}

async fn start(home: &Path, mutate: impl FnOnce(&mut Config)) -> std::sync::Arc<Core> {
    let paths = Paths::new(home);
    paths.ensure().unwrap();
    let mut cfg = Config::default();
    cfg.debounce_ms = 300;
    cfg.min_change_bytes = 200;
    cfg.threshold = 0.4;
    mutate(&mut cfg);
    cfg.save(&paths.config_file).unwrap();
    let opts = StartOptions { server_exe: Some(server_exe()), ..Default::default() };
    Core::start(paths, opts).await.unwrap()
}

fn chunk_ids(core: &Core, path: &Path) -> Vec<(String, Vec<u8>)> {
    let st = core.store.lock().unwrap();
    match st.file_by_path(&path.to_string_lossy()).unwrap() {
        Some(f) => st.chunks_of(f.id).unwrap().into_iter().map(|c| (c.kind, c.chunk_hash)).collect(),
        None => vec![],
    }
}

fn big_text(tag: &str, lines: usize) -> String {
    (0..lines).map(|i| format!("{tag} line {i}: the harbour lights shimmer over quiet water number {}\n", i * 31 % 97)).collect()
}

async fn settle(core: &Core) {
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(core.wait_idle(Duration::from_secs(300)).await, "indexing did not finish");
}

#[tokio::test]
#[ignore]
async fn watcher_and_reembed_rules() {
    let _ = tracing_subscriber::fmt().with_env_filter("medialake_core=debug").with_test_writer().try_init();
    let tmp = std::env::temp_dir().join(format!("medialake-e2e-{}", std::process::id()));
    let (home, docs) = (tmp.join("home"), tmp.join("docs"));
    std::fs::create_dir_all(&docs).unwrap();
    let f = docs.join("notes.txt");
    std::fs::write(&f, big_text("alpha", 200)).unwrap();

    let core = start(&home, |_| {}).await;
    core.add_dir(docs.clone(), true).await.unwrap();
    settle(&core).await;
    let first = chunk_ids(&core, &f);
    assert!(first.iter().any(|(k, _)| k == "name") && first.iter().any(|(k, _)| k == "text"), "{first:?}");

    // 1. tiny edit (< min_change 200 B): vectors are kept, but the change accumulates
    let mut s = std::fs::read_to_string(&f).unwrap();
    s.push_str("tiny\n");
    std::fs::write(&f, &s).unwrap();
    settle(&core).await;
    assert_eq!(chunk_ids(&core, &f), first, "small edit must not re-embed");

    // 2. more small edits: once the accumulated change passes the threshold the file is re-embedded
    s.push_str(&"x".repeat(60));
    s.push('\n');
    s.push_str(&"y".repeat(60));
    s.push('\n');
    s.push_str(&"z".repeat(100));
    s.push('\n');
    std::fs::write(&f, &s).unwrap();
    settle(&core).await;
    let second = chunk_ids(&core, &f);
    assert_ne!(second, first, "accumulated change must trigger a re-embed");

    // 3. rename: path follows, content is searchable under the new name, no re-embed of content
    let g = docs.join("harbour-diary.txt");
    std::fs::rename(&f, &g).unwrap();
    settle(&core).await;
    assert!(chunk_ids(&core, &f).is_empty());
    let moved = chunk_ids(&core, &g);
    let content = |v: &[(String, Vec<u8>)]| v.iter().filter(|(k, _)| k == "text").cloned().collect::<Vec<_>>();
    assert_eq!(content(&moved), content(&second), "content vectors must survive a rename");
    let hits = core.search("harbour lights over quiet water").await.unwrap();
    assert!(hits.first().is_some_and(|h| h.path.ends_with("harbour-diary.txt")), "{hits:?}");

    // 4. delete
    std::fs::remove_file(&g).unwrap();
    settle(&core).await;
    assert!(chunk_ids(&core, &g).is_empty());
    assert!(core.search("harbour lights over quiet water").await.unwrap().is_empty());

    // 5. too large for the content index: name only
    let mut cfg = core.config();
    cfg.max_file_bytes = 10_000;
    core.set_config(cfg).await.unwrap();
    let h = docs.join("huge.txt");
    std::fs::write(&h, big_text("omega", 400)).unwrap();
    settle(&core).await;
    let kinds: Vec<String> = chunk_ids(&core, &h).into_iter().map(|c| c.0).collect();
    assert_eq!(kinds, vec!["name".to_string()], "oversize file must be name-only");

    core.shutdown().await;
    let _ = std::fs::remove_dir_all(&tmp);
}
