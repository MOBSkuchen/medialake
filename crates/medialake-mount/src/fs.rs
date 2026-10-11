//! The NFSv3 view of a sync: a thin layer over [`Store`]. Metadata comes from the local cache tree
//! (never the network), reads hydrate the blocks they touch, writes make the file dirty and tell the
//! sync engine.

use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::sync::Arc;

use async_trait::async_trait;
use medialake_proto::sync::{is_ignored, normalize_path, parent_of};
use medialake_sync::util::{is_exec, now_ms, set_exec, set_mtime_ms};
use nfsserve::nfs::*;
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};

use crate::store::Store;

pub struct MountFs {
    inner: Arc<Inner>,
}

struct Inner {
    store: Arc<Store>,
    read_only: bool,
    /// 128 random bits in every file handle: a handle cannot be made up by another local process.
    secret: [u8; 16],
    uid: u32,
    gid: u32,
}

fn io_stat(e: &std::io::Error) -> nfsstat3 {
    match e.raw_os_error() {
        Some(libc::ENOENT) => nfsstat3::NFS3ERR_NOENT,
        Some(libc::EEXIST) => nfsstat3::NFS3ERR_EXIST,
        Some(libc::ENOTEMPTY) => nfsstat3::NFS3ERR_NOTEMPTY,
        Some(libc::ENOTDIR) => nfsstat3::NFS3ERR_NOTDIR,
        Some(libc::EISDIR) => nfsstat3::NFS3ERR_ISDIR,
        Some(libc::ENOSPC) => nfsstat3::NFS3ERR_NOSPC,
        Some(libc::EACCES) => nfsstat3::NFS3ERR_ACCES,
        Some(libc::EPERM) => nfsstat3::NFS3ERR_PERM,
        Some(libc::ENAMETOOLONG) => nfsstat3::NFS3ERR_NAMETOOLONG,
        Some(libc::EINVAL) => nfsstat3::NFS3ERR_INVAL,
        Some(libc::EROFS) => nfsstat3::NFS3ERR_ROFS,
        Some(libc::EFBIG) => nfsstat3::NFS3ERR_FBIG,
        _ => match e.kind() {
            std::io::ErrorKind::NotFound => nfsstat3::NFS3ERR_NOENT,
            std::io::ErrorKind::AlreadyExists => nfsstat3::NFS3ERR_EXIST,
            std::io::ErrorKind::PermissionDenied => nfsstat3::NFS3ERR_ACCES,
            _ => nfsstat3::NFS3ERR_IO,
        },
    }
}

