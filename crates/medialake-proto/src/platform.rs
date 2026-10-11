//! Platform lookups shared by the client and the server, so the two cannot drift.

use std::path::PathBuf;

/// The per-user data folder: `%LOCALAPPDATA%\MediaLake` on Windows,
/// `~/Library/Application Support/MediaLake` on macOS (`$XDG_DATA_HOME`-less Unix: `~/.local/share`).
pub fn data_dir() -> PathBuf {
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library").join("Application Support"));
    #[cfg(not(any(windows, target_os = "macos")))]
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share")));
    base.unwrap_or_else(std::env::temp_dir).join("MediaLake")
}

/// The machine name: `COMPUTERNAME` when set (Windows), else `gethostname`, else `fallback`.
pub fn host_name(fallback: &str) -> String {
    if let Ok(n) = std::env::var("COMPUTERNAME") {
        if !n.is_empty() {
            return n;
        }
    }
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: the buffer is valid for `buf.len()` bytes; gethostname NUL-terminates unless truncated.
        let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len() - 1) } == 0;
        if ok {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            if let Ok(s) = std::str::from_utf8(&buf[..end]) {
                let s = s.trim_end_matches(".local");
                if !s.is_empty() {
                    return s.to_string();
                }
            }
        }
    }
    fallback.to_string()
}
