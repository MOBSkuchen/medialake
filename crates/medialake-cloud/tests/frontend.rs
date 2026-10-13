//! The Cloud Files frontend against the real sync engine and host, over a small stand-in for the platform
//! (placeholders are sparse files, "hydration" is a read that calls the provider). What this proves is the
//! contract between the engine and the frontend: nothing is read or downloaded that was not asked for.
//! What it cannot prove is how Windows behaves: see `docs/w0-cloud-files.md`.

use std::collections::HashMap;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use medialake_cloud::{CloudConfig, CloudSession, leave_root};
use medialake_cloudapi::*;
use medialake_proto::sync::Access;
use medialake_sync::host::config::{BackupPolicy, HostedConfig};
use medialake_sync::host::hosted::HostedSync;
use medialake_sync::member::engine::{ConflictPolicy, Engine, EngineConfig};
use medialake_sync::member::frontend::Frontend;
use medialake_sync::member::transport::{LocalTransport, Transport};
use medialake_sync::util::{mtime_ms, now_ms, set_mtime_ms};

const MIB: usize = 1 << 20;

// ---------------- the stand-in platform ----------------

#[derive(Clone)]
struct Entry {
    placeholder: bool,
    in_sync: bool,
    dehydrated: bool,
    pin: Pin,
    identity: Option<Identity>,
}

struct Fake {
    root: Mutex<PathBuf>,
    files: Mutex<HashMap<PathBuf, Entry>>,
    provider: Mutex<Option<Arc<dyn Provider>>>,
    fetches: AtomicU64,
    fetched: Arc<AtomicU64>,
    unregistered: Mutex<Vec<PathBuf>>,
}

struct Conn;
impl Connection for Conn {}

struct FileOut {
    file: Mutex<std::fs::File>,
    fetched: Arc<AtomicU64>,
}

impl Transfer for FileOut {
    fn data(&self, offset: u64, bytes: &[u8]) -> CloudResult<()> {
        assert!(offset % ALIGN == 0, "unaligned transfer at {offset}");
        let mut f = self.file.lock().unwrap();
        f.seek(SeekFrom::Start(offset)).unwrap();
        f.write_all(bytes).unwrap();
        self.fetched.fetch_add(bytes.len() as u64, Ordering::SeqCst);
        Ok(())
    }
    fn progress(&self, _total: u64, _done: u64) {}
}

impl Fake {
    fn new() -> Arc<Fake> {
        Arc::new(Fake {
            root: Mutex::new(PathBuf::new()),
            files: Mutex::new(HashMap::new()),
            provider: Mutex::new(None),
            fetches: AtomicU64::new(0),
            fetched: Arc::new(AtomicU64::new(0)),
            unregistered: Mutex::new(vec![]),
        })
    }

    fn rel(&self, p: &Path) -> String {
        p.strip_prefix(&*self.root.lock().unwrap()).unwrap().to_string_lossy().replace('\\', "/")
    }

    /// What the platform does when something reads a dehydrated placeholder.
    fn hydrate_now(&self, path: &Path) -> CloudResult<()> {
        let e = self.files.lock().unwrap().get(path).cloned();
        let Some(e) = e else { return Ok(()) };
        if !e.dehydrated {
            return Ok(());
        }
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let mtime = mtime_ms(&std::fs::metadata(path).unwrap());
        let req = FetchRequest { rel: self.rel(path), identity: e.identity.clone(), file_size: size, offset: 0, length: size, process: String::new() };
        let out = FileOut { file: Mutex::new(std::fs::OpenOptions::new().write(true).open(path).unwrap()), fetched: self.fetched.clone() };
        let provider = self.provider.lock().unwrap().clone().expect("connected");
        self.fetches.fetch_add(1, Ordering::SeqCst);
        provider.fetch(&req, &out)?;
        drop(out);
        set_mtime_ms(path, mtime).unwrap();
        self.files.lock().unwrap().get_mut(path).unwrap().dehydrated = false;
        Ok(())
    }

    /// An application reading the file.
    fn read(&self, path: &Path) -> Vec<u8> {
        self.hydrate_now(path).expect("hydration");
        std::fs::read(path).unwrap()
    }

