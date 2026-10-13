//! The cfapi layer against the real platform: registers sync roots in a temporary folder (NTFS) and reads
//! placeholders through Windows. Runs on Windows only.
#![cfg(windows)]

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use medialake_cloudapi::win::{MAX_CHUNK, WinCloud};
use medialake_cloudapi::*;

/// Content of every file: byte `i` is `i % 251`.
fn pattern(offset: u64, len: u64) -> Vec<u8> {
    (offset..offset + len).map(|i| (i % 251) as u8).collect()
}

#[derive(Default)]
struct Source {
    requests: Mutex<Vec<(String, u64, u64)>>,
    fail: Mutex<Option<CloudError>>,
}

impl Provider for Source {
    fn fetch(&self, req: &FetchRequest, out: &dyn Transfer) -> CloudResult<()> {
        self.requests.lock().unwrap().push((req.rel.clone(), req.offset, req.length));
        if let Some(e) = self.fail.lock().unwrap().clone() {
            return Err(e);
        }
        let pieces = plan_chunks(req.offset, req.length, req.file_size, MAX_CHUNK);
        let total: u64 = pieces.iter().map(|(_, l)| l).sum();
        let mut done = 0;
        for (offset, len) in pieces {
            out.data(offset, &pattern(offset, len))?;
            done += len;
            out.progress(total, done);
        }
        Ok(())
    }
}

struct Root {
    _dir: tempfile::TempDir,
    path: PathBuf,
    api: WinCloud,
    source: Arc<Source>,
    conn: Option<Box<dyn Connection>>,
}

impl Root {
    fn new() -> Root {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap().join("Root");
        std::fs::create_dir_all(&path).unwrap();
        let api = WinCloud;
        api.available(&path).expect("Cloud Files on an NTFS temp folder");
        let reg = Registration {
            provider_name: "MediaLake Test".into(),
            provider_version: "0.1".into(),
            root_identity: b"test".to_vec(),
            provider_id: *b"MediaLakeTests01",
        };
        api.register(&path, &reg).expect("register");
        let source = Arc::new(Source::default());
        let conn = api.connect(&path, source.clone()).expect("connect");
        Root { _dir: dir, path, api, source, conn: Some(conn) }
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.path.join(rel)
    }

    fn file(&self, rel: &str, size: u64, rev: u64) {
        let p = self.p(rel);
        let info = PlaceholderInfo {
            is_dir: false,
            size,
            mtime_ms: 1_700_000_000_000,
            read_only: false,
            identity: Some(Identity { rev, hash: "ab12".into() }),
            in_sync: true,
        };
        self.api.create_placeholder(p.parent().unwrap(), p.file_name().unwrap().to_str().unwrap(), &info).expect("file placeholder");
    }

    fn dir(&self, rel: &str) {
        let p = self.p(rel);
        let info = PlaceholderInfo { is_dir: true, size: 0, mtime_ms: 1_700_000_000_000, read_only: false, identity: None, in_sync: true };
        self.api.create_placeholder(p.parent().unwrap(), p.file_name().unwrap().to_str().unwrap(), &info).expect("folder placeholder");
    }

    fn requests(&self) -> Vec<(String, u64, u64)> {
        self.source.requests.lock().unwrap().clone()
    }

    fn state(&self, rel: &str) -> FileState {
        self.api.state(&self.p(rel)).unwrap()
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        self.conn.take();
        let _ = self.api.unregister(&self.path);
    }
}

