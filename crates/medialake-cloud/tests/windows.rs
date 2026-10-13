//! The Cloud Files frontend on the real platform, with the real sync engine and host: `tests/frontend.rs`
//! again, but Windows itself decides what a placeholder is and when content is fetched. Each test
//! registers a sync root in a temporary folder (NTFS) and unregisters it at the end. Windows only.
#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use medialake_cloud::{CloudConfig, CloudSession, leave_root_default};
use medialake_cloudapi::win::WinCloud;
use medialake_cloudapi::{CloudApi, FileState, Pin, attr};
use medialake_proto::sync::*;
use medialake_sync::host::config::{BackupPolicy, HostedConfig};
use medialake_sync::host::hosted::HostedSync;
use medialake_sync::member::engine::{ConflictPolicy, Engine, EngineConfig};
use medialake_sync::member::frontend::Frontend;
use medialake_sync::member::transport::{ChangesReply, Fetch, LocalTransport, RangeFetch, Transport, Upload};
use medialake_sync::util::{now_ms, set_mtime_ms};

const MIB: usize = 1 << 20;

/// The host in-process, counting what is asked of it: `ranged` is what Windows fetched for a reader,
/// `downloaded` what the engine fetched itself.
struct Counting {
    inner: LocalTransport,
    ranged: AtomicU64,
    downloads: AtomicU64,
    offline: Mutex<bool>,
}

impl Transport for Counting {
    fn changes(&self, e: &str, c: u64, l: u32) -> anyhow::Result<ChangesReply> {
        self.inner.changes(e, c, l)
    }
    fn propose(&self, c: &[Change]) -> anyhow::Result<Verdict> {
        self.inner.propose(c)
    }
    fn commit(&self, c: &[Change]) -> anyhow::Result<Vec<ChangeResult>> {
        self.inner.commit(c)
    }
    fn download(&self, p: &str, r: u64, part: &Path) -> anyhow::Result<Fetch> {
        self.downloads.fetch_add(1, Ordering::SeqCst);
        self.inner.download(p, r, part)
    }
    fn read_range(&self, p: &str, r: u64, o: u64, l: u64) -> anyhow::Result<RangeFetch> {
        if *self.offline.lock().unwrap() {
            anyhow::bail!("cannot connect to 10.0.0.2:47900: connection refused");
        }
        let res = self.inner.read_range(p, r, o, l)?;
        if let RangeFetch::Data(d) = &res {
            self.ranged.fetch_add(d.len() as u64, Ordering::SeqCst);
        }
        Ok(res)
    }
    fn upload(&self, h: &str, s: &Path, n: u64) -> anyhow::Result<Upload> {
        self.inner.upload(h, s, n)
    }
    fn versions(&self, p: &str) -> anyhow::Result<Vec<VersionInfo>> {
        self.inner.versions(p)
    }
    fn snapshots(&self) -> anyhow::Result<Vec<SnapshotInfo>> {
        self.inner.snapshots()
    }
    fn restore(&self, w: RestoreWhat, c: bool) -> anyhow::Result<Vec<ChangeResult>> {
        self.inner.restore(w, c)
    }
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| ((i * 31 + seed as usize) % 251) as u8).collect()
}

struct Fx {
    base: PathBuf,
    host_root: PathBuf,
    hs: Arc<HostedSync>,
    root: PathBuf,
    engine: Engine,
    session: Option<CloudSession>,
    tr: Arc<Counting>,
    access: Access,
    /// Last: it is deleted when everything that has files open in it is gone.
    _dir: tempfile::TempDir,
}

impl Fx {
    fn new() -> Fx {
        Fx::with(Access::ReadWrite)
    }

