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
root = '{}'
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

/// What the user saw on a German Windows when a computer had stopped sharing: the debug form of a refused
/// connection, with the hint at the very end.
const SEEN: &str = r#"cannot connect to 192.168.178.70:47900: IoError { err: Os { code: 10061, kind: ConnectionRefused, message: "Es konnte keine Verbindung hergestellt werden, da der Zielcomputer die Verbindung verweigerte." } } (is this host a MediaLake host with sync enabled?)"#;

#[test]
fn errors_become_one_plain_sentence() {
    use medialake_core::sync::{classify_error, user_message as u};
    let unreachable = "NAS cannot be reached. It may be switched off, on another network, or no longer sharing folders.";
    assert_eq!(u(SEEN, "NAS"), unreachable);
    assert_eq!(classify_error(SEEN), "offline");
    // what the transport writes now, with and without a name for the other side
    assert_eq!(u("cannot connect to 10.0.0.2:47900: connection refused (is this host a MediaLake host with sync enabled?)", "NAS"), unreachable);
    assert!(u("the connection to nas.local:47900 failed: timed out", "").starts_with("The other computer cannot be reached"));
    assert_eq!(classify_error("the connection to nas.local:47900 failed: connection reset"), "offline");
    // removed on the other side, a folder taken away, a reader trying to restore
    assert_eq!(u("access denied: authentication failed", "NAS"), "NAS no longer accepts this device. Ask for a new invitation code there.");
    assert_eq!(u("access denied: no access to sync 9f2c", "NAS"), "NAS no longer shares this folder with this device.");
    assert!(u("access denied: read-only access", "NAS").starts_with("This folder is read only"));
    // invitation codes
    assert!(u("the host refused pairing: invalid or expired pairing token", "").contains("no longer valid"));
    assert!(u("the pairing code has a typo (checksum mismatch); copy it again", "").contains("typo"));
    assert!(u("this is not a MediaLake pairing code", "").contains("not an invitation code"));
    // the remote search server (Settings > Model) speaks the same way, without the words about folders
    use medialake_core::sync::server_message as s;
    assert_eq!(s("cannot connect to 10.0.0.9:47900: connection refused"), "The server cannot be reached. It may be switched off or on another network.");
    assert!(s("invalid or expired pairing token").contains("no longer valid"));
    assert_eq!(s(r#"unexpected response Embeddings([])"#), "Something went wrong. The details are in the log.");
    // sharing
    assert_eq!(
        u(r#"cannot listen on 0.0.0.0:47900: IoError { err: Os { code: 10048, kind: AddrInUse, message: "Normalerweise darf jede Socketadresse nur einmal verwendet werden." } }"#, ""),
        "Sharing could not start: port 47900 is used by another program."
    );
    // a full disk is told by its number, whatever language the system speaks
    let full = if cfg!(windows) { "a.bin: Es steht nicht genug Speicherplatz zur Verfügung. (os error 112)" } else { "a.bin: Kein Speicherplatz mehr (os error 28)" };
    assert_eq!(classify_error(full), "disk_full");
    assert_eq!(u(full, ""), "This computer's disk is full.");
    // the inside of the program never reaches the user
    assert_eq!(u(r#"unexpected answer to hello: Changes { epoch: "x", seq: 3 }"#, ""), "Something went wrong. The details are in the log.");
    // what is written for people stays, without the part for developers
    assert_eq!(u(r"C:\x\a.txt: Zugriff verweigert (os error 5)", ""), r"C:\x\a.txt: Zugriff verweigert");
    assert_eq!(u("another sync already uses this folder", ""), "another sync already uses this folder");
    // and a sentence that went through once is not changed by a second pass (the UI's last line of defence)
    for raw in [SEEN, "access denied: authentication failed", "the host refused pairing: invalid or expired pairing token", full] {
        let once = u(raw, "NAS");
        assert_eq!(u(&once, ""), once, "{raw}");
        assert!(once.len() < 130, "short enough to be read: {once}");
    }
}

/// A hosts a folder; B pairs. Returns (A, B, shared dir, home of B's folders, the temp dir guard).
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
            // a literal string: a Windows path is full of backslashes
            "search_enabled = false\n[sync]\nenabled = true\ndevice_name = \"Device A\"\n[sync.hosting]\nbind = \"127.0.0.1:0\"\n[[sync.hosted]]\nid = \"fedcba9876543210fedcba9876543210\"\nname = \"Docs\"\nroot = '{}'\nbackup = {{ strategy = \"none\" }}\n",
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
    assert!(st.notice.contains("already had files"), "notice={:?} full={st:?}", st.notice);
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

/// "Name shown to others" is the name everywhere: what a member calls its host, what a host lists its
/// devices as, and both follow a rename. The computer's own name is not used once a name is chosen.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_chosen_name_is_the_name_everywhere() {
    let (a, b, _shared, _base, _tmp, host_id) = host_and_member(&[("hello.txt", b"hello".to_vec())]).await;
    let (ma, mb) = (a.sync().unwrap(), b.sync().unwrap());
    assert_eq!(mb.hosts()[0].name, "Device A", "the host is known by its chosen name, not the computer's");
    assert_eq!(ma.clients().await[0].name, "Device B");

    // the member renames itself: the host's list of devices follows
    let mut c = b.config();
    c.sync.device_name = "Laptop".into();
    b.set_config(c).await.unwrap();
    let t = Instant::now();
    while ma.clients().await[0].name != "Laptop" {
        assert!(t.elapsed() < Duration::from_secs(20), "the host still lists {:?}", ma.clients().await[0].name);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // the host renames itself: the member's list of computers follows
    let mut c = a.config();
    c.sync.device_name = "NAS".into();
    a.set_config(c).await.unwrap();
    let hosts = mb.check_hosts().await; // what the member does by itself every half minute
    assert_eq!((hosts[0].name.as_str(), hosts[0].state.as_str()), ("NAS", "ok"));
    assert_eq!(b.config().sync.hosts[0].name, "NAS", "and it is remembered");
    assert_eq!(hosts[0].id, host_id);
    a.shutdown().await;
    b.shutdown().await;
}

/// "Remove device" on the host: the member must not go on offering that computer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removed_device_no_longer_offers_the_computer() {
    let (a, b, shared, base, _tmp, host_id) = host_and_member(&[("hello.txt", b"hello".to_vec())]).await;
    let (ma, mb) = (a.sync().unwrap(), b.sync().unwrap());
    assert_eq!(mb.available(&host_id).await.unwrap().len(), 1);

    // nothing is joined from it: picking it says why it does not work, and it is gone from the list
    ma.revoke_client("Device B").await.unwrap();
    let e = mb.available(&host_id).await.unwrap_err();
    assert_eq!(e, "Device A no longer accepts this device. Ask for a new invitation code there.");
    assert!(mb.hosts().is_empty(), "{:?}", mb.hosts());
    assert!(b.config().sync.hosts.is_empty());

    // a new code brings it back
    let code = ma.invite(&[("Docs".into(), "rw".into())]).await.unwrap();
    let paired = b.sync_pair(&code).await.unwrap();
    assert_eq!(paired.host_id, host_id);
    let mirror = base.join("mirror");
    b.sync_join(&host_id, "Docs", &mirror, "mirror", "server_wins").await.unwrap();
    until("first sync", Duration::from_secs(30), || mirror.join("hello.txt").exists()).await;

    // with a folder joined the computer stays (its folder says what happened) but is marked, so that the
    // "add a folder" dialog leaves it out
    let id = ma.clients().await[0].id.clone();
    ma.revoke_client(&id).await.unwrap();
    let hosts = mb.check_hosts().await;
    assert_eq!((hosts.len(), hosts[0].state.as_str(), hosts[0].folders), (1, "revoked", 1), "{hosts:?}");
    until("the folder says that access was withdrawn", Duration::from_secs(40), || mb.status()[0].state == "denied").await;
    assert_eq!(mb.status()[0].detail, "Device A no longer accepts this device. Ask for a new invitation code there.");
    assert!(mirror.join("hello.txt").exists(), "the downloaded files stay");

    // "ask for a new invitation code": pairing again revives the folder, which runs with the new credentials
    let code = ma.invite(&[("Docs".into(), "rw".into())]).await.unwrap();
    b.sync_pair(&code).await.unwrap();
    until("the folder syncs again", Duration::from_secs(40), || matches!(mb.status()[0].state.as_str(), "idle" | "syncing")).await;
    assert_eq!(mb.hosts()[0].state, "", "no longer marked");
    std::fs::write(mirror.join("after.txt"), "after pairing again").unwrap();
    until("member -> host", Duration::from_secs(40), || shared.join("after.txt").exists()).await;
    std::fs::write(shared.join("from-host.txt"), "x").unwrap();
    until("host -> member", Duration::from_secs(40), || mirror.join("from-host.txt").exists()).await;

    // leaving the folder lets the computer be forgotten
    b.sync_leave("Docs", true).await.unwrap();
    mb.forget_host(&host_id).await.unwrap();
    assert!(mb.hosts().is_empty());
    a.shutdown().await;
    b.shutdown().await;
}

/// The host turns a device into a reader (and back): the member follows without being restarted, puts
/// back what is changed on it meanwhile, and does not call it an error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_change_of_access_on_the_host_reaches_the_member() {
    let (a, b, shared, base, _tmp, host_id) = host_and_member(&[("keep.txt", b"keep me".to_vec()), ("other.txt", b"x".to_vec())]).await;
    let (ma, mb) = (a.sync().unwrap(), b.sync().unwrap());
    let mirror = base.join("mirror");
    b.sync_join(&host_id, "Docs", &mirror, "mirror", "server_wins").await.unwrap();
    until("first sync", Duration::from_secs(30), || mirror.join("keep.txt").exists() && mirror.join("other.txt").exists()).await;
    assert_eq!(mb.status()[0].access, "rw");

    // read-only on the host; the member finds out when its next change is refused
    ma.grant("Device B", "Docs", "ro").await.unwrap();
    std::fs::write(mirror.join("attempt.txt"), "written by a reader").unwrap();
    until("the member runs the folder as read-only", Duration::from_secs(40), || mb.status()[0].access == "ro").await;
    assert_eq!(b.config().sync.folders[0].access, "ro");
    assert!(!shared.join("attempt.txt").exists(), "a reader's file does not reach the host");
    // a file deleted on the reader comes back, and the host keeps it
    std::fs::remove_file(mirror.join("keep.txt")).unwrap();
    until("the deleted file is back", Duration::from_secs(40), || mirror.join("keep.txt").exists()).await;
    assert_eq!(std::fs::read_to_string(shared.join("keep.txt")).unwrap(), "keep me");
    let st = &mb.status()[0];
    assert!(matches!(st.state.as_str(), "idle" | "syncing" | "starting"), "a change of access is not an error: {st:?}");

    // and read-write again: the member is told by its regular question to the host
    ma.grant("Device B", "Docs", "rw").await.unwrap();
    mb.check_hosts().await;
    assert_eq!(mb.status()[0].access, "rw");
    std::fs::write(mirror.join("new.txt"), "from B").unwrap();
    until("changes travel again", Duration::from_secs(40), || shared.join("new.txt").exists()).await;
    a.shutdown().await;
    b.shutdown().await;
}

/// The Windows counterpart of the mount test: two cores over TLS, the member's folder is a Cloud Files
/// sync root. Windows fetches through the app's own connection to the host when a file is opened.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cloud_sync_loads_on_demand_pins_and_leaves() {
    let big: Vec<u8> = (0..3 * 1024 * 1024 + 11).map(|i| ((i * 7) % 251) as u8).collect();
    let (a, b, shared, base, _tmp, host_id) = host_and_member(&[("hello.txt", b"hello".to_vec()), ("big.bin", big.clone()), ("docs/a.md", b"# a".to_vec())]).await;
    let root = base.join("MediaLake").join("Docs");
    // "auto" is Cloud Files on Windows
    let f = b.sync_join(&host_id, "Docs", &root, "auto", "server_wins").await.unwrap();
    assert_eq!(f.frontend, "auto");
    let m = b.sync().unwrap();

    until("placeholders appear", Duration::from_secs(30), || m.browse("Docs", "").map(|i| i.len() == 3).unwrap_or(false)).await;
    let st = m.status();
    assert_eq!(st[0].frontend, "cloud", "{:?}", st[0]);
    assert_eq!(st[0].notice, "");
    // nothing downloaded yet: names and sizes are there, content is not
    let items = m.browse("Docs", "").unwrap();
    let big_item = items.iter().find(|i| i.name == "big.bin").unwrap();
    assert_eq!((big_item.size, big_item.state.as_str(), big_item.cached_bytes), (big.len() as u64, "cloud", 0));
    assert_eq!(std::fs::metadata(root.join("big.bin")).unwrap().len(), big.len() as u64);

    // opening one file loads it (and only it); the read runs off the runtime's threads, like a program's
    let p = root.join("hello.txt");
    assert_eq!(tokio::task::spawn_blocking(move || std::fs::read_to_string(p)).await.unwrap().unwrap(), "hello");
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
    let p = root.join("big.bin");
    assert_eq!(tokio::task::spawn_blocking(move || std::fs::read(p)).await.unwrap().unwrap(), big);

    // a file written into the folder reaches the host, and a change on the host reaches the folder
    std::fs::write(root.join("from-b.txt"), "written into the sync root").unwrap();
    until("upload", Duration::from_secs(30), || shared.join("from-b.txt").exists()).await;
    assert_eq!(std::fs::read_to_string(shared.join("from-b.txt")).unwrap(), "written into the sync root");
    std::fs::write(shared.join("docs/new.md"), "# new on the host").unwrap();
    until("the host's new file is listed", Duration::from_secs(30), || root.join("docs/new.md").exists()).await;

    // pausing keeps the files reachable
    m.set_folder("Docs", &serde_json::json!({ "paused": true })).await.unwrap();
    let p = root.join("docs/new.md");
    assert_eq!(tokio::task::spawn_blocking(move || std::fs::read_to_string(p)).await.unwrap().unwrap(), "# new on the host");
    m.set_folder("Docs", &serde_json::json!({ "paused": false })).await.unwrap();

    // leaving keeps what was downloaded as ordinary files; the folder is an ordinary folder again
    b.sync_leave("Docs", true).await.unwrap();
    assert_eq!(std::fs::read(root.join("big.bin")).unwrap(), big);
    assert_eq!(std::fs::read_to_string(root.join("docs/a.md")).unwrap(), "# a");
    assert!(b.sync().unwrap().status().is_empty());
    let (n, errors) = b.sync_unregister_roots().await;
    assert_eq!((n, errors.len()), (0, 0), "nothing is left to unregister");
    a.shutdown().await;
    b.shutdown().await;
}

/// A device that the host has made a reader cannot delete or move the host's files in its Cloud Files
/// folder (Windows refuses on the app's behalf), and can again once the host lets it write.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reader_cannot_delete_from_its_cloud_folder() {
    use std::os::windows::process::CommandExt;
    let (a, b, shared, base, _tmp, host_id) = host_and_member(&[("keep.txt", b"keep me".to_vec()), ("d/inner.txt", b"x".to_vec())]).await;
    let (ma, mb) = (a.sync().unwrap(), b.sync().unwrap());
    let root = base.join("Docs");
    b.sync_join(&host_id, "Docs", &root, "auto", "server_wins").await.unwrap();
    until("placeholders appear", Duration::from_secs(30), || root.join("keep.txt").exists() && root.join("d/inner.txt").exists()).await;
    let plain = |rel: &str| root.join(rel).to_string_lossy().trim_start_matches(r"\\?\").replace('/', "\\");
    let run = |line: String| {
        let out = std::process::Command::new("cmd").raw_arg(format!("/s /c \"{line}\"")).output().expect("cmd");
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
    };

    ma.grant("Device B", "Docs", "ro").await.unwrap();
    mb.check_hosts().await;
    let st = mb.status();
    assert_eq!((st[0].access.as_str(), st[0].frontend.as_str()), ("ro", "cloud"), "{:?}", st[0]);
    let said = run(format!("del \"{}\"", plain("keep.txt"))) + &run(format!("rmdir /s /q \"{}\"", plain("d")));
    assert!(root.join("keep.txt").exists() && root.join("d/inner.txt").exists(), "a reader deleted the host's files: {said}");
    // the file still opens, and nothing reached the host
    let p = root.join("keep.txt");
    assert_eq!(tokio::task::spawn_blocking(move || std::fs::read_to_string(p)).await.unwrap().unwrap(), "keep me");
    assert!(shared.join("keep.txt").exists());

    ma.grant("Device B", "Docs", "rw").await.unwrap();
    mb.check_hosts().await;
    assert_eq!(mb.status()[0].access, "rw");
    let said = run(format!("del \"{}\"", plain("d\\inner.txt")));
    assert!(!root.join("d/inner.txt").exists(), "a writer may delete: {said}");
    until("the delete reaches the host", Duration::from_secs(40), || !shared.join("d/inner.txt").exists()).await;
    b.sync_leave("Docs", false).await.unwrap();
    a.shutdown().await;
    b.shutdown().await;
}

/// Removed on the host and invited again: the files of a Cloud Files folder open again (what serves them
/// talks to the host with the new credentials).
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pairing_again_revives_a_cloud_folder() {
    let (a, b, _shared, base, _tmp, host_id) = host_and_member(&[("one.txt", b"one".to_vec()), ("two.txt", b"two".to_vec())]).await;
    let (ma, mb) = (a.sync().unwrap(), b.sync().unwrap());
    let root = base.join("Docs");
    b.sync_join(&host_id, "Docs", &root, "auto", "server_wins").await.unwrap();
    until("placeholders appear", Duration::from_secs(30), || root.join("one.txt").exists() && root.join("two.txt").exists()).await;
    let read = |name: &'static str| {
        let p = root.join(name);
        tokio::task::spawn_blocking(move || std::fs::read_to_string(p))
    };
    assert_eq!(read("one.txt").await.unwrap().unwrap(), "one");

    ma.revoke_client("Device B").await.unwrap();
    assert_eq!(mb.check_hosts().await[0].state, "revoked");
    assert!(read("two.txt").await.unwrap().is_err(), "a removed device gets no content");

    let code = ma.invite(&[("Docs".into(), "rw".into())]).await.unwrap();
    b.sync_pair(&code).await.unwrap();
    assert_eq!(read("two.txt").await.unwrap().unwrap(), "two");
    until("the folder syncs again", Duration::from_secs(40), || matches!(mb.status()[0].state.as_str(), "idle" | "syncing")).await;
    b.sync_leave("Docs", false).await.unwrap();
    a.shutdown().await;
    b.shutdown().await;
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_that_cannot_be_a_sync_root_becomes_a_mirror() {
    let (a, b, _shared, base, _tmp, host_id) = host_and_member(&[("hello.txt", b"hello".to_vec())]).await;
    let point = base.join("occupied");
    std::fs::create_dir_all(&point).unwrap();
    std::fs::write(point.join("mine.txt"), "already here").unwrap();
    let f = b.sync_join(&host_id, "Docs", &point, "auto", "server_wins").await.unwrap();
    let m = b.sync().unwrap();
    until("downloaded as a mirror", Duration::from_secs(30), || point.join("hello.txt").exists()).await;
    let st = &m.status()[0];
    assert_eq!(st.frontend, "mirror", "{st:?}");
    assert!(st.notice.contains("already had files"), "notice={:?} full={st:?}", st.notice);
    assert_eq!(std::fs::read_to_string(point.join("mine.txt")).unwrap(), "already here");
    assert_eq!(f.frontend, "auto");
    // a mirror has nothing to pin
    assert!(m.pin("Docs", "").is_err());
    a.shutdown().await;
    b.shutdown().await;
}

/// The uninstaller's call on Windows: every Cloud Files root goes back to a plain folder, without the
/// sync running.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uninstall_turns_sync_roots_back_into_folders() {
    let (a, b, _shared, base, _tmp, host_id) = host_and_member(&[("seen.txt", b"seen".to_vec()), ("unseen.txt", b"unseen".to_vec())]).await;
    let root = base.join("Docs");
    b.sync_join(&host_id, "Docs", &root, "cloud", "server_wins").await.unwrap();
    let m = b.sync().unwrap();
    until("placeholders appear", Duration::from_secs(30), || m.browse("Docs", "").map(|i| i.len() == 2).unwrap_or(false)).await;
    let p = root.join("seen.txt");
    assert_eq!(tokio::task::spawn_blocking(move || std::fs::read_to_string(p)).await.unwrap().unwrap(), "seen");
    let (n, errors) = b.sync_unregister_roots().await;
    assert_eq!((n, errors.len()), (1, 0), "{errors:?}");
    assert_eq!(std::fs::read_to_string(root.join("seen.txt")).unwrap(), "seen");
    assert!(!root.join("unseen.txt").exists(), "a placeholder without content cannot stay");
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
