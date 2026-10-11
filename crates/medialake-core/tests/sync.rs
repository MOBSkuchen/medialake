//! S5: two cores in one process. A hosts a folder from the app (no model, search off), B pairs with an
//! invite code, joins and keeps a mirror in step in both directions.

use std::path::Path;
use std::time::{Duration, Instant};

use medialake_core::config::Paths;
use medialake_core::core::{Core, StartOptions};

fn write_config(home: &Path, body: &str) {
    std::fs::create_dir_all(home).unwrap();
    std::fs::write(home.join("config.toml"), body).unwrap();
}

async fn until(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let t = Instant::now();
    while t.elapsed() < timeout {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("timed out waiting for: {what}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn app_hosts_and_app_joins() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let (home_a, home_b) = (base.join("home-a"), base.join("home-b"));
    let shared = base.join("shared");
    let mirror = base.join("mirror");
    std::fs::create_dir_all(shared.join("docs")).unwrap();
    std::fs::write(shared.join("hello.txt"), "hello from A").unwrap();
    std::fs::write(shared.join("docs/readme.md"), "# readme").unwrap();

    // A: search off, sync on, one hosted folder, loopback listener
    write_config(
        &home_a,
        &format!(
            r#"
search_enabled = false
[sync]
enabled = true
device_name = "Device A"
[sync.hosting]
bind = "127.0.0.1:0"
[[sync.hosted]]
id = "{}"
name = "Docs"
root = "{}"
backup = {{ strategy = "file", keep = 5 }}
"#,
            "0123456789abcdef0123456789abcdef",
            shared.display()
        ),
    );
    write_config(&home_b, "search_enabled = false\n[sync]\nenabled = true\ndevice_name = \"Device B\"\n");

    let opts = StartOptions { no_watch: true, ..Default::default() };
    let a = Core::start(Paths::new(&home_a), opts.clone()).await.unwrap();
    let b = Core::start(Paths::new(&home_b), opts).await.unwrap();

    // search is off: no model, no indexer, and the search call says so
    assert!(a.search("anything").await.is_err());
    assert!(!a.config().search_enabled);

    let status = a.sync().unwrap().hosting_status().await;
    assert!(status.running, "{status:?}");
    assert_eq!(status.syncs.len(), 1);
    assert_eq!(status.syncs[0].files, 2);

    // invite -> pair -> list -> join
    let code = a.sync().unwrap().invite(&[("Docs".into(), "rw".into())]).await.unwrap();
    let paired = b.sync_pair(&code).await.unwrap();
    assert!(!paired.embedding, "an in-app host does not serve embeddings");
    let avail = b.sync().unwrap().available(&paired.host_id).await.unwrap();
    assert_eq!(avail.len(), 1);
    assert_eq!(avail[0].name, "Docs");
    assert!(!avail[0].joined);
    let f = b.sync_join(&paired.host_id, "Docs", &mirror, "mirror", "server_wins").await.unwrap();
    assert_eq!(f.access, "rw");
    // pairing survived into the config file, with the secret protected
    let on_disk = std::fs::read_to_string(home_b.join("config.toml")).unwrap();
    assert!(on_disk.contains("[[sync.hosts]]") && on_disk.contains("[[sync.folders]]"), "{on_disk}");

    // A -> B
    until("initial download", Duration::from_secs(20), || mirror.join("docs/readme.md").exists() && mirror.join("hello.txt").exists()).await;
    assert_eq!(std::fs::read_to_string(mirror.join("hello.txt")).unwrap(), "hello from A");

    // B -> A (picked up by B's watcher, committed, written into A's folder)
    std::fs::write(mirror.join("from-b.txt"), "written on B").unwrap();
    until("upload to host", Duration::from_secs(30), || shared.join("from-b.txt").exists()).await;
    assert_eq!(std::fs::read_to_string(shared.join("from-b.txt")).unwrap(), "written on B");

    // edit on A's disk -> B (A's watcher + notification stream)
    std::fs::write(shared.join("hello.txt"), "edited on A").unwrap();
    until("host edit reaches the member", Duration::from_secs(30), || {
        std::fs::read_to_string(mirror.join("hello.txt")).map(|s| s == "edited on A").unwrap_or(false)
    })
    .await;

    // state and history are visible through the manager
    let st = b.sync().unwrap().status();
    assert_eq!(st.len(), 1);
    assert_eq!(st[0].name, "Docs");
    let vs = b.sync().unwrap().versions("Docs", "hello.txt").await.unwrap();
    assert!(!vs.is_empty());
    let listing = b.sync().unwrap().browse("Docs", "").unwrap();
    assert!(listing.iter().any(|i| i.name == "docs" && i.is_dir));

    // the host sees who is paired and can take access away again
    let clients = a.sync().unwrap().clients().await;
    assert_eq!(clients.len(), 1);
    assert_eq!(clients[0].syncs.get("Docs").map(String::as_str), Some("rw"));
    a.sync().unwrap().grant("Device B", "Docs", "ro").await.unwrap();
    let avail = b.sync().unwrap().available(&paired.host_id).await.unwrap();
    assert_eq!(avail[0].access, "ro");

    // leaving keeps the files and stops the sync
    b.sync_leave("Docs", true).await.unwrap();
    assert!(mirror.join("hello.txt").exists());
    assert!(b.sync().unwrap().status().is_empty());

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_is_off_by_default_and_search_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    // an old config without any of the new keys
    write_config(&home, "hotkey = \"Ctrl+Space\"\nthreshold = 0.5\n");
    let c = medialake_core::config::Config::load(&home.join("config.toml")).unwrap();
    assert!(c.search_enabled);
    assert!(!c.sync.enabled);
    assert_eq!(c.sync.hosting.bind, "0.0.0.0:47900");
    assert!(c.sync.folders.is_empty());
    // round trip keeps the sync section readable
    let mut c2 = c.clone();
    c2.sync.enabled = true;
    c2.sync.hosted.push(medialake_sync::host::config::HostedConfig {
        id: "aa".into(),
        name: "X".into(),
        root: "/tmp/x".into(),
        store: None,
        backup: medialake_sync::host::config::BackupPolicy::Time { every_hours: 12, keep: 3 },
        ignore: vec!["*.bak".into()],
    });
    c2.save(&home.join("config.toml")).unwrap();
    let c3 = medialake_core::config::Config::load(&home.join("config.toml")).unwrap();
    assert_eq!(c2, c3);
}
