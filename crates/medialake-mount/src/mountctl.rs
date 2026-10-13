//! Mounting, unmounting and cleaning up after a crash. Everything is the system's own `mount_nfs` /
//! `unmount(2)`; nothing here needs root (the mount point is a folder the user owns).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

/// Our exports are called `medialake-<random hex>`; this tells a stale mount of ours from anyone's.
pub const EXPORT_PREFIX: &str = "medialake-";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountInfo {
    pub from: String,
    pub on: PathBuf,
    pub fstype: String,
}

impl MountInfo {
    pub fn is_ours(&self) -> bool {
        self.fstype == "nfs" && self.from.strip_prefix("127.0.0.1:/").is_some_and(|e| e.starts_with(EXPORT_PREFIX))
    }
}

pub struct MountSpec<'a> {
    pub port: u16,
    pub export: &'a str,
    pub point: &'a Path,
    pub read_only: bool,
}

/// The options of the A0 decision (`docs/a0-macos-mount.md`).
pub fn mount_options(port: u16, read_only: bool) -> String {
    let mut o = format!(
        "vers=3,tcp,locallocks,noresvport,port={port},mountport={port},rsize=1048576,wsize=1048576,\
         nobrowse,nodev,nosuid,soft,intr,nfc,timeo=20,retrans=3,deadtimeout=30,actimeo=0,nonegnamecache"
    );
    if read_only {
        o.push_str(",rdonly");
    }
    o
}

#[cfg(target_os = "macos")]
pub fn mounts() -> Vec<MountInfo> {
    use std::ffi::CStr;
    // getfsstat fills a buffer we own (getmntinfo returns a shared static one that another thread's call
    // reallocates). MNT_NOWAIT: answer from the kernel's cache, never ask a (possibly dead) file system.
    for _ in 0..4 {
        // SAFETY: a null buffer asks only for the number of mounts.
        let n = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
        if n <= 0 {
            return vec![];
        }
        let cap = n as usize + 8;
        // SAFETY: statfs is plain data; all-zero is a valid value.
        let mut buf: Vec<libc::statfs> = vec![unsafe { std::mem::zeroed() }; cap];
        // SAFETY: the buffer has room for `cap` entries and the size passed is its size in bytes.
        let got = unsafe { libc::getfsstat(buf.as_mut_ptr(), (cap * std::mem::size_of::<libc::statfs>()) as libc::c_int, libc::MNT_NOWAIT) };
        if got < 0 {
            return vec![];
        }
        let got = got as usize;
        if got >= cap {
            continue; // the table grew meanwhile
        }
        return buf
            .iter()
            .take(got)
            .map(|s| {
                // SAFETY: the name fields are NUL-terminated C strings inside the struct.
                let (fstype, on, from) = unsafe {
                    (
                        CStr::from_ptr(s.f_fstypename.as_ptr()).to_string_lossy().into_owned(),
                        CStr::from_ptr(s.f_mntonname.as_ptr()).to_string_lossy().into_owned(),
                        CStr::from_ptr(s.f_mntfromname.as_ptr()).to_string_lossy().into_owned(),
                    )
                };
                MountInfo { from, on: PathBuf::from(on), fstype }
            })
            .collect();
    }
    vec![]
}

#[cfg(not(target_os = "macos"))]
pub fn mounts() -> Vec<MountInfo> {
    vec![]
}

/// The mount at exactly `point`, if any (paths compared after resolving symlinks such as `/var`).
pub fn mount_at(point: &Path) -> Option<MountInfo> {
    let canon = point.canonicalize().unwrap_or_else(|_| point.to_path_buf());
    mounts().into_iter().find(|m| m.on == canon || m.on == point)
}

