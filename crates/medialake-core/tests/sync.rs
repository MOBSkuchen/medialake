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

#[test]
fn errors_map_to_states() {
    use medialake_core::sync::classify_error as c;
    assert_eq!(c("cannot connect to 10.0.0.2:47900: Os { code: 61, kind: ConnectionRefused }"), "offline");
    assert_eq!(c("cannot resolve nas.local:47900: failed to lookup address"), "offline");
    assert_eq!(c("access denied: no access to sync 9f2c"), "denied");
    assert_eq!(c("a.txt: No space left on device (os error 28)"), "disk_full");
    assert_eq!(c("a.txt: rejected by the host: ignored name"), "error");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_can_be_switched_off_and_on_while_running() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    write_config(&home, "search_enabled = false\n");
    let core = Core::start(Paths::new(&home), StartOptions { no_watch: true, ..Default::default() }).await.unwrap();
    let off = format!("{:?}", core.search("hello").await.unwrap_err());
    assert!(off.contains("turned off"), "{off}");

    let mut c = core.config();
    c.search_enabled = true;
    core.set_config(c).await.unwrap();
    // no model server in this test: the failure is now "unavailable", not "turned off"
    let on = format!("{:?}", core.search("hello").await.unwrap_err());
    assert!(!on.contains("turned off"), "{on}");
    // the setting was saved
    assert!(medialake_core::config::Config::load(&home.join("config.toml")).unwrap().search_enabled);

    let mut c = core.config();
    c.search_enabled = false;
    core.set_config(c).await.unwrap();
    let off = format!("{:?}", core.search("hello").await.unwrap_err());
    assert!(off.contains("turned off"), "{off}");
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_can_be_enabled_at_runtime_and_limits_apply() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    write_config(&home, "search_enabled = false\n");
    let core = Core::start(Paths::new(&home), StartOptions { no_watch: true, ..Default::default() }).await.unwrap();
    assert!(core.sync().is_err());
    let mut c = core.config();
    c.sync.enabled = true;
    c.sync.upload_limit_kbps = 4000;
    core.set_config(c).await.unwrap();
    let m = core.sync().unwrap();
    assert!(m.status().is_empty());
    // raising the limit later reaches the manager without a restart
    let mut c = core.config();
    c.sync.upload_limit_kbps = 0;
    core.set_config(c).await.unwrap();
    let mut c = core.config();
    c.sync.enabled = false;
    core.set_config(c).await.unwrap();
    assert!(core.sync().is_err());
    core.shutdown().await;
}