    /// An application changing a hydrated file: the platform marks it as not in sync.
    fn edit(&self, path: &Path, content: &[u8]) {
        self.hydrate_now(path).unwrap();
        std::fs::write(path, content).unwrap();
        if let Some(e) = self.files.lock().unwrap().get_mut(path) {
            e.in_sync = false;
        }
    }

    fn entry(&self, path: &Path) -> Option<Entry> {
        self.files.lock().unwrap().get(path).cloned()
    }
}

impl CloudApi for Fake {
    fn available(&self, _root: &Path) -> Result<(), String> {
        Ok(())
    }

    fn register(&self, root: &Path, _reg: &Registration) -> CloudResult<()> {
        *self.root.lock().unwrap() = root.to_path_buf();
        Ok(())
    }

    fn connect(&self, root: &Path, provider: Arc<dyn Provider>) -> CloudResult<Box<dyn Connection>> {
        *self.root.lock().unwrap() = root.to_path_buf();
        *self.provider.lock().unwrap() = Some(provider);
        Ok(Box::new(Conn))
    }

    fn unregister(&self, root: &Path) -> CloudResult<()> {
        self.unregistered.lock().unwrap().push(root.to_path_buf());
        Ok(())
    }

    fn create_placeholder(&self, parent: &Path, name: &str, info: &PlaceholderInfo) -> CloudResult<()> {
        let p = parent.join(name);
        if info.is_dir {
            std::fs::create_dir(&p).map_err(|e| CloudError::Failed(e.to_string()))?;
        } else {
            let f = std::fs::File::create(&p).map_err(|e| CloudError::Failed(e.to_string()))?;
            f.set_len(info.size).unwrap();
            drop(f);
            set_mtime_ms(&p, info.mtime_ms).unwrap();
        }
        self.files.lock().unwrap().insert(
            p,
            Entry { placeholder: true, in_sync: info.in_sync, dehydrated: !info.is_dir, pin: Pin::Unspecified, identity: info.identity.clone() },
        );
        Ok(())
    }

    fn update_placeholder(&self, path: &Path, info: &PlaceholderInfo, dehydrate: bool) -> CloudResult<()> {
        let f = std::fs::OpenOptions::new().write(true).open(path).map_err(|e| CloudError::Failed(e.to_string()))?;
        if dehydrate {
            f.set_len(0).unwrap();
        }
        f.set_len(info.size).unwrap();
        drop(f);
        set_mtime_ms(path, info.mtime_ms).unwrap();
        let mut files = self.files.lock().unwrap();
        let e = files.entry(path.to_path_buf()).or_insert(Entry { placeholder: true, in_sync: true, dehydrated: false, pin: Pin::Unspecified, identity: None });
        e.identity = info.identity.clone();
        e.in_sync = info.in_sync;
        if dehydrate {
            e.dehydrated = true;
        }
        Ok(())
    }

    fn mark_synced(&self, path: &Path, identity: &Identity) -> CloudResult<()> {
        let mut files = self.files.lock().unwrap();
        let e = files.entry(path.to_path_buf()).or_insert(Entry { placeholder: false, in_sync: false, dehydrated: false, pin: Pin::Unspecified, identity: None });
        e.placeholder = true;
        e.in_sync = true;
        e.identity = Some(identity.clone());
        Ok(())
    }

    fn state(&self, path: &Path) -> CloudResult<FileState> {
        let Ok(md) = std::fs::metadata(path) else { return Ok(FileState::default()) };
        let e = self.files.lock().unwrap().get(path).cloned();
        Ok(match e {
            Some(e) => FileState {
                exists: true,
                is_dir: md.is_dir(),
                placeholder: e.placeholder,
                in_sync: e.in_sync,
                dehydrated: e.dehydrated,
                pin: e.pin,
                identity: e.identity,
                size: md.len(),
                attributes: 0,
            },
            None => FileState { exists: true, is_dir: md.is_dir(), size: md.len(), ..Default::default() },
        })
    }

    fn hydrate(&self, path: &Path) -> CloudResult<()> {
        self.hydrate_now(path)
    }