fn any_stat(e: &anyhow::Error) -> nfsstat3 {
    match e.downcast_ref::<std::io::Error>() {
        Some(io) => io_stat(io),
        None => {
            tracing::warn!("{e:#}");
            nfsstat3::NFS3ERR_IO
        }
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    match tokio::task::spawn_blocking(f).await {
        Ok(v) => v,
        Err(e) => std::panic::resume_unwind(e.into_panic()),
    }
}

fn time3(sec: i64, nsec: i64) -> nfstime3 {
    nfstime3 { seconds: sec.clamp(0, u32::MAX as i64) as u32, nseconds: nsec.clamp(0, 999_999_999) as u32 }
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") }
}

impl MountFs {
    pub fn new(store: Arc<Store>, read_only: bool) -> MountFs {
        // SAFETY: getuid/getgid have no preconditions.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        MountFs { inner: Arc::new(Inner { store, read_only, secret: medialake_sync::util::random_bytes::<16>(), uid, gid }) }
    }
}

impl Inner {
    fn rel(&self, id: fileid3) -> Result<String, nfsstat3> {
        self.store.index().path_of(id).ok_or(nfsstat3::NFS3ERR_STALE)
    }

    fn rw(&self) -> Result<(), nfsstat3> {
        if self.read_only { Err(nfsstat3::NFS3ERR_ROFS) } else { Ok(()) }
    }

    fn attr(&self, id: u64, m: &std::fs::Metadata) -> fattr3 {
        let dir = m.is_dir();
        let mut mode = if dir { 0o755 } else if is_exec(m) { 0o755 } else { 0o644 };
        if self.read_only {
            mode &= 0o555;
        }
        fattr3 {
            ftype: if dir { ftype3::NF3DIR } else { ftype3::NF3REG },
            mode,
            nlink: if dir { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            size: if dir { 0 } else { m.len() },
            used: if dir { 0 } else { m.blocks() * 512 },
            rdev: specdata3::default(),
            fsid: 0x4d4c,
            fileid: id,
            atime: time3(m.atime(), m.atime_nsec()),
            mtime: time3(m.mtime(), m.mtime_nsec()),
            ctime: time3(m.ctime(), m.ctime_nsec()),
        }
    }

    /// Validates a name a client wants to create, returns it NFC-normalised.
    fn new_name(name: &filename3) -> Result<String, nfsstat3> {
        let s = std::str::from_utf8(name.as_ref()).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        if s.contains('/') {
            return Err(nfsstat3::NFS3ERR_INVAL);
        }
        normalize_path(s).map_err(|e| match e {
            medialake_proto::sync::PathError::TooLong | medialake_proto::sync::PathError::BadComponent(_) if s.len() > 255 => nfsstat3::NFS3ERR_NAMETOOLONG,
            _ => nfsstat3::NFS3ERR_INVAL,
        })
    }

    /// A name that is looked up or removed: whatever the client sends, as UTF-8, NFC if possible.
    fn old_name(name: &filename3) -> Result<String, nfsstat3> {
        let s = std::str::from_utf8(name.as_ref()).map_err(|_| nfsstat3::NFS3ERR_NOENT)?;
        if s.is_empty() || s.contains('/') || s.contains('\0') {
            return Err(nfsstat3::NFS3ERR_NOENT);
        }
        Ok(normalize_path(s).unwrap_or_else(|_| s.to_string()))
    }

    fn apply_sattr(store: &Store, rel: &str, s: &sattr3, read_only_fs: bool) -> Result<(), nfsstat3> {
        let abs = store.abs(rel);
        if let set_size3::size(n) = s.size {
            if read_only_fs {
                return Err(nfsstat3::NFS3ERR_ROFS);
            }
            let m = std::fs::metadata(&abs).map_err(|e| io_stat(&e))?;
            if m.is_dir() {
                return Err(nfsstat3::NFS3ERR_ISDIR);
            }
            if m.len() != n {
                if n != 0 {
                    store.ensure_full(rel).map_err(|e| any_stat(&e))?;
                }
                let f = std::fs::OpenOptions::new().write(true).open(&abs).map_err(|e| io_stat(&e))?;
                f.set_len(n).map_err(|e| io_stat(&e))?;
                store.after_write(rel);
            }
        }
        if let set_mode3::mode(m) = s.mode {
            if std::fs::metadata(&abs).is_ok_and(|md| md.is_file()) {
                let _ = set_exec(&abs, m & 0o100 != 0);
                store.changed(rel);
            }
        }
        let ms = match s.mtime {
            set_mtime::DONT_CHANGE => None,
            set_mtime::SET_TO_SERVER_TIME => Some(now_ms()),
            set_mtime::SET_TO_CLIENT_TIME(t) => Some(t.seconds as i64 * 1000 + (t.nseconds / 1_000_000) as i64),
        };
        if let Some(ms) = ms {
            set_mtime_ms(&abs, ms).map_err(|e| io_stat(&e))?;
            store.index().set_mtime(rel, ms);
            store.changed(rel);
        }
        Ok(())
    }
}

#[async_trait]
impl NFSFileSystem for MountFs {
    fn capabilities(&self) -> VFSCapabilities {
        if self.inner.read_only { VFSCapabilities::ReadOnly } else { VFSCapabilities::ReadWrite }
    }

    fn root_dir(&self) -> fileid3 {
        1
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let store = self.inner.store.clone();
        let nm = nfsstring(filename.as_ref().to_vec());
        blocking(move || {
            let dir = store.index().path_of(dirid).ok_or(nfsstat3::NFS3ERR_STALE)?;
            match nm.as_ref() {
                b"." => return Ok(dirid),
                b".." => {
                    let parent = parent_of(&dir).unwrap_or("");
                    return store.index().ino(parent).map_err(|_| nfsstat3::NFS3ERR_IO);
                }
                _ => {}
            }
            let name = Inner::old_name(&nm)?;
            let rel = join(&dir, &name);
            match std::fs::symlink_metadata(store.abs(&rel)) {
                Ok(m) if !m.file_type().is_symlink() => store.index().ino(&rel).map_err(|_| nfsstat3::NFS3ERR_IO),
                Ok(_) => Err(nfsstat3::NFS3ERR_NOENT),
                Err(e) => Err(io_stat(&e)),
            }
        })
        .await
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        let me = self.inner.clone();
        blocking(move || {
            let rel = me.store.index().path_of(id).ok_or(nfsstat3::NFS3ERR_STALE)?;
            let m = std::fs::metadata(me.store.abs(&rel)).map_err(|_| nfsstat3::NFS3ERR_STALE)?;
            Ok(me.attr(id, &m))
        })
        .await
    }

    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        let me = self.inner.clone();
        blocking(move || {
            let rel = me.store.index().path_of(id).ok_or(nfsstat3::NFS3ERR_STALE)?;
            // changing times or the executable bit on a read-only mount is refused too
            me.rw()?;
            Inner::apply_sattr(&me.store, &rel, &setattr, me.read_only)?;
            let m = std::fs::metadata(me.store.abs(&rel)).map_err(|e| io_stat(&e))?;
            Ok(me.attr(id, &m))
        })
        .await
    }

    async fn read(&self, id: fileid3, offset: u64, count: u32) -> Result<(Vec<u8>, bool), nfsstat3> {
        let store = self.inner.store.clone();
        blocking(move || {
            let rel = store.index().path_of(id).ok_or(nfsstat3::NFS3ERR_STALE)?;
            match std::fs::metadata(store.abs(&rel)) {
                Ok(m) if m.is_dir() => return Err(nfsstat3::NFS3ERR_ISDIR),
                Ok(_) => {}
                Err(_) => return Err(nfsstat3::NFS3ERR_STALE),
            }
            store.read(&rel, offset, count).map_err(|e| any_stat(&e))
        })
        .await
    }

    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        let me = self.inner.clone();
        let data = data.to_vec();
        blocking(move || {
            me.rw()?;
            let rel = me.store.index().path_of(id).ok_or(nfsstat3::NFS3ERR_STALE)?;
            let abs = me.store.abs(&rel);
            if std::fs::metadata(&abs).is_ok_and(|m| m.is_dir()) {
                return Err(nfsstat3::NFS3ERR_ISDIR);
            }
            // a partial placeholder needs its remaining content before it can be changed in place
            me.store.ensure_full(&rel).map_err(|e| any_stat(&e))?;
            let f = std::fs::OpenOptions::new().write(true).open(&abs).map_err(|e| io_stat(&e))?;
            f.write_all_at(&data, offset).map_err(|e| io_stat(&e))?;
            drop(f);
            me.store.after_write(&rel);
            let m = std::fs::metadata(&abs).map_err(|e| io_stat(&e))?;
            Ok(me.attr(id, &m))
        })
        .await
    }

    async fn create(&self, dirid: fileid3, filename: &filename3, attr: sattr3) -> Result<(fileid3, fattr3), nfsstat3> {
        let me = self.inner.clone();
        let name = filename.as_ref().to_vec();
        blocking(move || {
            me.rw()?;
            let dir = me.rel(dirid)?;
            let name = Inner::new_name(&nfsstring(name))?;
            let rel = join(&dir, &name);
            let abs = me.store.abs(&rel);
            match std::fs::symlink_metadata(&abs) {
                Ok(m) if m.is_dir() => return Err(nfsstat3::NFS3ERR_ISDIR),
                Ok(_) => {
                    // UNCHECKED create over an existing file: truncated, like open(O_TRUNC)
                    let mut a = attr;
                    if matches!(a.size, set_size3::Void) {
                        a.size = set_size3::size(0);
                    }
                    Inner::apply_sattr(&me.store, &rel, &a, me.read_only)?;
                }
                Err(_) => {
                    std::fs::OpenOptions::new().write(true).create_new(true).open(&abs).map_err(|e| io_stat(&e))?;
                    let _ = std::fs::set_permissions(&abs, std::fs::Permissions::from_mode(0o644));
                    let mut a = attr;
                    a.size = set_size3::Void;
                    Inner::apply_sattr(&me.store, &rel, &a, me.read_only)?;
                }
            }
            me.store.changed(&rel);
            let id = me.store.index().ino(&rel).map_err(|_| nfsstat3::NFS3ERR_IO)?;
            let m = std::fs::metadata(&abs).map_err(|e| io_stat(&e))?;
            Ok((id, me.attr(id, &m)))
        })
        .await
    }

    async fn create_exclusive(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let me = self.inner.clone();
        let name = filename.as_ref().to_vec();
        blocking(move || {
            me.rw()?;
            let dir = me.rel(dirid)?;
            let name = Inner::new_name(&nfsstring(name))?;
            let rel = join(&dir, &name);
            let abs = me.store.abs(&rel);
            std::fs::OpenOptions::new().write(true).create_new(true).open(&abs).map_err(|e| io_stat(&e))?;
            let _ = std::fs::set_permissions(&abs, std::fs::Permissions::from_mode(0o644));
            me.store.changed(&rel);
            me.store.index().ino(&rel).map_err(|_| nfsstat3::NFS3ERR_IO)
        })
        .await
    }

    async fn mkdir(&self, dirid: fileid3, dirname: &filename3) -> Result<(fileid3, fattr3), nfsstat3> {
        let me = self.inner.clone();
        let name = dirname.as_ref().to_vec();
        blocking(move || {
            me.rw()?;
            let dir = me.rel(dirid)?;
            let name = Inner::new_name(&nfsstring(name))?;
            let rel = join(&dir, &name);
            let abs = me.store.abs(&rel);
            std::fs::create_dir(&abs).map_err(|e| io_stat(&e))?;
            me.store.changed(&rel);
            let id = me.store.index().ino(&rel).map_err(|_| nfsstat3::NFS3ERR_IO)?;
            let m = std::fs::metadata(&abs).map_err(|e| io_stat(&e))?;
            Ok((id, me.attr(id, &m)))
        })
        .await
    }

    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        let me = self.inner.clone();
        let name = filename.as_ref().to_vec();
        blocking(move || {
            me.rw()?;
            let dir = me.rel(dirid)?;
            let name = Inner::old_name(&nfsstring(name))?;
            let rel = join(&dir, &name);
            let abs = me.store.abs(&rel);
            let m = std::fs::symlink_metadata(&abs).map_err(|e| io_stat(&e))?;
            if m.is_dir() {
                match std::fs::remove_dir(&abs) {
                    Ok(()) => {}
                    Err(e) if e.raw_os_error() == Some(libc::ENOTEMPTY) => {
                        // only junk (.DS_Store, ._x) left counts as empty, as it does in the sync
                        let only_junk = std::fs::read_dir(&abs).map(|rd| rd.flatten().all(|e| e.file_name().to_str().is_some_and(|n| is_ignored(n, &[])))).unwrap_or(false);
                        if !only_junk {
                            return Err(nfsstat3::NFS3ERR_NOTEMPTY);
                        }
                        std::fs::remove_dir_all(&abs).map_err(|e| io_stat(&e))?;
                    }
                    Err(e) => return Err(io_stat(&e)),
                }
            } else {
                std::fs::remove_file(&abs).map_err(|e| io_stat(&e))?;
            }
            let _ = me.store.index().remove_tree(&rel);
            me.store.changed(&rel);
            Ok(())
        })
        .await
    }

    async fn rename(&self, from_dirid: fileid3, from_filename: &filename3, to_dirid: fileid3, to_filename: &filename3) -> Result<(), nfsstat3> {
        let me = self.inner.clone();
        let (ff, tf) = (from_filename.as_ref().to_vec(), to_filename.as_ref().to_vec());
        blocking(move || {
            me.rw()?;
            let (fd, td) = (me.rel(from_dirid)?, me.rel(to_dirid)?);
            let src = join(&fd, &Inner::old_name(&nfsstring(ff))?);
            let dst = join(&td, &Inner::new_name(&nfsstring(tf))?);
            if src == dst {
                return Ok(());
            }
            let (sa, da) = (me.store.abs(&src), me.store.abs(&dst));
            let sm = std::fs::symlink_metadata(&sa).map_err(|e| io_stat(&e))?;
            if let Ok(dm) = std::fs::symlink_metadata(&da) {
                match (sm.is_dir(), dm.is_dir()) {
                    (false, true) => return Err(nfsstat3::NFS3ERR_ISDIR),
                    (true, false) => return Err(nfsstat3::NFS3ERR_NOTDIR),
                    _ => {}
                }
            }
            std::fs::rename(&sa, &da).map_err(|e| io_stat(&e))?;
            let _ = me.store.index().rename_tree(&src, &dst);
            me.store.changed(&src);
            me.store.changed(&dst);
            Ok(())
        })
        .await
    }

    async fn readdir(&self, dirid: fileid3, start_after: fileid3, max_entries: usize) -> Result<ReadDirResult, nfsstat3> {
        let me = self.inner.clone();
        blocking(move || {
            let dir = me.rel(dirid)?;
            let abs = me.store.abs(&dir);
            let rd = std::fs::read_dir(&abs).map_err(|e| io_stat(&e))?;
            let mut all: Vec<(u64, String, std::fs::Metadata)> = vec![];
            for e in rd.flatten() {
                let Some(name) = e.file_name().to_str().map(str::to_string) else { continue };
                let Ok(m) = e.metadata() else { continue };
                if m.file_type().is_symlink() {
                    continue;
                }
                let Ok(id) = me.store.index().ino(&join(&dir, &name)) else { continue };
                all.push((id, name, m));
            }
            all.sort_by_key(|(id, _, _)| *id);
            let mut entries = vec![];
            let mut end = true;
            for (id, name, m) in all {
                if id <= start_after {
                    continue;
                }
                if entries.len() >= max_entries {
                    end = false;
                    break;
                }
                entries.push(DirEntry { fileid: id, name: name.into_bytes().into(), attr: me.attr(id, &m) });
            }
            Ok(ReadDirResult { entries, end })
        })
        .await
    }

    async fn symlink(&self, _dirid: fileid3, _linkname: &filename3, _symlink: &nfspath3, _attr: &sattr3) -> Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }

    async fn readlink(&self, _id: fileid3) -> Result<nfspath3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }

    fn id_to_fh(&self, id: fileid3) -> nfs_fh3 {
        let mut data = Vec::with_capacity(24);
        data.extend_from_slice(&self.inner.secret);
        data.extend_from_slice(&id.to_le_bytes());
        nfs_fh3 { data }
    }

    fn fh_to_id(&self, fh: &nfs_fh3) -> Result<fileid3, nfsstat3> {
        if fh.data.len() != 24 || fh.data[..16] != self.inner.secret {
            return Err(nfsstat3::NFS3ERR_BADHANDLE);
        }
        Ok(u64::from_le_bytes(fh.data[16..24].try_into().unwrap()))
    }

    fn serverid(&self) -> cookieverf3 {
        let mut v = [0u8; 8];
        v.copy_from_slice(&self.inner.secret[..8]);
        v
    }
}
