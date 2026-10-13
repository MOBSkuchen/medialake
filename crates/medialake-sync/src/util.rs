//! Small helpers shared by the host and the member.

use std::io::{Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("system random source");
    b
}

pub fn random_id() -> String {
    medialake_proto::pairing::hex(&random_bytes::<16>())
}

/// A path as people write it: without the `\\?\` that `canonicalize` puts in front of a drive path on
/// Windows (it would be stored in the configuration and shown in every list). UNC paths keep theirs.
pub fn plain_path(p: &Path) -> std::path::PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => std::path::PathBuf::from(rest),
        _ => p.to_path_buf(),
    }
}

pub fn mtime_ms(m: &std::fs::Metadata) -> i64 {
    m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64).unwrap_or(0)
}

pub fn set_mtime_ms(path: &Path, ms: i64) -> std::io::Result<()> {
    let t = UNIX_EPOCH + std::time::Duration::from_millis(ms.max(0) as u64);
    let f = std::fs::OpenOptions::new().write(true).open(path)?;
    f.set_modified(t)
}

#[cfg(unix)]
pub fn is_exec(m: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    m.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
pub fn is_exec(_m: &std::fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
pub fn set_exec(path: &Path, exec: bool) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let m = std::fs::metadata(path)?;
    let mode = m.permissions().mode();
    let new = if exec { mode | ((mode & 0o444) >> 2) } else { mode & !0o111 };
    if new != mode {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(new))?;
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn set_exec(_path: &Path, _exec: bool) -> std::io::Result<()> {
    Ok(())
}

/// BLAKE3 of a whole file (hex) and its length.
pub fn hash_file(path: &Path) -> std::io::Result<(String, u64)> {
    let mut f = std::fs::File::open(path)?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        total += n as u64;
    }
    Ok((h.finalize().to_hex().to_string(), total))
}

/// Copies `src` to `dst` while hashing what was written.
pub fn copy_hashing(src: &Path, dst: &Path) -> std::io::Result<(String, u64)> {
    let mut r = std::fs::File::open(src)?;
    let mut w = std::fs::File::create(dst)?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        w.write_all(&buf[..n])?;
        total += n as u64;
    }
    w.flush()?;
    Ok((h.finalize().to_hex().to_string(), total))
}

/// Copy-on-write clone where the file system supports it (APFS). Returns false if not possible.
#[cfg(target_os = "macos")]
pub fn try_clone(src: &Path, dst: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let (Ok(s), Ok(d)) = (std::ffi::CString::new(src.as_os_str().as_bytes()), std::ffi::CString::new(dst.as_os_str().as_bytes())) else {
        return false;
    };
    // SAFETY: both pointers are valid NUL-terminated strings for the duration of the call.
    unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), 0) == 0 }
}

#[cfg(not(target_os = "macos"))]
pub fn try_clone(_src: &Path, _dst: &Path) -> bool {
    false
}

/// Moves a file, falling back to copy + delete across volumes.
pub fn move_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    match std::fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(_) => {
            let tmp = dst.with_extension("medialake-move.part");
            std::fs::copy(src, &tmp)?;
            std::fs::rename(&tmp, dst)?;
            std::fs::remove_file(src)
        }
    }
}

/// Paces transfers to a rate shared by all of them (0 = unlimited). Callers `throttle` after (or
/// before) moving `n` bytes; the limiter spreads them evenly over time.
pub struct RateLimiter {
    bytes_per_sec: std::sync::atomic::AtomicU64,
    next: std::sync::Mutex<std::time::Instant>,
}

impl RateLimiter {
    /// `kbps` = kilobits per second, like the settings.
    pub fn new(kbps: u64) -> RateLimiter {
        let r = RateLimiter { bytes_per_sec: Default::default(), next: std::sync::Mutex::new(std::time::Instant::now()) };
        r.set_kbps(kbps);
        r
    }

    pub fn set_kbps(&self, kbps: u64) {
        self.bytes_per_sec.store(kbps.saturating_mul(1000) / 8, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn bytes_per_sec(&self) -> u64 {
        self.bytes_per_sec.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// A buffer size that keeps the pacing smooth (about a quarter second of data), at most `max`.
    pub fn chunk(&self, max: usize) -> usize {
        match self.bytes_per_sec() {
            0 => max,
            r => ((r / 4) as usize).clamp(4096, max),
        }
    }

    pub async fn throttle(&self, n: usize) {
        let rate = self.bytes_per_sec();
        if rate == 0 || n == 0 {
            return;
        }
        let wait = {
            let mut next = self.next.lock().unwrap();
            let now = std::time::Instant::now();
            let start = (*next).max(now);
            *next = start + std::time::Duration::from_secs_f64(n as f64 / rate as f64);
            start.saturating_duration_since(now)
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
}
