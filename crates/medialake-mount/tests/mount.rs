//! A1: a host, a member engine over the mount frontend, and a real `mount_nfs` of the result. The test
//! process plays Finder: it uses ordinary file calls on the mount point. macOS only (the mount needs the
//! system's NFS client; it works without root).

#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use medialake_mount::{MountConfig, MountSession};
use medialake_proto::sync::Access;
use medialake_sync::host::config::{BackupPolicy, HostedConfig};
use medialake_sync::host::hosted::HostedSync;
use medialake_sync::member::engine::{ConflictPolicy, Engine, EngineConfig};
use medialake_sync::member::frontend::Frontend;
use medialake_sync::member::transport::{ChangesReply, Fetch, LocalTransport, RangeFetch, Transport, Upload};
use medialake_proto::sync::{Change, ChangeResult, RestoreWhat, SnapshotInfo, Verdict, VersionInfo};
use std::sync::atomic::{AtomicBool, Ordering};

/// The host, which can be made unreachable.
struct Flaky {
    inner: LocalTransport,
    offline: Arc<AtomicBool>,
}

impl Flaky {
    fn check(&self) -> anyhow::Result<()> {
        if self.offline.load(Ordering::SeqCst) {
            anyhow::bail!("cannot connect to the host: Connection refused");
        }
        Ok(())
    }
}

impl Transport for Flaky {
    fn changes(&self, e: &str, c: u64, l: u32) -> anyhow::Result<ChangesReply> {
        self.check()?;
        self.inner.changes(e, c, l)
    }
    fn propose(&self, c: &[Change]) -> anyhow::Result<Verdict> {
        self.check()?;
        self.inner.propose(c)
    }
    fn commit(&self, c: &[Change]) -> anyhow::Result<Vec<ChangeResult>> {
        self.check()?;
        self.inner.commit(c)
    }
    fn download(&self, p: &str, r: u64, part: &Path) -> anyhow::Result<Fetch> {
        self.check()?;
        self.inner.download(p, r, part)
    }
    fn read_range(&self, p: &str, r: u64, o: u64, l: u64) -> anyhow::Result<RangeFetch> {
        self.check()?;
        self.inner.read_range(p, r, o, l)
    }
    fn upload(&self, h: &str, s: &Path, n: u64) -> anyhow::Result<Upload> {
        self.check()?;
        self.inner.upload(h, s, n)
    }
    fn versions(&self, p: &str) -> anyhow::Result<Vec<VersionInfo>> {
        self.check()?;
        self.inner.versions(p)
    }
    fn snapshots(&self) -> anyhow::Result<Vec<SnapshotInfo>> {
        self.check()?;
        self.inner.snapshots()
    }
    fn restore(&self, w: RestoreWhat, c: bool) -> anyhow::Result<Vec<ChangeResult>> {
        self.check()?;
        self.inner.restore(w, c)
    }
}

const MIB: usize = 1 << 20;

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| ((i * 31 + seed as usize) % 251) as u8).collect()
}

struct Fx {
    _dir: tempfile::TempDir,
    host_root: PathBuf,
    hs: Arc<HostedSync>,
    point: PathBuf,
    engine: Engine,
    session: Option<MountSession>,
    tr: Arc<dyn Transport>,
    cache: PathBuf,
    offline: Arc<AtomicBool>,
}

impl Fx {
    async fn new(limit: u64, read_only: bool) -> Fx {
        Fx::with(limit, read_only, ConflictPolicy::ServerWins).await
    }