fn read_range(p: &Path, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
    let mut f = std::fs::File::open(p)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

#[test]
fn placeholders_are_created_and_say_what_they_are() {
    let r = Root::new();
    r.file("hello.txt", 1024, 7);
    r.dir("docs");
    r.file("docs/notes.txt", 10_000, 8);

    let st = r.state("hello.txt");
    assert!(st.exists && st.placeholder && st.in_sync && st.dehydrated && !st.is_dir, "{st:?}");
    assert_eq!(st.size, 1024);
    assert_eq!(st.identity, Some(Identity { rev: 7, hash: "ab12".into() }));
    assert_eq!(st.pin, Pin::Unspecified);
    assert!(attr::would_download(st.attributes), "{:#x}", st.attributes);
    assert_eq!(r.api.state_light(&r.p("hello.txt")).unwrap().identity, None);

    let d = r.state("docs");
    assert!(d.exists && d.is_dir && d.placeholder, "{d:?}");
    assert!(r.state("docs/notes.txt").dehydrated);
    assert!(!r.state("nothing.txt").exists);

    // what the engine's scan uses must see every placeholder and must not fetch anything
    let m = std::fs::symlink_metadata(r.p("hello.txt")).unwrap();
    assert!(!m.file_type().is_symlink() && m.is_file());
    assert_eq!(m.len(), 1024);
    let m = std::fs::symlink_metadata(r.p("docs")).unwrap();
    assert!(!m.file_type().is_symlink() && m.is_dir());
    let names: Vec<String> = std::fs::read_dir(&r.path).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(std::fs::read_dir(r.p("docs")).unwrap().count() == 1);
    assert!(r.requests().is_empty(), "looking is not reading: {:?}", r.requests());
}

#[test]
fn reads_are_served_from_the_provider() {
    let r = Root::new();
    r.file("hello.txt", 1024, 1);
    r.dir("docs");
    r.file("docs/deep.bin", 300_000, 2);
    r.file("big.bin", 24 << 20, 3);

    assert_eq!(std::fs::read(r.p("hello.txt")).unwrap(), pattern(0, 1024));
    assert_eq!(std::fs::read(r.p("docs/deep.bin")).unwrap(), pattern(0, 300_000));
    let st = r.state("hello.txt");
    assert!(st.placeholder && st.in_sync && !st.dehydrated, "{st:?}");
    assert!(!attr::would_download(st.attributes));

    // a range in the middle of a large file, unaligned. Full hydration: the whole file comes, in
    // several pieces, and is then marked as being here
    assert_eq!(read_range(&r.p("big.bin"), 5_000_123, 1000).unwrap(), pattern(5_000_123, 1000));
    let reqs = r.requests();
    assert!(reqs.iter().any(|(rel, ..)| rel == "docs/deep.bin"), "paths are sync-relative: {reqs:?}");
    assert_eq!(reqs.iter().filter(|(rel, ..)| rel == "big.bin").collect::<Vec<_>>(), vec![&("big.bin".to_string(), 0, 24 << 20)]);
    assert!(!r.state("big.bin").dehydrated);
    assert_eq!(read_range(&r.p("big.bin"), (24 << 20) - 500, 500).unwrap(), pattern((24 << 20) - 500, 500));
    // the end of the file is not a multiple of the block size
    r.file("odd.bin", 10_001, 4);
    assert_eq!(read_range(&r.p("odd.bin"), 9_000, 1001).unwrap(), pattern(9_000, 1001));
    // nothing is asked for twice
    assert_eq!(r.requests().len(), 4, "{:?}", r.requests());
}

#[test]
fn another_program_reads_through_the_provider() {
    let r = Root::new();
    r.file("seen.bin", 70_000, 1);
    let path = r.p("seen.bin").to_string_lossy().trim_start_matches(r"\\?\").to_string();
    let out = std::process::Command::new("cmd").args(["/c", "copy", "/b", &path, "NUL"]).output().expect("cmd");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    assert_eq!(r.requests(), vec![("seen.bin".to_string(), 0, 70_000)]);
    assert!(!r.state("seen.bin").dehydrated);
}

#[test]
fn a_failing_provider_fails_the_read_at_once() {
    let r = Root::new();
    r.file("a.bin", 50_000, 1);
    *r.source.fail.lock().unwrap() = Some(CloudError::Offline("cannot connect".into()));
    let started = std::time::Instant::now();
    let e = std::fs::read(r.p("a.bin")).expect_err("the host is away");
    assert!(started.elapsed() < std::time::Duration::from_secs(10), "no time-out: {:?}", started.elapsed());
    // ERROR_CLOUD_FILE_NETWORK_UNAVAILABLE
    assert_eq!(e.raw_os_error(), Some(388), "{e}");
    *r.source.fail.lock().unwrap() = Some(CloudError::Failed("no".into()));
    let e = std::fs::read(r.p("a.bin")).expect_err("still failing");
    // ERROR_CLOUD_FILE_UNSUCCESSFUL
    assert_eq!(e.raw_os_error(), Some(389), "{e}");
    // and once the host is back the same file opens
    *r.source.fail.lock().unwrap() = None;
    assert_eq!(std::fs::read(r.p("a.bin")).unwrap(), pattern(0, 50_000));
}

#[test]
fn pin_hydrate_and_free() {
    let r = Root::new();
    r.file("keep.bin", 200_000, 1);
    r.api.set_pin(&r.p("keep.bin"), Pin::Pinned).unwrap();
    assert_eq!(r.state("keep.bin").pin, Pin::Pinned);
    assert!(r.state("keep.bin").dehydrated, "a pin mark alone fetches nothing");
    r.api.hydrate(&r.p("keep.bin")).unwrap();
    let st = r.state("keep.bin");
    assert!(!st.dehydrated && st.in_sync, "{st:?}");

    r.api.set_pin(&r.p("keep.bin"), Pin::Unpinned).unwrap();
    assert_eq!(r.state("keep.bin").pin, Pin::Unpinned);
    r.api.dehydrate(&r.p("keep.bin")).unwrap();
    assert!(r.state("keep.bin").dehydrated);
    assert_eq!(std::fs::read(r.p("keep.bin")).unwrap(), pattern(0, 200_000));

    // a folder carries a mark too
    r.dir("album");
    r.api.set_pin(&r.p("album"), Pin::Pinned).unwrap();
    assert_eq!(r.state("album").pin, Pin::Pinned);
}

#[test]
fn local_files_become_placeholders_and_edits_show() {
    let r = Root::new();
    std::fs::write(r.p("mine.txt"), b"made on this PC").unwrap();
    let st = r.state("mine.txt");
    assert!(st.exists && !st.placeholder && !st.in_sync, "{st:?}");

    let id = Identity { rev: 5, hash: "c0ffee".into() };
    r.api.mark_synced(&r.p("mine.txt"), &id).unwrap();
    let st = r.state("mine.txt");
    assert!(st.placeholder && st.in_sync && !st.dehydrated, "{st:?}");
    assert_eq!(st.identity, Some(id));
    assert_eq!(std::fs::read(r.p("mine.txt")).unwrap(), b"made on this PC");

    // an edit is what clears "in sync"
    std::fs::write(r.p("mine.txt"), b"edited on this PC, longer").unwrap();
    let st = r.state("mine.txt");
    assert!(st.placeholder && !st.in_sync, "{st:?}");
    let id2 = Identity { rev: 6, hash: "decaf0".into() };
    r.api.mark_synced(&r.p("mine.txt"), &id2).unwrap();
    let st = r.state("mine.txt");
    assert!(st.in_sync && st.identity == Some(id2), "{st:?}");
}

/// A provider whose content can change, like a host that received a push.
struct Host(Mutex<Vec<u8>>);

impl Provider for Host {
    fn fetch(&self, req: &FetchRequest, out: &dyn Transfer) -> CloudResult<()> {
        let c = self.0.lock().unwrap().clone();
        for (o, l) in plan_chunks(req.offset, req.length, req.file_size, MAX_CHUNK) {
            out.data(o, &c[o as usize..((o + l) as usize).min(c.len())])?;
        }
        Ok(())
    }
}

#[test]
fn a_file_that_changed_size_here_comes_back_whole() {
    let mut r = Root::new();
    let host = Arc::new(Host(Mutex::new(vec![])));
    r.conn.take();
    r.conn = Some(r.api.connect(&r.path, host.clone()).unwrap());
    for (name, new) in [("grow.txt", &b"version one, then edited on this PC"[..]), ("shrink.txt", b"short"), ("same.txt", b"VERSION ONE")] {
        *host.0.lock().unwrap() = b"version one".to_vec();
        r.file(name, 11, 1);
        std::fs::write(r.p(name), new).unwrap();
        let before = std::fs::metadata(r.p(name)).unwrap().modified().unwrap();
        // pushed: the host has the new content, the file is in sync again
        *host.0.lock().unwrap() = new.to_vec();
        r.api.mark_synced(&r.p(name), &Identity { rev: 2, hash: "cd".into() }).unwrap();
        assert_eq!(std::fs::metadata(r.p(name)).unwrap().modified().unwrap(), before, "marking keeps the file's time");
        let st = r.state(name);
        assert!(st.in_sync && !st.dehydrated && st.size == new.len() as u64, "{st:?}");
        // freed and opened again: all of the new content, not the old length padded with zeros
        r.api.dehydrate(&r.p(name)).unwrap();
        assert_eq!(std::fs::read(r.p(name)).unwrap(), new, "{name}");
    }
}

#[test]
fn an_update_replaces_what_a_placeholder_stands_for() {
    let r = Root::new();
    r.file("doc.bin", 20_000, 1);
    assert_eq!(std::fs::read(r.p("doc.bin")).unwrap(), pattern(0, 20_000));
    let info = PlaceholderInfo {
        is_dir: false,
        size: 30_000,
        mtime_ms: 1_710_000_000_000,
        read_only: false,
        identity: Some(Identity { rev: 2, hash: "beef".into() }),
        in_sync: true,
    };
    r.api.update_placeholder(&r.p("doc.bin"), &info, true).unwrap();
    let st = r.state("doc.bin");
    assert!(st.placeholder && st.in_sync && st.dehydrated, "{st:?}");
    assert_eq!(st.size, 30_000);
    assert_eq!(st.identity.unwrap().rev, 2);
    let m = std::fs::metadata(r.p("doc.bin")).unwrap();
    assert_eq!(m.modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis(), 1_710_000_000_000);
    assert_eq!(std::fs::read(r.p("doc.bin")).unwrap(), pattern(0, 30_000));
}

#[test]
fn read_only_placeholders_and_revert() {
    let r = Root::new();
    let info = PlaceholderInfo {
        is_dir: false,
        size: 5000,
        mtime_ms: 1_700_000_000_000,
        read_only: true,
        identity: Some(Identity { rev: 1, hash: "aa".into() }),
        in_sync: true,
    };
    r.api.create_placeholder(&r.path, "ro.txt", &info).unwrap();
    let st = r.state("ro.txt");
    assert!(st.attributes & attr::READONLY != 0 && st.dehydrated, "{st:?}");
    assert!(std::fs::OpenOptions::new().write(true).open(r.p("ro.txt")).is_err());
    assert_eq!(std::fs::read(r.p("ro.txt")).unwrap(), pattern(0, 5000));

    // a hydrated placeholder goes back to being a plain file
    r.file("plain.txt", 3000, 2);
    assert_eq!(std::fs::read(r.p("plain.txt")).unwrap(), pattern(0, 3000));
    r.api.revert(&r.p("plain.txt")).unwrap();
    let st = r.state("plain.txt");
    assert!(st.exists && !st.placeholder, "{st:?}");
    assert_eq!(std::fs::read(r.p("plain.txt")).unwrap(), pattern(0, 3000));
}

#[test]
fn registering_twice_and_other_volumes() {
    let r = Root::new();
    let reg = Registration { provider_name: "MediaLake Test".into(), provider_version: "0.2".into(), root_identity: b"test".to_vec(), provider_id: *b"MediaLakeTests01" };
    r.api.register(&r.path, &reg).expect("an existing registration is updated");
    assert!(r.api.available(Path::new(r"\\server\share\folder")).is_err());
    assert!(r.api.available(Path::new(r"relative\folder")).is_err());
}