#[cfg(unix)]
pub fn unmount(point: &Path, force: bool) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(point.as_os_str().as_bytes())?;
    #[cfg(target_os = "macos")]
    let flags = if force { libc::MNT_FORCE } else { 0 };
    #[cfg(target_os = "macos")]
    // SAFETY: `c` is a valid NUL-terminated path.
    let rc = unsafe { libc::unmount(c.as_ptr(), flags) };
    #[cfg(not(target_os = "macos"))]
    // SAFETY: `c` is a valid NUL-terminated path.
    let rc = unsafe { libc::umount(c.as_ptr()) };
    if rc != 0 {
        let e = std::io::Error::last_os_error();
        bail!("cannot unmount {}: {e}", point.display());
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn unmount(_point: &Path, _force: bool) -> Result<()> {
    bail!("not supported on this platform")
}

/// Unmounts what we mounted at `point`: politely, then by force (open files do not keep a mount of
/// a file system whose server we are about to stop).
pub fn unmount_ours(point: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match unmount(point, false) {
            Ok(()) => return Ok(()),
            Err(_) if mount_at(point).is_none() => return Ok(()),
            Err(e) => {
                if Instant::now() >= deadline {
                    return unmount(point, true).map_err(|e2| anyhow::anyhow!("{e}; forced: {e2}"));
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    }
}

/// Force-unmounts every mount of ours that is still listed (a crash or force-quit left them). Called at
/// start, when no server of ours can be alive. Returns the mount points.
pub fn cleanup_stale() -> Vec<PathBuf> {
    cleanup_stale_in(Path::new("/"))
}

/// Like [`cleanup_stale`], for mounts at or below `parent` only.
pub fn cleanup_stale_in(parent: &Path) -> Vec<PathBuf> {
    let parent = parent.canonicalize().unwrap_or_else(|_| parent.to_path_buf());
    let mut gone = vec![];
    for m in mounts().into_iter().filter(|m| m.is_ours() && m.on.starts_with(&parent)) {
        if unmount(&m.on, true).is_ok() {
            gone.push(m.on);
        }
    }
    gone
}

/// Mounts `127.0.0.1:/<export>` on `point`.
pub fn mount(spec: &MountSpec<'_>) -> Result<()> {
    if cfg!(not(target_os = "macos")) {
        bail!("the mount frontend is only available on macOS");
    }
    if let Some(m) = mount_at(spec.point) {
        if m.is_ours() {
            let _ = unmount(spec.point, true);
        } else {
            bail!("{} is already used by another volume ({} on {})", spec.point.display(), m.from, m.fstype);
        }
    }
    let mut child = std::process::Command::new("/sbin/mount_nfs")
        .arg("-o")
        .arg(mount_options(spec.port, spec.read_only))
        .arg(format!("127.0.0.1:/{}", spec.export))
        .arg(spec.point)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match child.try_wait()? {
            Some(status) => {
                if status.success() {
                    return Ok(());
                }
                let mut err = String::new();
                if let Some(mut s) = child.stderr.take() {
                    use std::io::Read;
                    let _ = s.read_to_string(&mut err);
                }
                bail!("mount_nfs failed ({status}): {}", err.trim());
            }
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                bail!("mount_nfs did not finish within 30 s");
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Which mounts were made by which process, so that a later start can tell a crash's leftovers from a
/// mount that another running instance owns.
pub struct Registry {
    file: PathBuf,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
struct Record {
    point: PathBuf,
    pid: u32,
}

impl Registry {
    pub fn new(file: PathBuf) -> Registry {
        Registry { file }
    }

    fn read(&self) -> Vec<Record> {
        std::fs::read_to_string(&self.file).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    fn write(&self, v: &[Record]) {
        if let Some(p) = self.file.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        let tmp = self.file.with_extension("tmp");
        if serde_json::to_string(v).ok().and_then(|s| std::fs::write(&tmp, s).ok()).is_some() {
            let _ = std::fs::rename(&tmp, &self.file);
        }
    }

    pub fn record(&self, point: &Path) {
        let mut v = self.read();
        v.retain(|r| r.point != point);
        v.push(Record { point: point.to_path_buf(), pid: std::process::id() });
        self.write(&v);
    }

    pub fn forget(&self, point: &Path) {
        let mut v = self.read();
        v.retain(|r| r.point != point);
        self.write(&v);
    }

    /// Force-unmounts what a dead process left mounted. Returns the mount points.
    pub fn cleanup_dead(&self) -> Vec<PathBuf> {
        let mut keep = vec![];
        let mut gone = vec![];
        for r in self.read() {
            if r.pid == std::process::id() || pid_alive(r.pid) {
                keep.push(r);
                continue;
            }
            if mount_at(&r.point).is_some_and(|m| m.is_ours()) && unmount(&r.point, true).is_ok() {
                gone.push(r.point.clone());
            }
        }
        self.write(&keep);
        gone
    }
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn pid_alive(_pid: u32) -> bool {
    true
}

/// A mount point must be a folder we may mount on: it exists (or is created), is a directory, and holds
/// nothing but ignorable junk.
pub fn prepare_mount_point(point: &Path) -> Result<()> {
    std::fs::create_dir_all(point)?;
    if mount_at(point).is_some_and(|m| !m.is_ours()) {
        bail!("{} is already a mount point of another volume", point.display());
    }
    if mount_at(point).is_none() {
        let mut stray = vec![];
        for e in std::fs::read_dir(point)?.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !medialake_proto::sync::is_ignored(&name, &[]) {
                stray.push(name);
            }
        }
        if !stray.is_empty() {
            bail!("{} is not empty ({}); the mount needs an empty folder", point.display(), stray.iter().take(3).cloned().collect::<Vec<_>>().join(", "));
        }
    }
    Ok(())
}