    async fn with(limit: u64, read_only: bool, policy: ConflictPolicy) -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let host_root = base.join("host");
        std::fs::create_dir_all(&host_root).unwrap();
        let hs = HostedSync::open(
            HostedConfig {
                id: medialake_sync::util::random_id(),
                name: "Test".into(),
                root: host_root.clone(),
                store: None,
                backup: BackupPolicy::None,
                ignore: vec![],
            },
            None,
        )
        .unwrap();
        let access = if read_only { Access::ReadOnly } else { Access::ReadWrite };
        let offline = Arc::new(AtomicBool::new(false));
        let tr: Arc<dyn Transport> = Arc::new(Flaky { inner: LocalTransport { host: hs.clone(), access, author: "Mac".into() }, offline: offline.clone() });
        let point = base.join("mnt");
        let cache = base.join("cache");
        let session = MountSession::start(
            MountConfig { sync_id: "s1".into(), name: "Test".into(), point: point.clone(), cache_dir: cache.clone(), read_only, limit_bytes: limit },
            tr.clone(),
        )
        .await
        .expect("mount");
        let fe: Arc<dyn Frontend> = session.frontend();
        let cfg = EngineConfig {
            device: "Mac".into(),
            policy,
            access,
            state_dir: base.join("state"),
            settle_ms: 0,
            ignore: vec![],
        };
        let engine = Engine::new(cfg, fe, tr.clone()).unwrap();
        Fx { _dir: dir, host_root, hs, point, engine, session: Some(session), tr, cache, offline }
    }

    fn host_write(&self, rel: &str, bytes: &[u8]) {
        let p = self.host_root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, bytes).unwrap();
        // old enough to count as settled
        medialake_sync::util::set_mtime_ms(&p, medialake_sync::util::now_ms() - 600_000).unwrap();
    }

    /// Host scan + engine rounds until nothing moves.
    fn settle(&mut self) {
        for _ in 0..12 {
            let mut activity = self.hs.scan().unwrap();
            let r = self.engine.sync_once().unwrap();
            activity += r.downloaded + r.pushed + r.uploaded + r.removed_local + r.conflicts;
            if !r.retry && activity == 0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        panic!("did not settle");
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.point.join(rel)
    }

    fn stats(&self) -> medialake_mount::StoreStats {
        self.session.as_ref().unwrap().store().stats()
    }

    async fn stop(mut self) {
        if let Some(s) = self.session.take() {
            s.stop().await;
        }
    }
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(20) {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn files_appear_without_content_and_load_on_demand() {
    let mut fx = Fx::new(0, false).await;
    fx.host_write("small.txt", b"hello from the host");
    fx.host_write("big.bin", &pattern(5 * MIB + 123, 1));
    fx.host_write("docs/a.md", b"# a");
    fx.settle();

    // browsing: names, sizes and folders are there, and nothing was fetched
    let mut names: Vec<String> = std::fs::read_dir(&fx.point).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    assert_eq!(names, vec!["big.bin", "docs", "small.txt"]);
    assert_eq!(std::fs::metadata(fx.p("big.bin")).unwrap().len(), (5 * MIB + 123) as u64);
    assert_eq!(fx.stats().fetched_bytes, 0, "browsing must not fetch content");
    assert_eq!(fx.stats().cached_bytes, 0);

    // a small read fetches one block, not the file
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(fx.p("big.bin")).unwrap();
    f.seek(SeekFrom::Start(3 * MIB as u64 + 10)).unwrap();
    let mut buf = [0u8; 4096];
    f.read_exact(&mut buf).unwrap();
    let want = pattern(5 * MIB + 123, 1);
    assert_eq!(&buf[..], &want[3 * MIB + 10..3 * MIB + 10 + 4096]);
    let fetched = fx.stats().fetched_bytes;
    assert!(fetched >= MIB as u64 && fetched <= 2 * MIB as u64, "fetched {fetched}");

    // the whole file reads back identical, and small files too
    assert_eq!(std::fs::read(fx.p("big.bin")).unwrap(), want);
    assert_eq!(std::fs::read_to_string(fx.p("small.txt")).unwrap(), "hello from the host");
    assert_eq!(std::fs::read_to_string(fx.p("docs/a.md")).unwrap(), "# a");
    let st = fx.session.as_ref().unwrap().store().browse("").unwrap();
    assert!(st.iter().any(|i| i.name == "big.bin" && i.state == "cached"), "{st:?}");

    // reading changed nothing for the sync
    fx.settle();
    assert!(fx.hs.entry("big.bin").is_some());
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writes_creates_renames_and_deletes_reach_the_host() {
    let mut fx = Fx::new(0, false).await;
    fx.host_write("keep.txt", b"original");
    fx.host_write("big.bin", &pattern(3 * MIB, 2));
    fx.settle();

    // a new file, written through the mount
    std::fs::write(fx.p("new.txt"), "made on the mac").unwrap();
    std::fs::create_dir(fx.p("folder")).unwrap();
    std::fs::write(fx.p("folder/inner.txt"), "inner").unwrap();
    fx.settle();
    assert_eq!(std::fs::read_to_string(fx.host_root.join("new.txt")).unwrap(), "made on the mac");
    assert_eq!(std::fs::read_to_string(fx.host_root.join("folder/inner.txt")).unwrap(), "inner");

    // an in-place edit of a placeholder: appended to, so the rest must come first
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(fx.p("big.bin")).unwrap();
        f.write_all(b"TAIL").unwrap();
    }
    fx.settle();
    let mut want = pattern(3 * MIB, 2);
    want.extend_from_slice(b"TAIL");
    assert_eq!(std::fs::read(fx.host_root.join("big.bin")).unwrap(), want);

    // atomic save: temp file then rename over the original
    std::fs::write(fx.p(".keep.txt.sb-123"), "saved twice").unwrap();
    std::fs::rename(fx.p(".keep.txt.sb-123"), fx.p("keep.txt")).unwrap();
    fx.settle();
    assert_eq!(std::fs::read_to_string(fx.host_root.join("keep.txt")).unwrap(), "saved twice");
    assert!(!fx.host_root.join(".keep.txt.sb-123").exists());

    // move and delete
    std::fs::rename(fx.p("new.txt"), fx.p("folder/moved.txt")).unwrap();
    std::fs::remove_file(fx.p("folder/inner.txt")).unwrap();
    fx.settle();
    assert!(!fx.host_root.join("new.txt").exists());
    assert_eq!(std::fs::read_to_string(fx.host_root.join("folder/moved.txt")).unwrap(), "made on the mac");
    assert!(!fx.host_root.join("folder/inner.txt").exists());

    // AppleDouble and .DS_Store stay on this Mac
    std::fs::write(fx.p("._keep.txt"), "resource fork").unwrap();
    std::fs::write(fx.p(".DS_Store"), "finder").unwrap();
    fx.settle();
    assert!(!fx.host_root.join("._keep.txt").exists());
    assert!(!fx.host_root.join(".DS_Store").exists());
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_changes_replace_cached_content() {
    let mut fx = Fx::new(0, false).await;
    fx.host_write("doc.txt", b"version one");
    fx.settle();
    assert_eq!(std::fs::read_to_string(fx.p("doc.txt")).unwrap(), "version one");

    fx.host_write("doc.txt", b"version two, longer");
    fx.settle();
    assert_eq!(std::fs::metadata(fx.p("doc.txt")).unwrap().len(), 19);
    assert_eq!(std::fs::read_to_string(fx.p("doc.txt")).unwrap(), "version two, longer");

    // deleting on the host removes it from the mount
    std::fs::remove_file(fx.host_root.join("doc.txt")).unwrap();
    fx.settle();
    assert!(!fx.p("doc.txt").exists());
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cache_limit_evicts_clean_files_least_recently_used_first() {
    let mut fx = Fx::new(3 * MIB as u64, false).await;
    for i in 0..4u8 {
        fx.host_write(&format!("f{i}.bin"), &pattern(2 * MIB, i));
    }
    fx.settle();
    for i in 0..4u8 {
        assert_eq!(std::fs::read(fx.p(&format!("f{i}.bin"))).unwrap(), pattern(2 * MIB, i));
    }
    let s = fx.stats();
    assert!(s.cached_bytes <= 3 * MIB as u64, "{s:?}");
    // the evicted files are holes on disk again
    use std::os::unix::fs::MetadataExt;
    let on_disk: u64 = (0..4).map(|i| std::fs::metadata(fx.cache.join(format!("tree/f{i}.bin"))).unwrap().blocks() * 512).sum();
    assert!(on_disk <= 4 * MIB as u64, "allocated {on_disk}");
    assert!(fx.stats().fetched_bytes >= 8 * MIB as u64);
    // everything is still readable (from the client's cache or refetched), and the host was not touched
    for i in 0..4u8 {
        assert_eq!(std::fs::read(fx.p(&format!("f{i}.bin"))).unwrap(), pattern(2 * MIB, i));
    }
    fx.settle();
    for i in 0..4u8 {
        assert_eq!(std::fs::read(fx.host_root.join(format!("f{i}.bin"))).unwrap(), pattern(2 * MIB, i));
    }
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pinned_folders_are_kept_and_survive_eviction() {
    let mut fx = Fx::new(2 * MIB as u64, false).await;
    fx.host_write("keep/a.bin", &pattern(MIB + 5, 9));
    fx.host_write("keep/b.bin", &pattern(MIB + 7, 8));
    fx.host_write("other/c.bin", &pattern(2 * MIB, 7));
    fx.settle();
    let store = fx.session.as_ref().unwrap().store().clone();
    store.pin("keep").unwrap();
    wait_until("pinned files hydrated", || {
        let items = store.browse("keep").unwrap();
        items.len() == 2 && items.iter().all(|i| i.state == "cached" && i.pinned)
    });
    // reading something else cannot push the pinned files out
    assert_eq!(std::fs::read(fx.p("other/c.bin")).unwrap(), pattern(2 * MIB, 7));
    let items = store.browse("keep").unwrap();
    assert!(items.iter().all(|i| i.state == "cached"), "{items:?}");
    // a file that arrives later in a pinned folder is fetched at once
    fx.host_write("keep/late.bin", &pattern(1000, 3));
    fx.settle();
    wait_until("late file cached", || store.browse("keep").unwrap().iter().any(|i| i.name == "late.bin" && i.state == "cached"));
    store.unpin("keep").unwrap();
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_only_members_cannot_write() {
    let mut fx = Fx::new(0, true).await;
    fx.host_write("a.txt", b"read me");
    fx.settle();
    assert_eq!(std::fs::read_to_string(fx.p("a.txt")).unwrap(), "read me");
    let e = std::fs::write(fx.p("b.txt"), "no").unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc_erofs()), "{e}");
    assert!(std::fs::remove_file(fx.p("a.txt")).is_err());
    fx.stop().await;
}

fn libc_erofs() -> i32 {
    30
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crashed_mount_is_cleaned_up_and_the_folder_usable_again() {
    let fx = Fx::new(0, false).await;
    let point = fx.point.clone();
    assert!(medialake_mount::mountctl::mount_at(&point).is_some_and(|m| m.is_ours()));
    // simulate a crash: the session is gone without unmounting
    let session = fx.session;
    std::mem::forget(session);
    let gone = medialake_mount::mountctl::cleanup_stale_in(point.parent().unwrap());
    assert!(gone.iter().any(|p| p.ends_with("mnt")), "{gone:?}");
    assert!(medialake_mount::mountctl::mount_at(&point).is_none());
    // the folder is empty and a new session can mount on it again
    let (_, _) = (fx.engine.cursor(), &fx.tr);
    let cfg = MountConfig { sync_id: "s1".into(), name: "T".into(), point: point.clone(), cache_dir: fx.cache.clone(), read_only: false, limit_bytes: 0 };
    let again = MountSession::start(cfg, fx.tr.clone()).await.expect("mount again");
    assert!(again.is_mounted());
    again.stop().await;
    assert!(!Path::new(&point).join("x").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_non_empty_folder_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let point = base.join("mnt");
    std::fs::create_dir_all(&point).unwrap();
    std::fs::write(point.join("mine.txt"), "user data").unwrap();
    let hs = HostedSync::open(
        HostedConfig { id: "x".repeat(8), name: "T".into(), root: base.join("host"), store: None, backup: BackupPolicy::None, ignore: vec![] },
        None,
    );
    std::fs::create_dir_all(base.join("host")).unwrap();
    let hs = hs.or_else(|_| {
        HostedSync::open(HostedConfig { id: "y".repeat(8), name: "T".into(), root: base.join("host"), store: None, backup: BackupPolicy::None, ignore: vec![] }, None)
    });
    let hs = hs.unwrap();
    let tr: Arc<dyn Transport> = Arc::new(LocalTransport { host: hs, access: Access::ReadWrite, author: "Mac".into() });
    let r = MountSession::start(
        MountConfig { sync_id: "s".into(), name: "T".into(), point: point.clone(), cache_dir: base.join("cache"), read_only: false, limit_bytes: 0 },
        tr,
    )
    .await;
    let e = r.err().expect("must refuse");
    assert!(format!("{e:#}").contains("not empty"), "{e:#}");
    assert!(point.join("mine.txt").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn touching_a_placeholder_does_not_download_it() {
    let mut fx = Fx::new(0, false).await;
    fx.host_write("movie.bin", &pattern(4 * MIB, 4));
    fx.settle();
    // `touch`: only the modification time changes
    let t = std::time::SystemTime::now() - Duration::from_secs(3600);
    let f = std::fs::OpenOptions::new().write(true).open(fx.p("movie.bin")).unwrap();
    f.set_modified(t).unwrap();
    drop(f);
    fx.settle();
    assert_eq!(fx.stats().fetched_bytes, 0, "a touch must not hydrate");
    let m = std::fs::metadata(fx.p("movie.bin")).unwrap().modified().unwrap();
    assert!(m.duration_since(t).unwrap_or_default() < Duration::from_secs(2) || t.duration_since(m).unwrap_or_default() < Duration::from_secs(2));
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_conflicting_edit_keeps_both_versions() {
    let mut fx = Fx::new(0, false).await;
    fx.host_write("plan.txt", b"base text");
    fx.settle();
    assert_eq!(std::fs::read_to_string(fx.p("plan.txt")).unwrap(), "base text");

    // both sides change it before they meet
    std::fs::write(fx.p("plan.txt"), "edited on the Mac").unwrap();
    fx.host_write("plan.txt", b"edited on the host");
    fx.settle();

    assert_eq!(std::fs::read_to_string(fx.p("plan.txt")).unwrap(), "edited on the host");
    let copies: Vec<String> = std::fs::read_dir(&fx.point)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("conflict"))
        .collect();
    assert_eq!(copies.len(), 1, "{copies:?}");
    assert_eq!(std::fs::read_to_string(fx.p(&copies[0])).unwrap(), "edited on the Mac");
    // and the copy reached the host too
    assert_eq!(std::fs::read_to_string(fx.host_root.join(&copies[0])).unwrap(), "edited on the Mac");
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn offline_cached_files_work_and_the_rest_fails_cleanly() {
    let mut fx = Fx::new(0, false).await;
    fx.host_write("cached.txt", b"I am here");
    fx.host_write("cloud.bin", &pattern(2 * MIB, 5));
    fx.settle();
    assert_eq!(std::fs::read_to_string(fx.p("cached.txt")).unwrap(), "I am here");

    fx.offline.store(true, Ordering::SeqCst);
    // listing and metadata need no network
    assert_eq!(std::fs::metadata(fx.p("cloud.bin")).unwrap().len(), (2 * MIB) as u64);
    // the cached file opens and can be saved; the file that was never fetched gives an I/O error
    assert_eq!(std::fs::read_to_string(fx.p("cached.txt")).unwrap(), "I am here");
    std::fs::write(fx.p("cached.txt"), "edited offline").unwrap();
    let e = std::fs::read(fx.p("cloud.bin")).unwrap_err();
    assert!(matches!(e.raw_os_error(), Some(5) | Some(60)), "{e}");
    assert!(fx.engine.sync_once().is_err(), "no host, no round");

    // back online: the offline edit is uploaded
    fx.offline.store(false, Ordering::SeqCst);
    fx.settle();
    assert_eq!(std::fs::read_to_string(fx.host_root.join("cached.txt")).unwrap(), "edited offline");
    assert_eq!(std::fs::read(fx.p("cloud.bin")).unwrap(), pattern(2 * MIB, 5));
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_readers_get_identical_content() {
    let mut fx = Fx::new(0, false).await;
    let want = pattern(12 * MIB + 77, 6);
    fx.host_write("shared.bin", &want);
    fx.settle();
    let path = fx.p("shared.bin");
    let mut tasks = vec![];
    for k in 0..6usize {
        let (p, want) = (path.clone(), want.clone());
        tasks.push(tokio::task::spawn_blocking(move || {
            use std::io::{Read, Seek, SeekFrom};
            let mut f = std::fs::File::open(&p).unwrap();
            // different readers start in different places
            let start = (k * 2 * MIB) as u64;
            f.seek(SeekFrom::Start(start)).unwrap();
            let mut got = vec![];
            f.read_to_end(&mut got).unwrap();
            assert_eq!(got, &want[start as usize..]);
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    // each block was fetched about once, not once per reader
    assert!(fx.stats().fetched_bytes <= 2 * want.len() as u64, "{:?}", fx.stats());
    fx.stop().await;
}

/// The save patterns of AppKit applications, run by a real Cocoa process on the mount.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cocoa_save_patterns_work_on_the_mount() {
    if std::process::Command::new("swift").arg("--version").output().is_err() {
        eprintln!("swift not installed: skipped");
        return;
    }
    let mut fx = Fx::new(0, false).await;
    fx.settle();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("spike/saves.swift");
    let point = fx.point.clone();
    let out = tokio::task::spawn_blocking(move || std::process::Command::new("swift").arg(script).arg(point).output().unwrap()).await.unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    eprintln!("{text}");
    assert!(text.contains("ok   Data.write(.atomic)"), "{text}\n{}", String::from_utf8_lossy(&out.stderr));
    assert!(!text.lines().any(|l| l.starts_with("FAIL")), "{text}");
    // what the script left behind syncs without complaint
    fx.settle();
    assert!(fx.host_root.join("atomic.txt").exists());
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_registry_removes_what_a_dead_process_left_and_spares_a_live_one() {
    use medialake_mount::mountctl::{self, Registry};
    let fx = Fx::new(0, false).await;
    let point = fx.point.clone();
    let reg_file = fx.cache.parent().unwrap().join("mounts.json");
    let reg = Registry::new(reg_file.clone());

    // a process that is gone
    let mut dead = std::process::Command::new("true").spawn().unwrap();
    let dead_pid = dead.id();
    dead.wait().unwrap();
    std::fs::write(&reg_file, format!(r#"[{{"point":{:?},"pid":{dead_pid}}}]"#, point.display().to_string())).unwrap();
    let session = fx.session;
    // a live owner (this process) is spared
    reg.record(&point);
    assert!(reg.cleanup_dead().is_empty());
    assert!(mountctl::mount_at(&point).is_some());
    // the dead one is cleaned up
    std::fs::write(&reg_file, format!(r#"[{{"point":{:?},"pid":{dead_pid}}}]"#, point.display().to_string())).unwrap();
    let gone = reg.cleanup_dead();
    assert_eq!(gone.len(), 1, "{gone:?}");
    assert!(mountctl::mount_at(&point).is_none());
    drop(session);
}