    fn dehydrate(&self, path: &Path) -> CloudResult<()> {
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let mtime = mtime_ms(&std::fs::metadata(path).unwrap());
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_len(0).unwrap();
        f.set_len(size).unwrap();
        drop(f);
        set_mtime_ms(path, mtime).unwrap();
        self.files.lock().unwrap().get_mut(path).unwrap().dehydrated = true;
        Ok(())
    }

    fn set_pin(&self, path: &Path, pin: Pin) -> CloudResult<()> {
        self.files.lock().unwrap().entry(path.to_path_buf()).or_insert(Entry { placeholder: false, in_sync: false, dehydrated: false, pin: Pin::Unspecified, identity: None }).pin = pin;
        Ok(())
    }

    fn revert(&self, path: &Path) -> CloudResult<()> {
        self.files.lock().unwrap().remove(path);
        Ok(())
    }
}

// ---------------- fixture ----------------

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| ((i * 31 + seed as usize) % 251) as u8).collect()
}

struct Fx {
    _dir: tempfile::TempDir,
    base: PathBuf,
    host_root: PathBuf,
    hs: Arc<HostedSync>,
    root: PathBuf,
    fake: Arc<Fake>,
    engine: Engine,
    session: Option<CloudSession>,
    tr: Arc<dyn Transport>,
}

