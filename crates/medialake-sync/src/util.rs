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
