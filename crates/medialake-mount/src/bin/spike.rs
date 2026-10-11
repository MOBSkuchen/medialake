//! A0 spike: a toy file system behind the same NFSv3 server the real frontend will use. In-memory tree,
//! artificially slow reads, every request logged with a timestamp, so that what Finder, Quick Look and
//! the editors do to a mount can be read off the log.
//!
//!   mount-spike [--port 11111] [--delay-ms 500] [--log /tmp/spike.log]
//!
//! then, in another shell:
//!   mkdir -p ~/spike && mount_nfs -o vers=3,tcp,nolocks,noresvport,port=11111,mountport=11111 localhost:/ ~/spike

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use nfsserve::nfs::*;
use nfsserve::tcp::{NFSTcp, NFSTcpListener};
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};

struct Node {
    name: Vec<u8>,
    parent: u64,
    dir: bool,
    /// content is generated on demand: byte i = (i % 251) as u8, plus whatever was written
    size: u64,
    written: Vec<u8>,
    synthetic: bool,
    mtime: u32,
}

struct Toy {
    nodes: Mutex<BTreeMap<u64, Node>>,
    next: AtomicU64,
    delay: Duration,
    log: Mutex<std::fs::File>,
    t0: Instant,
}

impl Toy {
    fn log(&self, what: String) {
        let t = self.t0.elapsed().as_millis();
        let _ = writeln!(self.log.lock().unwrap(), "{t:>8} ms  {what}");
    }

    fn attr(&self, id: u64, n: &Node) -> fattr3 {
        let t = nfstime3 { seconds: n.mtime, nseconds: 0 };
        fattr3 {
            ftype: if n.dir { ftype3::NF3DIR } else { ftype3::NF3REG },
            mode: if n.dir { 0o755 } else { 0o644 },
            nlink: if n.dir { 2 } else { 1 },
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            size: n.size,
            used: n.size,
            rdev: specdata3::default(),
            fsid: 7,
            fileid: id,
            atime: t,
            mtime: t,
            ctime: t,
        }
    }

    fn add(&self, parent: u64, name: &str, dir: bool, size: u64, synthetic: bool) -> u64 {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as u32;
        self.nodes.lock().unwrap().insert(id, Node { name: name.as_bytes().to_vec(), parent, dir, size, written: vec![], synthetic, mtime: now });
        id
    }
}

fn nm(f: &filename3) -> String {
    String::from_utf8_lossy(f).into_owned()
}