impl Fx {
    fn new() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let host_root = base.join("host");
        std::fs::create_dir_all(&host_root).unwrap();
        let hs = HostedSync::open(
            HostedConfig { id: "c".repeat(32), name: "Docs".into(), root: host_root.clone(), store: None, backup: BackupPolicy::None, ignore: vec![] },
            None,
        )
        .unwrap();
        let tr: Arc<dyn Transport> = Arc::new(LocalTransport { host: hs.clone(), access: Access::ReadWrite, author: "PC".into() });
        let root = base.join("MediaLake").join("Docs");
        let fake = Fake::new();
        let session = CloudSession::start_with(
            fake.clone(),
            CloudConfig { sync_id: "c".repeat(32), name: "Docs".into(), root: root.clone(), read_only: false },
            tr.clone(),
        )
        .expect("session");
        let engine = Fx::engine(&base.join("state"), session.frontend(), tr.clone());
        Fx { _dir: dir, base, host_root, hs, root, fake, engine, session: Some(session), tr }
    }

    fn engine(state: &Path, fe: Arc<dyn Frontend>, tr: Arc<dyn Transport>) -> Engine {
        let cfg = EngineConfig {
            device: "PC".into(),
            policy: ConflictPolicy::ServerWins,
            access: Access::ReadWrite,
            state_dir: state.to_path_buf(),
            settle_ms: 0,
            ignore: vec![],
        };
        Engine::new(cfg, fe, tr).unwrap()
    }

    fn host_write(&self, rel: &str, bytes: &[u8]) {
        let p = self.host_root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, bytes).unwrap();
        set_mtime_ms(&p, now_ms() - 600_000).unwrap();
    }

    fn settle(&mut self) {
        for _ in 0..12 {
            let mut activity = self.hs.scan().unwrap();
            let r = self.engine.sync_once().unwrap();
            activity += r.downloaded + r.pushed + r.uploaded + r.removed_local + r.conflicts;
            if !r.retry && activity == 0 {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("did not settle");
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn fetched(&self) -> u64 {
        self.fake.fetched.load(Ordering::SeqCst)
    }
}

// ---------------- tests ----------------

#[test]
fn the_folder_fills_with_placeholders_and_nothing_is_fetched() {
    let mut fx = Fx::new();
    fx.host_write("hello.txt", b"hello from the host");
    fx.host_write("big.bin", &pattern(3 * MIB + 17, 1));
    fx.host_write("docs/a.md", b"# a");
    fx.settle();

    // names, sizes and folders are there
    assert_eq!(std::fs::metadata(fx.p("big.bin")).unwrap().len(), (3 * MIB + 17) as u64);
    assert!(fx.p("docs").is_dir());
    let st = fx.fake.state(&fx.p("big.bin")).unwrap();
    assert!(st.placeholder && st.in_sync && st.dehydrated, "{st:?}");
    // ... and the identity says which revision and content each stands for
    let id = st.identity.expect("identity");
    assert_eq!(id.hash, fx.hs.entry("big.bin").unwrap().hash);
    assert_eq!(id.rev, fx.hs.entry("big.bin").unwrap().rev);
    // the host was not asked for any content: not by the engine, not by the scan
    assert_eq!(fx.fake.fetches.load(Ordering::SeqCst), 0);
    assert_eq!(fx.fetched(), 0);

    // opening a file is what fetches it
    assert_eq!(fx.fake.read(&fx.p("hello.txt")), b"hello from the host");
    assert_eq!(fx.fake.fetches.load(Ordering::SeqCst), 1);
    assert_eq!(fx.fake.read(&fx.p("big.bin")), pattern(3 * MIB + 17, 1));
    // a read leaves nothing for the sync to do
    fx.settle();
    assert_eq!(fx.hs.entry("big.bin").unwrap().rev, id.rev);
}

#[test]
fn a_lost_state_database_does_not_download_the_sync() {
    let mut fx = Fx::new();
    fx.host_write("a.bin", &pattern(2 * MIB, 3));
    fx.host_write("b.bin", &pattern(2 * MIB, 4));
    fx.settle();
    assert_eq!(fx.fetched(), 0);

    // a new engine, no memory of anything: the placeholders still say what they are
    let fe = fx.session.as_ref().unwrap().frontend();
    let mut fresh = Fx::engine(&fx.base.join("state2"), fe, fx.tr.clone());
    for _ in 0..4 {
        fx.hs.scan().unwrap();
        fresh.sync_once().unwrap();
    }
    assert_eq!(fx.fetched(), 0, "re-adopting clean placeholders must not read them");
    assert!(fx.hs.entry("a.bin").is_some());
}

#[test]
fn an_edit_is_pushed_and_the_file_stays_a_synced_placeholder() {
    let mut fx = Fx::new();
    fx.host_write("note.txt", b"version one");
    fx.settle();
    fx.fake.edit(&fx.p("note.txt"), b"version two, edited on this PC");
    // `settle_ms` is 0 and the edit is new: make it look settled
    set_mtime_ms(&fx.p("note.txt"), now_ms() - 600_000).unwrap();
    fx.settle();
    assert_eq!(std::fs::read(fx.host_root.join("note.txt")).unwrap(), b"version two, edited on this PC");
    let st = fx.fake.state(&fx.p("note.txt")).unwrap();
    assert!(st.placeholder && st.in_sync, "{st:?}");
    assert_eq!(st.identity.unwrap().hash, fx.hs.entry("note.txt").unwrap().hash);
}

#[test]
fn a_new_local_file_is_pushed_and_becomes_a_placeholder() {
    let mut fx = Fx::new();
    fx.settle();
    std::fs::write(fx.p("mine.txt"), b"made on this PC").unwrap();
    set_mtime_ms(&fx.p("mine.txt"), now_ms() - 600_000).unwrap();
    fx.settle();
    assert_eq!(std::fs::read(fx.host_root.join("mine.txt")).unwrap(), b"made on this PC");
    let st = fx.fake.state(&fx.p("mine.txt")).unwrap();
    assert!(st.placeholder && st.in_sync, "converted so that Explorer can free it: {st:?}");
}

#[test]
fn a_remote_change_replaces_a_clean_placeholder_and_frees_its_content() {
    let mut fx = Fx::new();
    fx.host_write("doc.txt", b"first");
    fx.settle();
    assert_eq!(fx.fake.read(&fx.p("doc.txt")), b"first");
    let before = fx.fetched();

    fx.host_write("doc.txt", b"second, longer");
    fx.settle();
    // the update did not fetch; the content is gone again until it is read
    assert_eq!(fx.fetched(), before);
    let st = fx.fake.state(&fx.p("doc.txt")).unwrap();
    assert!(st.dehydrated && st.in_sync, "{st:?}");
    assert_eq!(std::fs::metadata(fx.p("doc.txt")).unwrap().len(), 14);
    assert_eq!(fx.fake.read(&fx.p("doc.txt")), b"second, longer");
}

#[test]
fn a_pinned_file_is_kept_up_to_date_with_its_content() {
    let mut fx = Fx::new();
    fx.host_write("keep.bin", &pattern(MIB, 5));
    fx.settle();
    fx.fake.set_pin(&fx.p("keep.bin"), Pin::Pinned).unwrap();
    fx.fake.hydrate(&fx.p("keep.bin")).unwrap();
    let before = fx.fetched();

    fx.host_write("keep.bin", &pattern(MIB + 100, 6));
    fx.settle();
    // pinned: the engine downloaded the new version at once
    assert!(fx.fetched() >= before, "downloads go through the transport, not through the platform");
    assert_eq!(std::fs::read(fx.p("keep.bin")).unwrap(), pattern(MIB + 100, 6));
    let st = fx.fake.state(&fx.p("keep.bin")).unwrap();
    assert!(!st.dehydrated && st.in_sync && st.placeholder, "{st:?}");
}

#[test]
fn renames_and_deletes_travel_in_both_directions() {
    let mut fx = Fx::new();
    fx.host_write("a.txt", b"a");
    fx.host_write("old/b.txt", b"bee");
    fx.settle();

    std::fs::rename(fx.p("a.txt"), fx.p("renamed.txt")).unwrap();
    std::fs::remove_file(fx.p("old/b.txt")).unwrap();
    fx.settle();
    assert!(fx.host_root.join("renamed.txt").exists() && !fx.host_root.join("a.txt").exists());
    assert!(!fx.host_root.join("old/b.txt").exists());

    std::fs::remove_file(fx.host_root.join("renamed.txt")).unwrap();
    fx.settle();
    assert!(!fx.p("renamed.txt").exists());
}

#[test]
fn leaving_keeps_what_was_downloaded_and_removes_placeholders() {
    let mut fx = Fx::new();
    fx.host_write("seen.txt", b"I was opened");
    fx.host_write("unseen.bin", &pattern(MIB, 7));
    fx.settle();
    assert_eq!(fx.fake.read(&fx.p("seen.txt")), b"I was opened");

    let root = fx.root.clone();
    let session = fx.session.take().unwrap();
    session.leave(true).unwrap();
    assert_eq!(std::fs::read(root.join("seen.txt")).unwrap(), b"I was opened");
    assert!(!root.join("unseen.bin").exists(), "a placeholder without content cannot stay");
    assert!(fx.fake.entry(&root.join("seen.txt")).is_none(), "reverted to a normal file");
    assert_eq!(fx.fake.unregistered.lock().unwrap().as_slice(), &[root]);
}

#[test]
fn leaving_without_keeping_removes_everything() {
    let mut fx = Fx::new();
    fx.host_write("seen.txt", b"I was opened");
    fx.settle();
    fx.fake.read(&fx.p("seen.txt"));
    let root = fx.root.clone();
    fx.session.take().unwrap().leave(false).unwrap();
    assert!(!root.join("seen.txt").exists());
}

#[test]
fn a_folder_with_other_files_is_not_made_a_sync_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("occupied");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("mine.txt"), "user data").unwrap();
    let hs = HostedSync::open(
        HostedConfig { id: "d".repeat(32), name: "X".into(), root: dir.path().join("h"), store: None, backup: BackupPolicy::None, ignore: vec![] },
        None,
    );
    std::fs::create_dir_all(dir.path().join("h")).unwrap();
    let hs = hs.or_else(|_| {
        HostedSync::open(HostedConfig { id: "e".repeat(32), name: "X".into(), root: dir.path().join("h"), store: None, backup: BackupPolicy::None, ignore: vec![] }, None)
    });
    let tr: Arc<dyn Transport> = Arc::new(LocalTransport { host: hs.unwrap(), access: Access::ReadWrite, author: "PC".into() });
    let r = CloudSession::start_with(Fake::new(), CloudConfig { sync_id: "d".repeat(32), name: "X".into(), root: root.clone(), read_only: false }, tr);
    let e = r.err().expect("must refuse");
    assert!(format!("{e:#}").contains("not empty"), "{e:#}");
    assert!(root.join("mine.txt").exists());
}

#[test]
fn leave_root_works_without_a_connection() {
    // uninstall: the roots are unregistered although the app never connected them
    let fake = Fake::new();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("R");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("kept.txt"), "x").unwrap();
    leave_root(fake.as_ref(), &root, true).unwrap();
    assert!(root.join("kept.txt").exists());
    assert_eq!(fake.unregistered.lock().unwrap().len(), 1);
}