    fn with(access: Access) -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let host_root = base.join("host");
        std::fs::create_dir_all(&host_root).unwrap();
        let id = medialake_sync::util::random_id();
        let hs = HostedSync::open(
            HostedConfig { id: id.clone(), name: "Docs".into(), root: host_root.clone(), store: None, backup: BackupPolicy::None, ignore: vec![] },
            None,
        )
        .unwrap();
        let tr = Arc::new(Counting {
            inner: LocalTransport { host: hs.clone(), access, author: "PC".into() },
            ranged: AtomicU64::new(0),
            downloads: AtomicU64::new(0),
            offline: Mutex::new(false),
        });
        let root = base.join("MediaLake").join("Docs");
        let session = CloudSession::start(
            CloudConfig { sync_id: id, name: "Docs".into(), root: root.clone(), read_only: access == Access::ReadOnly },
            tr.clone(),
        )
        .expect("a sync root on the temp folder");
        let root = session.root().to_path_buf();
        let engine = Fx::engine(&base.join("state"), session.frontend(), tr.clone(), access);
        Fx { _dir: dir, base, host_root, hs, root, engine, session: Some(session), tr, access }
    }

    fn engine(state: &Path, fe: Arc<dyn Frontend>, tr: Arc<dyn Transport>, access: Access) -> Engine {
        let cfg = EngineConfig { device: "PC".into(), policy: ConflictPolicy::ServerWins, access, state_dir: state.to_path_buf(), settle_ms: 0, ignore: vec![] };
        Engine::new(cfg, fe, tr).unwrap()
    }

    fn host_write(&self, rel: &str, bytes: &[u8]) {
        let p = self.host_root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, bytes).unwrap();
        set_mtime_ms(&p, now_ms() - 600_000).unwrap();
    }

    fn settle(&mut self) {
        let mut last = vec![];
        for _ in 0..12 {
            let mut activity = self.hs.scan().unwrap();
            let r = self.engine.sync_once().unwrap();
            activity += r.downloaded + r.pushed + r.uploaded + r.removed_local + r.conflicts;
            if !r.retry && activity == 0 {
                return;
            }
            last = r.errors;
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("did not settle: {last:?}");
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn state(&self, rel: &str) -> FileState {
        WinCloud.state(&self.p(rel)).unwrap()
    }

    fn ranged(&self) -> u64 {
        self.tr.ranged.load(Ordering::SeqCst)
    }

    /// Waits for something that happens on another thread (the pin worker).
    fn eventually(&self, what: &str, f: impl Fn() -> bool) {
        let until = Instant::now() + Duration::from_secs(15);
        while Instant::now() < until {
            if f() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("never happened: {what}");
    }
}

impl Drop for Fx {
    fn drop(&mut self) {
        if let Some(s) = self.session.take() {
            let _ = s.leave(false);
        }
    }
}

#[test]
fn the_folder_fills_with_placeholders_and_nothing_is_fetched() {
    let mut fx = Fx::new();
    fx.host_write("hello.txt", b"hello from the host");
    fx.host_write("big.bin", &pattern(3 * MIB + 17, 1));
    fx.host_write("docs/a.md", b"# a");
    fx.host_write("docs/deep/er/b.md", b"# b");
    fx.host_write("empty.txt", b"");
    fx.settle();

    // names, sizes, times and folders are there
    let md = std::fs::metadata(fx.p("big.bin")).unwrap();
    assert_eq!(md.len(), (3 * MIB + 17) as u64);
    assert_eq!(medialake_sync::util::mtime_ms(&md), fx.hs.entry("big.bin").unwrap().mtime);
    assert!(fx.p("docs/deep/er").is_dir());
    let st = fx.state("big.bin");
    assert!(st.placeholder && st.in_sync && st.dehydrated, "{st:?}");
    assert!(attr::would_download(st.attributes));
    // ... and the identity says which revision and content each stands for
    let id = st.identity.expect("identity");
    assert_eq!(id.hash, fx.hs.entry("big.bin").unwrap().hash);
    assert_eq!(id.rev, fx.hs.entry("big.bin").unwrap().rev);
    assert!(fx.state("docs/deep/er/b.md").dehydrated);
    // the host was not asked for any content: not by the engine, not by the scan, not by Windows
    assert_eq!(fx.ranged(), 0);
    assert_eq!(fx.tr.downloads.load(Ordering::SeqCst), 0);
    // a second look changes nothing and fetches nothing
    fx.settle();
    assert_eq!(fx.ranged(), 0);

    // opening a file is what fetches it
    assert_eq!(std::fs::read(fx.p("hello.txt")).unwrap(), b"hello from the host");
    assert_eq!(fx.ranged(), 19);
    assert_eq!(std::fs::read(fx.p("big.bin")).unwrap(), pattern(3 * MIB + 17, 1));
    assert_eq!(std::fs::read(fx.p("empty.txt")).unwrap(), b"");
    let st = fx.state("big.bin");
    assert!(st.placeholder && st.in_sync && !st.dehydrated, "{st:?}");
    // a read leaves nothing for the sync to do
    let rev = fx.hs.seq();
    fx.settle();
    assert_eq!(fx.hs.seq(), rev, "reading must not look like a change");

    let fe = fx.session.as_ref().unwrap().frontend();
    let listing = fe.browse("").unwrap();
    let state_of = |n: &str| listing.iter().find(|e| e.name == n).map(|e| e.state).unwrap();
    assert_eq!(state_of("big.bin"), "cached");
    assert_eq!(state_of("docs"), "dir");
    assert_eq!(fe.browse("docs").unwrap().iter().find(|e| e.name == "a.md").unwrap().state, "cloud");
}

#[test]
fn a_lost_state_database_does_not_download_the_sync() {
    let mut fx = Fx::new();
    fx.host_write("a.bin", &pattern(2 * MIB, 3));
    fx.host_write("b.bin", &pattern(2 * MIB, 4));
    fx.settle();
    assert_eq!(fx.ranged(), 0);

    // a new engine, no memory of anything: the placeholders still say what they are
    let fe = fx.session.as_ref().unwrap().frontend();
    let mut fresh = Fx::engine(&fx.base.join("state2"), fe, fx.tr.clone(), Access::ReadWrite);
    for _ in 0..4 {
        fx.hs.scan().unwrap();
        fresh.sync_once().unwrap();
    }
    assert_eq!(fx.ranged(), 0, "re-adopting clean placeholders must not read them");
    assert_eq!(fx.tr.downloads.load(Ordering::SeqCst), 0);
    assert!(fx.state("a.bin").dehydrated);
}

#[test]
fn an_edit_is_pushed_and_the_file_stays_a_synced_placeholder() {
    let mut fx = Fx::new();
    fx.host_write("note.txt", b"version one");
    fx.settle();
    // an application changes the file: Windows fetches it first, then marks it as not in sync
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new().read(true).write(true).open(fx.p("note.txt")).unwrap();
        f.seek(SeekFrom::End(0)).unwrap();
        f.write_all(b", then edited on this PC").unwrap();
    }
    let st = fx.state("note.txt");
    assert!(st.placeholder && !st.in_sync && !st.dehydrated, "{st:?}");
    set_mtime_ms(&fx.p("note.txt"), now_ms() - 600_000).unwrap();
    fx.settle();
    assert_eq!(std::fs::read(fx.host_root.join("note.txt")).unwrap(), b"version one, then edited on this PC");
    let st = fx.state("note.txt");
    assert!(st.placeholder && st.in_sync, "{st:?}");
    assert_eq!(st.identity.unwrap().hash, fx.hs.entry("note.txt").unwrap().hash);

    // and the pushed file can be freed and comes back with the new content
    WinCloud.dehydrate(&fx.p("note.txt")).unwrap();
    assert!(fx.state("note.txt").dehydrated);
    assert_eq!(std::fs::read(fx.p("note.txt")).unwrap(), b"version one, then edited on this PC");
}

#[test]
fn a_new_local_file_is_pushed_and_becomes_a_placeholder() {
    let mut fx = Fx::new();
    fx.settle();
    std::fs::create_dir_all(fx.p("new folder")).unwrap();
    std::fs::write(fx.p("new folder/mine.txt"), b"made on this PC").unwrap();
    set_mtime_ms(&fx.p("new folder/mine.txt"), now_ms() - 600_000).unwrap();
    fx.settle();
    assert_eq!(std::fs::read(fx.host_root.join("new folder/mine.txt")).unwrap(), b"made on this PC");
    let st = fx.state("new folder/mine.txt");
    assert!(st.placeholder && st.in_sync && !st.dehydrated, "converted so that Explorer can free it: {st:?}");
    // "Free up space" then works on it, and it is fetched again when read
    WinCloud.dehydrate(&fx.p("new folder/mine.txt")).unwrap();
    assert_eq!(std::fs::read(fx.p("new folder/mine.txt")).unwrap(), b"made on this PC");
    let before = fx.hs.seq();
    fx.settle();
    assert_eq!(fx.hs.seq(), before);
}

#[test]
fn a_remote_change_replaces_a_clean_placeholder_and_frees_its_content() {
    let mut fx = Fx::new();
    fx.host_write("doc.txt", b"first");
    fx.host_write("never-read.txt", b"one");
    fx.settle();
    assert_eq!(std::fs::read(fx.p("doc.txt")).unwrap(), b"first");
    let before = fx.ranged();

    fx.host_write("doc.txt", b"second, longer");
    fx.host_write("never-read.txt", b"two, also longer");
    fx.settle();
    // the update did not fetch; the content is gone again until it is read
    assert_eq!(fx.ranged(), before);
    assert_eq!(fx.tr.downloads.load(Ordering::SeqCst), 0);
    let st = fx.state("doc.txt");
    assert!(st.dehydrated && st.in_sync, "{st:?}");
    assert_eq!(std::fs::metadata(fx.p("doc.txt")).unwrap().len(), 14);
    assert_eq!(std::fs::read(fx.p("doc.txt")).unwrap(), b"second, longer");
    assert_eq!(std::fs::read(fx.p("never-read.txt")).unwrap(), b"two, also longer");
}

#[test]
fn a_pinned_file_is_kept_up_to_date_with_its_content() {
    let mut fx = Fx::new();
    fx.host_write("keep.bin", &pattern(MIB, 5));
    fx.host_write("album/one.bin", &pattern(70_000, 6));
    fx.settle();
    let fe = fx.session.as_ref().unwrap().frontend();
    // "Always keep on this device" on a file and on a folder: the content comes without anyone reading it
    fe.pin("keep.bin").unwrap();
    fe.pin("album").unwrap();
    fx.eventually("pinned files are fetched", || !fx.state("keep.bin").dehydrated && !fx.state("album/one.bin").dehydrated);
    assert_eq!(fx.state("keep.bin").pin, Pin::Pinned);

    fx.host_write("keep.bin", &pattern(MIB + 100, 7));
    fx.host_write("album/two.bin", &pattern(50_000, 8));
    fx.settle();
    // pinned: the engine downloaded the new version at once, and what is new in a pinned folder too
    assert_eq!(std::fs::read(fx.p("keep.bin")).unwrap(), pattern(MIB + 100, 7));
    let st = fx.state("keep.bin");
    assert!(!st.dehydrated && st.in_sync && st.placeholder, "{st:?}");
    assert!(!fx.state("album/two.bin").dehydrated, "new in a pinned folder");

    // "Free up space"
    fe.unpin("keep.bin", true).unwrap();
    fx.eventually("freed", || fx.state("keep.bin").dehydrated);
    assert_eq!(std::fs::read(fx.p("keep.bin")).unwrap(), pattern(MIB + 100, 7));
}

#[test]
fn explorer_pins_are_noticed_by_the_watcher() {
    let mut fx = Fx::new();
    fx.host_write("a.bin", &pattern(40_000, 9));
    fx.settle();
    let fe = fx.session.as_ref().unwrap().frontend();
    let _watch = fe.watch(Duration::from_millis(100), Box::new(|_| {})).expect("a watcher");
    std::thread::sleep(Duration::from_millis(300));
    // what Explorer's "Always keep on this device" does: it sets attributes and tells nobody
    let path = fx.p("a.bin").to_string_lossy().trim_start_matches(r"\\?\").to_string();
    assert!(std::process::Command::new("attrib").args(["+P", "-U", &path]).status().unwrap().success());
    fx.eventually("the pinned file is fetched", || !fx.state("a.bin").dehydrated);
    assert!(std::process::Command::new("attrib").args(["-P", "+U", &path]).status().unwrap().success());
    fx.eventually("the unpinned file is freed", || fx.state("a.bin").dehydrated);
}

#[test]
fn renames_and_deletes_travel_in_both_directions() {
    let mut fx = Fx::new();
    fx.host_write("a.txt", b"a");
    fx.host_write("old/b.txt", b"bee");
    fx.host_write("old/c.txt", b"sea");
    fx.host_write("dir/x.txt", b"x");
    fx.settle();

    // a placeholder that was never opened is renamed and another deleted: nothing is fetched for it
    std::fs::rename(fx.p("a.txt"), fx.p("renamed.txt")).unwrap();
    std::fs::remove_file(fx.p("old/b.txt")).unwrap();
    std::fs::rename(fx.p("dir"), fx.p("moved dir")).unwrap();
    // Windows takes the "in sync" mark off what was renamed; the content is still not here
    let st = fx.state("renamed.txt");
    assert!(st.placeholder && st.dehydrated && !st.in_sync, "{st:?}");
    fx.settle();
    assert_eq!(fx.ranged(), 0, "a rename is not a read");
    assert_eq!(std::fs::read(fx.host_root.join("renamed.txt")).unwrap(), b"a", "the host must not lose a renamed file");
    assert!(!fx.host_root.join("a.txt").exists());
    assert!(!fx.host_root.join("old/b.txt").exists());
    assert!(fx.host_root.join("moved dir/x.txt").exists() && !fx.host_root.join("dir").exists());
    // the move kept the history: one entry that changed its name, not a delete and a new file
    assert!(fx.hs.entry("renamed.txt").unwrap().rev > fx.hs.entry("old/c.txt").unwrap().rev);
    let st = fx.state("renamed.txt");
    assert!(st.in_sync && st.dehydrated, "{st:?}");
    assert_eq!(st.identity.unwrap().rev, fx.hs.entry("renamed.txt").unwrap().rev);
    // and under its new name it opens
    assert_eq!(std::fs::read(fx.p("renamed.txt")).unwrap(), b"a");
    assert_eq!(std::fs::read(fx.p("moved dir/x.txt")).unwrap(), b"x");
    let read = fx.ranged();

    std::fs::remove_file(fx.host_root.join("renamed.txt")).unwrap();
    std::fs::rename(fx.host_root.join("old/c.txt"), fx.host_root.join("old/d.txt")).unwrap();
    std::fs::remove_dir_all(fx.host_root.join("moved dir")).unwrap();
    fx.settle();
    assert!(!fx.p("renamed.txt").exists());
    assert!(!fx.p("old/c.txt").exists() && fx.state("old/d.txt").dehydrated);
    assert!(!fx.p("moved dir").exists());
    assert_eq!(fx.ranged(), read);
}

#[test]
fn cutting_a_file_short_without_opening_it_first_is_pushed() {
    let mut fx = Fx::new();
    fx.host_write("long.txt", b"0123456789");
    fx.host_write("touched.txt", b"same as before");
    fx.settle();
    // a program truncates a file whose content is not here, and another only gets a new time
    std::fs::OpenOptions::new().write(true).open(fx.p("long.txt")).unwrap().set_len(4).unwrap();
    set_mtime_ms(&fx.p("long.txt"), now_ms() - 500_000).unwrap();
    set_mtime_ms(&fx.p("touched.txt"), now_ms() - 500_000).unwrap();
    let rev = fx.hs.entry("touched.txt").unwrap().rev;
    fx.settle();
    assert_eq!(std::fs::read(fx.host_root.join("long.txt")).unwrap(), b"0123");
    assert_eq!(std::fs::read(fx.p("long.txt")).unwrap(), b"0123");
    assert_eq!(fx.hs.entry("touched.txt").unwrap().rev, rev, "a new time alone is not a change");
    assert!(fx.state("touched.txt").dehydrated, "and it downloads nothing");
}

#[test]
fn paths_longer_than_260_characters_work() {
    let mut fx = Fx::new();
    let deep = (0..8).map(|i| format!("a folder with a rather long name {i:02}")).collect::<Vec<_>>().join("/");
    let rel = format!("{deep}/the file at the bottom.txt");
    assert!(fx.p(&rel).as_os_str().len() > 300);
    fx.host_write(&rel, b"deep down");
    fx.settle();
    assert!(fx.state(&rel).dehydrated);
    assert_eq!(std::fs::read(fx.p(&rel)).unwrap(), b"deep down");
    // and upwards
    let mine = format!("{deep}/made here.txt");
    std::fs::write(fx.p(&mine), b"from below").unwrap();
    set_mtime_ms(&fx.p(&mine), now_ms() - 600_000).unwrap();
    fx.settle();
    assert_eq!(std::fs::read(fx.host_root.join(&mine)).unwrap(), b"from below");
    assert!(fx.state(&mine).in_sync);
}

#[test]
fn an_unreachable_host_fails_the_open_and_recovers() {
    let mut fx = Fx::new();
    fx.host_write("a.txt", b"content");
    fx.settle();
    *fx.tr.offline.lock().unwrap() = true;
    let started = Instant::now();
    let e = std::fs::read(fx.p("a.txt")).expect_err("the host is away");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(e.raw_os_error(), Some(388), "network unavailable: {e}");
    *fx.tr.offline.lock().unwrap() = false;
    assert_eq!(std::fs::read(fx.p("a.txt")).unwrap(), b"content");
}

#[test]
fn a_read_only_member_gets_read_only_files_and_edits_are_reverted() {
    let mut fx = Fx::with(Access::ReadOnly);
    fx.host_write("doc.txt", b"from the host");
    fx.host_write("other.txt", b"untouched");
    fx.settle();
    let st = fx.state("doc.txt");
    assert!(st.attributes & attr::READONLY != 0 && st.dehydrated, "{st:?}");
    assert_eq!(std::fs::read(fx.p("doc.txt")).unwrap(), b"from the host");
    assert!(std::fs::write(fx.p("doc.txt"), b"no").is_err(), "the attribute stops ordinary programs");

    // a program that clears the attribute and writes anyway: the host's version comes back and the edit
    // is kept next to it
    let mut perms = std::fs::metadata(fx.p("doc.txt")).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(fx.p("doc.txt"), perms).unwrap();
    std::fs::write(fx.p("doc.txt"), b"edited although read-only").unwrap();
    set_mtime_ms(&fx.p("doc.txt"), now_ms() - 600_000).unwrap();
    fx.settle();
    assert_eq!(std::fs::read(fx.p("doc.txt")).unwrap(), b"from the host");
    assert_eq!(std::fs::read(fx.host_root.join("doc.txt")).unwrap(), b"from the host");
    let copies: Vec<String> =
        std::fs::read_dir(&fx.root).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.contains("conflict")).collect();
    assert_eq!(copies.len(), 1, "{copies:?}");
    assert_eq!(std::fs::read(fx.p(&copies[0])).unwrap(), b"edited although read-only");
    // a change on the host still arrives over a read-only placeholder
    fx.host_write("other.txt", b"changed on the host");
    fx.settle();
    assert_eq!(std::fs::read(fx.p("other.txt")).unwrap(), b"changed on the host");
    assert_eq!(fx.access, Access::ReadOnly);
}

#[test]
fn leaving_keeps_what_was_downloaded_and_removes_placeholders() {
    let mut fx = Fx::new();
    fx.host_write("seen.txt", b"I was opened");
    fx.host_write("unseen.bin", &pattern(MIB, 7));
    fx.host_write("dir/seen too.txt", b"me too");
    fx.host_write("gone/unseen.txt", b"never opened");
    fx.settle();
    assert_eq!(std::fs::read(fx.p("seen.txt")).unwrap(), b"I was opened");
    assert_eq!(std::fs::read(fx.p("dir/seen too.txt")).unwrap(), b"me too");

    let root = fx.root.clone();
    fx.session.take().unwrap().leave(true).unwrap();
    assert_eq!(std::fs::read(root.join("seen.txt")).unwrap(), b"I was opened");
    assert_eq!(std::fs::read(root.join("dir/seen too.txt")).unwrap(), b"me too");
    assert!(!root.join("unseen.bin").exists(), "a placeholder without content cannot stay");
    assert!(!root.join("gone/unseen.txt").exists());
    let st = WinCloud.state(&root.join("seen.txt")).unwrap();
    assert!(st.exists && !st.placeholder, "an ordinary file again: {st:?}");
    // the folder is an ordinary folder again: nothing of ours is left in it, and a file made in it stays plain
    let mut left: Vec<String> = std::fs::read_dir(&root).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    left.sort();
    assert_eq!(left, ["dir", "seen.txt"]);
    std::fs::write(root.join("after.txt"), b"x").unwrap();
    assert!(!WinCloud.state(&root.join("after.txt")).unwrap().placeholder);
}

#[test]
fn leaving_without_keeping_removes_everything() {
    let mut fx = Fx::new();
    fx.host_write("seen.txt", b"I was opened");
    fx.host_write("d/unseen.txt", b"x");
    fx.settle();
    std::fs::read(fx.p("seen.txt")).unwrap();
    let root = fx.root.clone();
    fx.session.take().unwrap().leave(false).unwrap();
    assert!(!root.join("seen.txt").exists() && !root.join("d").exists());
}

#[test]
fn a_stopped_session_comes_back_with_its_placeholders() {
    let mut fx = Fx::new();
    fx.host_write("a.bin", &pattern(100_000, 2));
    fx.host_write("read.txt", b"was read");
    fx.settle();
    assert_eq!(std::fs::read(fx.p("read.txt")).unwrap(), b"was read");
    let (id, root) = (fx.hs.id().to_string(), fx.root.clone());

    // the app quits: placeholders stay, opening one fails at once
    fx.session.take().unwrap().stop();
    assert!(std::fs::read(root.join("a.bin")).is_err());
    assert_eq!(std::fs::read(root.join("read.txt")).unwrap(), b"was read", "what is here stays readable");

    // the app starts again on the same folder (it is ours: not refused as non-empty)
    let session = CloudSession::start(CloudConfig { sync_id: id, name: "Docs".into(), root: root.clone(), read_only: false }, fx.tr.clone()).expect("restart");
    fx.engine = Fx::engine(&fx.base.join("state"), session.frontend(), fx.tr.clone(), Access::ReadWrite);
    fx.session = Some(session);
    let before = fx.ranged();
    fx.settle();
    assert_eq!(fx.ranged(), before, "a restart fetches nothing");
    assert_eq!(std::fs::read(root.join("a.bin")).unwrap(), pattern(100_000, 2));
}

#[test]
fn uninstall_unregisters_a_root_that_is_not_connected() {
    let mut fx = Fx::new();
    fx.host_write("seen.txt", b"kept");
    fx.host_write("unseen.txt", b"dropped");
    fx.settle();
    std::fs::read(fx.p("seen.txt")).unwrap();
    let root = fx.root.clone();
    fx.session.take().unwrap().stop();
    leave_root_default(&root, true).unwrap();
    assert_eq!(std::fs::read(root.join("seen.txt")).unwrap(), b"kept");
    assert!(!root.join("unseen.txt").exists());
    assert!(!WinCloud.state(&root.join("seen.txt")).unwrap().placeholder);
}