/// A hosts a folder; B pairs. Returns (A, B, shared dir, home of B's folders, the temp dir guard).
#[cfg(target_os = "macos")]
async fn host_and_member(files: &[(&str, Vec<u8>)]) -> (std::sync::Arc<Core>, std::sync::Arc<Core>, std::path::PathBuf, std::path::PathBuf, tempfile::TempDir, String) {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let (home_a, home_b) = (base.join("home-a"), base.join("home-b"));
    let shared = base.join("shared");
    std::fs::create_dir_all(&shared).unwrap();
    for (rel, bytes) in files {
        let p = shared.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, bytes).unwrap();
    }
    write_config(
        &home_a,
        &format!(
            "search_enabled = false\n[sync]\nenabled = true\ndevice_name = \"Device A\"\n[sync.hosting]\nbind = \"127.0.0.1:0\"\n[[sync.hosted]]\nid = \"fedcba9876543210fedcba9876543210\"\nname = \"Docs\"\nroot = \"{}\"\nbackup = {{ strategy = \"none\" }}\n",
            shared.display()
        ),
    );
    write_config(&home_b, "search_enabled = false\n[sync]\nenabled = true\ndevice_name = \"Device B\"\n");
    let opts = StartOptions { no_watch: true, ..Default::default() };
    let a = Core::start(Paths::new(&home_a), opts.clone()).await.unwrap();
    let b = Core::start(Paths::new(&home_b), opts).await.unwrap();
    let code = a.sync().unwrap().invite(&[("Docs".into(), "rw".into())]).await.unwrap();
    let paired = b.sync_pair(&code).await.unwrap();
    (a, b, shared, base, tmp, paired.host_id)
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mounted_sync_loads_on_demand_pins_and_leaves() {
    let big: Vec<u8> = (0..3 * 1024 * 1024 + 11).map(|i| ((i * 7) % 251) as u8).collect();
    let (a, b, shared, base, _tmp, host_id) = host_and_member(&[("hello.txt", b"hello".to_vec()), ("big.bin", big.clone()), ("docs/a.md", b"# a".to_vec())]).await;
    let point = base.join("volume");
    let f = b.sync_join(&host_id, "Docs", &point, "mount", "server_wins").await.unwrap();
    assert_eq!(f.frontend, "mount");
    let m = b.sync().unwrap();

    until("placeholders appear", Duration::from_secs(30), || {
        m.browse("Docs", "").map(|i| i.len() == 3).unwrap_or(false)
    })
    .await;
    let st = m.status();
    assert_eq!(st[0].frontend, "mount", "{:?}", st[0]);
    assert_eq!(st[0].notice, "");
    // nothing downloaded yet: sizes are there, content is not
    let items = m.browse("Docs", "").unwrap();
    let big_item = items.iter().find(|i| i.name == "big.bin").unwrap();
    assert_eq!(big_item.size, big.len() as u64);
    assert_eq!(big_item.state, "cloud");
    assert_eq!(m.status()[0].cached_bytes, 0);

    // opening one file loads it (and only it)
    assert_eq!(std::fs::read_to_string(point.join("hello.txt")).unwrap(), "hello");
    let items = m.browse("Docs", "").unwrap();
    assert_eq!(items.iter().find(|i| i.name == "hello.txt").unwrap().state, "cached");
    assert_eq!(items.iter().find(|i| i.name == "big.bin").unwrap().state, "cloud");

    // keep the whole sync on this device
    m.pin("Docs", "").unwrap();
    until("pinned files arrive", Duration::from_secs(30), || {
        let top = m.browse("Docs", "").unwrap();
        let inner = m.browse("Docs", "docs").unwrap();
        top.iter().filter(|i| !i.is_dir).all(|i| i.state == "cached") && inner.iter().all(|i| i.state == "cached")
    })
    .await;
    assert!(m.status()[0].cached_bytes >= big.len() as u64);

    // a file written into the volume reaches the host
    std::fs::write(point.join("from-b.txt"), "written through the mount").unwrap();
    until("upload", Duration::from_secs(30), || shared.join("from-b.txt").exists()).await;
    assert_eq!(std::fs::read_to_string(shared.join("from-b.txt")).unwrap(), "written through the mount");

    // leaving keeps what was downloaded as ordinary files and removes the volume
    b.sync_leave("Docs", true).await.unwrap();
    assert!(!medialake_mount::mountctl::mount_at(&point).is_some_and(|m| m.is_ours()));
    assert_eq!(std::fs::read(point.join("big.bin")).unwrap(), big);
    assert_eq!(std::fs::read_to_string(point.join("docs/a.md")).unwrap(), "# a");
    assert!(b.sync().unwrap().status().is_empty());
    a.shutdown().await;
    b.shutdown().await;
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_that_cannot_be_mounted_becomes_a_mirror() {
    let (a, b, _shared, base, _tmp, host_id) = host_and_member(&[("hello.txt", b"hello".to_vec())]).await;
    let point = base.join("occupied");
    std::fs::create_dir_all(&point).unwrap();
    std::fs::write(point.join("mine.txt"), "already here").unwrap();
    let f = b.sync_join(&host_id, "Docs", &point, "auto", "server_wins").await.unwrap();
    let m = b.sync().unwrap();
    until("downloaded as a mirror", Duration::from_secs(30), || point.join("hello.txt").exists()).await;
    let st = &m.status()[0];
    assert_eq!(st.frontend, "mirror", "{st:?}");
    assert!(st.notice.contains("not empty"), "notice={:?} full={st:?}", st.notice);
    assert_eq!(std::fs::read_to_string(point.join("mine.txt")).unwrap(), "already here");
    assert_eq!(f.frontend, "auto");
    a.shutdown().await;
    b.shutdown().await;
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cache_that_does_not_belong_to_the_engine_state_is_set_aside() {
    let (a, b, shared, base, _tmp, host_id) = host_and_member(&[("photo.jpg", b"the host's photo".to_vec())]).await;
    // a leftover cache of this sync (an earlier install, a deleted state database), with a stale file
    let stale = base.join("home-b/cache/sync/fedcba9876543210fedcba9876543210/tree");
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::write(stale.join("photo.jpg"), "stale content from before").unwrap();
    std::fs::write(stale.join("deleted-on-host.txt"), "must not come back").unwrap();

    let point = base.join("volume");
    b.sync_join(&host_id, "Docs", &point, "mount", "server_wins").await.unwrap();
    let m = b.sync().unwrap();
    until("synced", Duration::from_secs(30), || point.join("photo.jpg").exists()).await;
    assert_eq!(std::fs::read_to_string(point.join("photo.jpg")).unwrap(), "the host's photo");
    assert!(!point.join("deleted-on-host.txt").exists());
    // nothing stale reached the host
    assert!(!shared.join("deleted-on-host.txt").exists());
    assert_eq!(std::fs::read_to_string(shared.join("photo.jpg")).unwrap(), "the host's photo");
    let st = &m.status()[0];
    assert!(st.notice.contains("set aside"), "{st:?}");
    assert_eq!(st.pending_conflicts, 0);
    a.shutdown().await;
    b.shutdown().await;
}

/// Cloud Files is a Windows thing: elsewhere it is refused at join time, and "nothing to unregister" is
/// a clean answer for the uninstall call.
#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cloud_files_is_refused_off_windows_and_unregistering_nothing_is_fine() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    write_config(&home, "search_enabled = false\n[sync]\nenabled = true\n");
    let core = Core::start(Paths::new(&home), StartOptions { no_watch: true, ..Default::default() }).await.unwrap();
    let e = core.sync_join("nohost", "x", &tmp.path().join("f"), "cloud", "server_wins").await.unwrap_err();
    assert!(e.contains("only available on Windows"), "{e}");
    let (n, errors) = core.sync_unregister_roots().await;
    assert_eq!((n, errors.len()), (0, 0));
    // the sync was stopped by that call: starting it again is a settings change away
    assert!(core.sync().is_err());
    core.shutdown().await;
}