#[async_trait]
impl NFSFileSystem for Toy {
    fn capabilities(&self) -> VFSCapabilities {
        VFSCapabilities::ReadWrite
    }
    fn root_dir(&self) -> fileid3 {
        1
    }
    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let n = nm(filename);
        let nodes = self.nodes.lock().unwrap();
        if n == "." {
            return Ok(dirid);
        }
        if n == ".." {
            return Ok(nodes.get(&dirid).map(|d| d.parent).unwrap_or(1));
        }
        let r = nodes.iter().find(|(_, x)| x.parent == dirid && x.name == *filename.as_ref()).map(|(i, _)| *i);
        drop(nodes);
        self.log(format!("lookup {dirid}/{n} -> {r:?}"));
        r.ok_or(nfsstat3::NFS3ERR_NOENT)
    }
    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        let nodes = self.nodes.lock().unwrap();
        nodes.get(&id).map(|n| self.attr(id, n)).ok_or(nfsstat3::NFS3ERR_STALE)
    }
    async fn setattr(&self, id: fileid3, s: sattr3) -> Result<fattr3, nfsstat3> {
        let mut nodes = self.nodes.lock().unwrap();
        let n = nodes.get_mut(&id).ok_or(nfsstat3::NFS3ERR_STALE)?;
        if let set_size3::size(sz) = s.size {
            n.size = sz;
            if n.synthetic {
                n.synthetic = false;
                n.written.clear();
            }
            n.written.resize(sz as usize, 0);
        }
        let a = self.attr(id, n);
        drop(nodes);
        self.log(format!("setattr {id}"));
        Ok(a)
    }
    async fn read(&self, id: fileid3, offset: u64, count: u32) -> Result<(Vec<u8>, bool), nfsstat3> {
        self.log(format!("READ   id={id} off={offset} count={count}"));
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        let nodes = self.nodes.lock().unwrap();
        let n = nodes.get(&id).ok_or(nfsstat3::NFS3ERR_STALE)?;
        if offset >= n.size {
            return Ok((vec![], true));
        }
        let end = (offset + count as u64).min(n.size);
        let out: Vec<u8> = if n.synthetic { (offset..end).map(|i| (i % 251) as u8).collect() } else { n.written[offset as usize..end as usize].to_vec() };
        Ok((out, end == n.size))
    }
    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        let mut nodes = self.nodes.lock().unwrap();
        let n = nodes.get_mut(&id).ok_or(nfsstat3::NFS3ERR_STALE)?;
        if n.synthetic {
            n.written = (0..n.size).map(|i| (i % 251) as u8).collect();
            n.synthetic = false;
        }
        let end = offset as usize + data.len();
        if n.written.len() < end {
            n.written.resize(end, 0);
        }
        n.written[offset as usize..end].copy_from_slice(data);
        n.size = n.written.len() as u64;
        let a = self.attr(id, n);
        drop(nodes);
        self.log(format!("WRITE  id={id} off={offset} len={}", data.len()));
        Ok(a)
    }
    async fn create(&self, dirid: fileid3, filename: &filename3, _attr: sattr3) -> Result<(fileid3, fattr3), nfsstat3> {
        let n = nm(filename);
        self.log(format!("create {dirid}/{n}"));
        let id = self.add(dirid, &n, false, 0, false);
        Ok((id, self.getattr(id).await?))
    }
    async fn create_exclusive(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let n = nm(filename);
        self.log(format!("create_exclusive {dirid}/{n}"));
        Ok(self.add(dirid, &n, false, 0, false))
    }
    async fn mkdir(&self, dirid: fileid3, dirname: &filename3) -> Result<(fileid3, fattr3), nfsstat3> {
        let n = nm(dirname);
        self.log(format!("mkdir {dirid}/{n}"));
        let id = self.add(dirid, &n, true, 0, false);
        Ok((id, self.getattr(id).await?))
    }
    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        let id = self.lookup(dirid, filename).await?;
        self.log(format!("remove {dirid}/{}", nm(filename)));
        self.nodes.lock().unwrap().remove(&id);
        Ok(())
    }
    async fn rename(&self, fd: fileid3, ff: &filename3, td: fileid3, tf: &filename3) -> Result<(), nfsstat3> {
        let id = self.lookup(fd, ff).await?;
        self.log(format!("rename {fd}/{} -> {td}/{}", nm(ff), nm(tf)));
        if let Ok(old) = self.lookup(td, tf).await {
            self.nodes.lock().unwrap().remove(&old);
        }
        let mut nodes = self.nodes.lock().unwrap();
        let n = nodes.get_mut(&id).ok_or(nfsstat3::NFS3ERR_STALE)?;
        n.parent = td;
        n.name = tf.as_ref().to_vec();
        Ok(())
    }
    async fn readdir(&self, dirid: fileid3, start_after: fileid3, max_entries: usize) -> Result<ReadDirResult, nfsstat3> {
        self.log(format!("readdir {dirid} after={start_after}"));
        let nodes = self.nodes.lock().unwrap();
        let mut entries = vec![];
        let mut end = true;
        for (id, n) in nodes.iter().filter(|(id, n)| n.parent == dirid && **id != 1) {
            if *id <= start_after {
                continue;
            }
            if entries.len() >= max_entries {
                end = false;
                break;
            }
            entries.push(DirEntry { fileid: *id, name: n.name.clone().into(), attr: self.attr(*id, n) });
        }
        Ok(ReadDirResult { entries, end })
    }
    async fn symlink(&self, _d: fileid3, _l: &filename3, _s: &nfspath3, _a: &sattr3) -> Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }
    async fn readlink(&self, _id: fileid3) -> Result<nfspath3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).with_writer(std::io::stderr).init();
    let args: Vec<String> = std::env::args().collect();
    let get = |k: &str, d: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned().unwrap_or(d.into());
    let port: u16 = get("--port", "11111").parse()?;
    let delay: u64 = get("--delay-ms", "500").parse()?;
    let log = std::fs::File::create(get("--log", "/tmp/mount-spike.log"))?;
    let toy = Toy { nodes: Mutex::new(BTreeMap::new()), next: AtomicU64::new(2), delay: Duration::from_millis(delay), log: Mutex::new(log), t0: Instant::now() };
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as u32;
    toy.nodes.lock().unwrap().insert(1, Node { name: vec![], parent: 1, dir: true, size: 0, written: vec![], synthetic: false, mtime: now });
    let docs = toy.add(1, "docs", true, 0, false);
    toy.add(1, "hello.txt", false, 13, true);
    toy.add(1, "photo.jpg", false, 6_000_000, true);
    toy.add(1, "movie.mov", false, 1_500_000_000, true);
    toy.add(docs, "report.pdf", false, 2_000_000, true);
    toy.add(docs, "notes.md", false, 4_000, true);
    let listener = NFSTcpListener::bind(&format!("127.0.0.1:{port}"), toy).await?;
    eprintln!("serving on 127.0.0.1:{port} (delay {delay} ms)");
    listener.handle_forever().await?;
    Ok(())
}
