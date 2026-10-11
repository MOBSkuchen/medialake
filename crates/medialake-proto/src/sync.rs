//! File sync protocol (`SyncRequest` over the normal `Action`, plus three persistent calls) and the
//! cross-platform path rules both sides enforce.
//!
//! Sync has its own version number, exchanged in `SyncRequest::Hello`; `PROTO_VERSION` stays 1.

use bier_derive::{Deserialize, Serialize};
use bierpc::serialize::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use unicode_normalization::UnicodeNormalization;

pub const SYNC_PROTO_VERSION: u32 = 1;
/// Largest content frame of a download or upload.
pub const MAX_FRAME: usize = 4 * 1024 * 1024;
/// Entries per `Changes` page at most (the host clamps the requested limit).
pub const MAX_CHANGES_PAGE: u32 = 2000;
/// Seconds between heartbeats on a notification stream.
pub const HEARTBEAT_SECS: u64 = 30;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    ReadOnly,
    ReadWrite,
}

impl Access {
    pub fn as_str(self) -> &'static str {
        match self {
            Access::ReadOnly => "ro",
            Access::ReadWrite => "rw",
        }
    }
    pub fn parse(s: &str) -> Option<Access> {
        match s {
            "ro" => Some(Access::ReadOnly),
            "rw" => Some(Access::ReadWrite),
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
}

/// One file or directory of a sync as the host knows it (tombstones included).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Entry {
    pub path: String,
    pub kind: EntryKind,
    pub size: u64,
    /// Milliseconds since the Unix epoch. Data only: nothing compares clocks between machines.
    pub mtime: i64,
    /// BLAKE3 (hex) for files, empty for directories and tombstones.
    pub hash: String,
    pub exec: bool,
    /// The host's sequence number when this entry last changed.
    pub rev: u64,
    pub deleted: bool,
    pub author: String,
}

/// `base_rev` is the member's base for the path (0 = "I believe it does not exist"). The host accepts a
/// change only if it equals the entry's current `rev`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum Change {
    Put { path: String, hash: String, size: u64, mtime: i64, exec: bool, base_rev: u64 },
    Delete { path: String, base_rev: u64 },
    Mkdir { path: String },
    Move { from: String, to: String, base_rev: u64 },
}

impl Change {
    pub fn path(&self) -> &str {
        match self {
            Change::Put { path, .. } | Change::Delete { path, .. } | Change::Mkdir { path } => path,
            Change::Move { to, .. } => to,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictReason {
    /// The entry changed on the host since the member's base.
    BaseMismatch,
    /// Another live entry has the same name up to case / normalisation.
    CaseCollision,
    /// A directory delete found live children.
    NotEmpty,
    /// A parent of the path is a file.
    ParentIsFile,
    /// A move target already exists.
    TargetExists,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ChangeResult {
    Ok { rev: u64 },
    Conflict { reason: ConflictReason, current: Option<Entry> },
    Invalid { message: String },
    /// Commit only: the content for this hash was never uploaded (or was dropped).
    MissingContent,
    /// Read-only access, or no grant.
    Denied,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Verdict {
    pub results: Vec<ChangeResult>,
    /// Hashes the host does not have yet.
    pub need: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SyncInfo {
    pub id: String,
    pub name: String,
    pub access: Access,
    pub files: u64,
    pub bytes: u64,
    pub backup: String,
    pub seq: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct VersionInfo {
    pub rev: u64,
    pub hash: String,
    pub size: u64,
    pub mtime: i64,
    pub author: String,
    /// Seconds since the Unix epoch.
    pub created: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SnapshotInfo {
    pub id: u64,
    pub seq: u64,
    pub created: i64,
    pub entries: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum RestoreWhat {
    /// One file at a revision listed by `Versions`.
    File { path: String, rev: u64 },
    /// A snapshot; `path` limits it to a file or a folder (empty = everything). With `prune`, files
    /// below `path` that are not in the snapshot are deleted as well (they stay in the history).
    Snapshot { id: u64, path: String, prune: bool },
}

#[derive(Serialize, Deserialize, Debug)]
pub enum SyncRequest {
    Hello { version: u32 },
    ListSyncs,
    /// `cursor` 0 asks for the full live tree. A non-zero cursor older than the host's floor, or an
    /// `epoch` that is not the host's, is answered with `ResetRequired`.
    Changes { sync: String, epoch: String, cursor: u64, limit: u32 },
    Propose { sync: String, changes: Vec<Change> },
    Commit { sync: String, changes: Vec<Change> },
    Versions { sync: String, path: String },
    Snapshots { sync: String },
    Restore { sync: String, what: RestoreWhat, as_copy: bool },
}

#[derive(Serialize, Deserialize, Debug)]
pub enum SyncResponse {
    Host { host_id: String, name: String, version: u32 },
    Syncs(Vec<SyncInfo>),
    Changes { epoch: String, seq: u64, entries: Vec<Entry>, next_cursor: u64, more: bool },
    ResetRequired { epoch: String },
    Verdict(Verdict),
    Committed { results: Vec<ChangeResult> },
    Versions(Vec<VersionInfo>),
    Snapshots(Vec<SnapshotInfo>),
    Denied(String),
    Error(String),
}

/// Persistent calls (bierpc `call_persistent`). Content travels as raw frames, see [`write_frame`].
#[derive(Serialize, Deserialize, Debug)]
pub enum StreamAction {
    /// Host writes `u8 status (0 ok, 1 stale, 2 not found)`, then if ok `u64 total` and frames of the
    /// requested range ending with an empty frame. `len` 0 = to the end.
    Download { auth: String, sync: String, path: String, rev: u64, offset: u64, len: u64 },
    /// Host writes `u64 have` (bytes of `hash` it already holds, to resume from); the member then sends
    /// frames from that offset ending with an empty frame. The host keeps the content only if the hash
    /// matches.
    Upload { auth: String, sync: String, hash: String, size: u64 },
    /// Host writes `Notice`s: `Changed` whenever a sync advances, a heartbeat every 30 s.
    Subscribe { auth: String, syncs: Vec<SubscribeItem> },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SubscribeItem {
    pub sync: String,
    pub cursor: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Notice {
    /// Empty = heartbeat.
    pub sync: String,
    pub seq: u64,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub enum StreamResponse {
    Done,
    Stale,
    NotFound,
    HashMismatch,
    Denied(String),
    Error(String),
}

pub const DL_OK: u8 = 0;
pub const DL_STALE: u8 = 1;
pub const DL_NOT_FOUND: u8 = 2;

// ---------------- raw framing ----------------

/// Top bit of a frame's length: the payload is a zstd frame of at most [`MAX_FRAME`] bytes.
const FRAME_ZSTD: u32 = 0x8000_0000;
/// Frames below this are never worth compressing.
const MIN_COMPRESS: usize = 4096;
const ZSTD_LEVEL: i32 = 1;

/// `u32` big-endian length, then the bytes. An empty frame ends a stream.
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, data: &[u8]) -> std::io::Result<()> {
    w.write_all(&(data.len() as u32).to_be_bytes()).await?;
    w.write_all(data).await
}

/// Writes content frames, compressing them with zstd while that pays off. Media is mostly
/// incompressible: after a few frames that do not shrink by at least 10 % the writer stops trying for
/// the rest of the stream, so such transfers cost no CPU.
pub struct FrameWriter {
    misses: u32,
    out: Vec<u8>,
}

impl Default for FrameWriter {
    fn default() -> Self {
        FrameWriter { misses: 0, out: Vec::new() }
    }
}

impl FrameWriter {
    pub fn new() -> FrameWriter {
        FrameWriter::default()
    }

    /// A writer that never compresses.
    pub fn plain() -> FrameWriter {
        FrameWriter { misses: u32::MAX, out: Vec::new() }
    }

    pub async fn write<W: AsyncWrite + Unpin>(&mut self, w: &mut W, data: &[u8]) -> std::io::Result<()> {
        if data.len() >= MIN_COMPRESS && self.misses < 3 {
            self.out.clear();
            if let Ok(c) = zstd::bulk::compress(data, ZSTD_LEVEL) {
                if c.len() < data.len() / 10 * 9 {
                    self.misses = 0;
                    w.write_all(&(c.len() as u32 | FRAME_ZSTD).to_be_bytes()).await?;
                    return w.write_all(&c).await;
                }
            }
            self.misses += 1;
        }
        write_frame(w, data).await
    }
}

/// Reads one frame into `buf` (replacing its content; compressed frames are inflated, never beyond
/// [`MAX_FRAME`]). Returns the length; 0 = end of stream.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R, buf: &mut Vec<u8>) -> std::io::Result<usize> {
    let mut hdr = [0u8; 4];
    r.read_exact(&mut hdr).await?;
    let raw = u32::from_be_bytes(hdr);
    let compressed = raw & FRAME_ZSTD != 0;
    let len = (raw & !FRAME_ZSTD) as usize;
    if len > MAX_FRAME {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "frame too large"));
    }
    buf.clear();
    buf.resize(len, 0);
    r.read_exact(buf).await?;
    if compressed {
        let plain = zstd::bulk::decompress(buf, MAX_FRAME)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("bad compressed frame: {e}")))?;
        *buf = plain;
    }
    Ok(buf.len())
}

pub async fn write_u64<W: AsyncWrite + Unpin>(w: &mut W, v: u64) -> std::io::Result<()> {
    w.write_all(&v.to_be_bytes()).await
}

pub async fn read_u64<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b).await?;
    Ok(u64::from_be_bytes(b))
}

// ---------------- path rules (§4.6) ----------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    Empty,
    Absolute,
    BadComponent(String),
    ControlChar,
    TooLong,
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PathError::Empty => write!(f, "empty path"),
            PathError::Absolute => write!(f, "path must be relative"),
            PathError::BadComponent(c) => write!(f, "invalid path component {c:?}"),
            PathError::ControlChar => write!(f, "path contains a NUL or control character"),
            PathError::TooLong => write!(f, "path is too long"),
        }
    }
}

impl std::error::Error for PathError {}

pub const MAX_PATH_BYTES: usize = 4096;
pub const MAX_NAME_BYTES: usize = 255;

/// Validates a sync path and returns it NFC-normalised. Relative, `/`-separated, no empty / `.` / `..`
/// components, no NUL or control characters.
pub fn normalize_path(p: &str) -> Result<String, PathError> {
    if p.is_empty() {
        return Err(PathError::Empty);
    }
    if p.starts_with('/') {
        return Err(PathError::Absolute);
    }
    if p.chars().any(|c| c.is_control()) {
        return Err(PathError::ControlChar);
    }
    let nfc: String = p.nfc().collect();
    if nfc.len() > MAX_PATH_BYTES {
        return Err(PathError::TooLong);
    }
    for comp in nfc.split('/') {
        if comp.is_empty() || comp == "." || comp == ".." || comp.len() > MAX_NAME_BYTES {
            return Err(PathError::BadComponent(comp.to_string()));
        }
    }
    Ok(nfc)
}

/// The case-insensitive identity of a path: two live entries may not share it.
pub fn path_key(p: &str) -> String {
    p.nfc().collect::<String>().to_lowercase()
}

pub fn parent_of(p: &str) -> Option<&str> {
    p.rsplit_once('/').map(|(a, _)| a)
}

pub fn name_of(p: &str) -> &str {
    p.rsplit_once('/').map(|(_, n)| n).unwrap_or(p)
}

/// True if `path` is `dir` or lies below it (empty `dir` = everything).
pub fn is_within(path: &str, dir: &str) -> bool {
    dir.is_empty() || path == dir || (path.len() > dir.len() && path.starts_with(dir) && path.as_bytes()[dir.len()] == b'/')
}

const IGNORED_NAMES: &[&str] = &[".medialake-sync", ".DS_Store", "Thumbs.db", "desktop.ini"];

/// Built-in ignore rules (`.medialake-sync/`, `.DS_Store`, `._*`, `Thumbs.db`, `desktop.ini`, `~$*`,
/// `*.tmp`, `*.part`, `.~lock.*`) applied to every component, plus `extra` glob patterns (`*`, `?`)
/// matched against each component and, for patterns containing `/`, against the whole path.
pub fn is_ignored(path: &str, extra: &[String]) -> bool {
    for comp in path.split('/') {
        let lower = comp.to_ascii_lowercase();
        if IGNORED_NAMES.iter().any(|n| n.eq_ignore_ascii_case(comp))
            || comp.starts_with("._")
            || comp.starts_with("~$")
            || comp.starts_with(".~lock.")
            || lower.ends_with(".tmp")
            || lower.ends_with(".part")
        {
            return true;
        }
        if extra.iter().any(|g| !g.contains('/') && glob_match(g, comp)) {
            return true;
        }
    }
    extra.iter().any(|g| g.contains('/') && glob_match(g, path))
}

/// `*` (any run, also across `/`) and `?` (one character).
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti, mut star, mut mark) = (0usize, 0usize, None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Names a Windows member cannot represent. Valid in the sync; such a member lists the entry as
/// "cannot be synced to this device" and skips it.
pub fn unrepresentable_on_windows(path: &str) -> bool {
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4",
        "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    path.split('/').any(|c| {
        if c.chars().any(|ch| "<>:\"|?*\\".contains(ch) || ch.is_control()) || c.ends_with('.') || c.ends_with(' ') {
            return true;
        }
        let stem = c.split('.').next().unwrap_or(c).to_ascii_uppercase();
        RESERVED.contains(&stem.as_str())
    })
}

/// 64 lowercase hex characters.
pub fn is_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `name (conflict, <device>, 2026-10-11 14-03).ext` (UTC): the loser of a conflict, kept next to the winner.
pub fn conflict_name(path: &str, device: &str, unix_secs: i64) -> String {
    let (dir, name) = match path.rsplit_once('/') {
        Some((d, n)) => (Some(d), n),
        None => (None, path),
    };
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    let device: String = device.chars().filter(|c| !c.is_control() && !"/\\:*?\"<>|".contains(*c)).take(32).collect();
    let (y, mo, d, h, mi) = civil(unix_secs);
    let n = format!("{stem} (conflict, {device}, {y:04}-{mo:02}-{d:02} {h:02}-{mi:02}){ext}");
    match dir {
        Some(d) => format!("{d}/{n}"),
        None => n,
    }
}

/// UTC civil time from Unix seconds (Howard Hinnant's algorithm).
pub fn civil(secs: i64) -> (i64, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, (rem / 3600) as u32, ((rem % 3600) / 60) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_to_nfc() {
        let decomposed = "Cafe\u{301}/menu.txt";
        assert_eq!(normalize_path(decomposed).unwrap(), "Caf\u{e9}/menu.txt");
    }

    #[test]
    fn rejects_bad_paths() {
        for p in ["", "/abs", "a//b", "a/./b", "a/../b", "a/\0b", "a/"] {
            assert!(normalize_path(p).is_err(), "{p:?}");
        }
        assert!(normalize_path("a/b c/d.txt").is_ok());
    }

    #[test]
    fn case_keys() {
        assert_eq!(path_key("Report.TXT"), path_key("report.txt"));
        assert_eq!(path_key("Cafe\u{301}"), path_key("Caf\u{c9}"));
        assert_ne!(path_key("a/b"), path_key("a/c"));
    }

    #[test]
    fn ignore_rules() {
        assert!(is_ignored(".medialake-sync/state.db", &[]));
        assert!(is_ignored("a/.DS_Store", &[]));
        assert!(is_ignored("a/._x", &[]));
        assert!(is_ignored("a/~$doc.docx", &[]));
        assert!(is_ignored("a/b.TMP", &[]));
        assert!(is_ignored("a/b.part", &[]));
        assert!(is_ignored("a/.~lock.x#", &[]));
        assert!(!is_ignored("a/b.txt", &[]));
        assert!(is_ignored("a/b.txt", &["*.txt".into()]));
        assert!(is_ignored("build/out/x", &["build/*".into()]));
    }

    #[test]
    fn windows_names() {
        assert!(unrepresentable_on_windows("a/CON.txt"));
        assert!(unrepresentable_on_windows("a/b?"));
        assert!(unrepresentable_on_windows("trailing."));
        assert!(unrepresentable_on_windows("trailing /x"));
        assert!(!unrepresentable_on_windows("a/console.txt"));
    }

    #[test]
    fn conflict_names() {
        // 2026-10-11 14:03:00 UTC
        let t = 1_791_727_380;
        assert_eq!(civil(t), (2026, 10, 11, 14, 3));
        assert_eq!(conflict_name("a/b.txt", "Mac", t), "a/b (conflict, Mac, 2026-10-11 14-03).txt");
        assert_eq!(conflict_name("README", "Mac", t), "README (conflict, Mac, 2026-10-11 14-03)");
        assert_eq!(conflict_name(".hidden", "Mac", t), ".hidden (conflict, Mac, 2026-10-11 14-03)");
    }

    #[test]
    fn frames_roundtrip_compressed_and_plain() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(async {
            let text = "the quick brown fox ".repeat(2000).into_bytes();
            let noise: Vec<u8> = (0..20_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
            let mut wire: Vec<u8> = vec![];
            let mut fw = FrameWriter::new();
            fw.write(&mut wire, &text).await.unwrap();
            fw.write(&mut wire, &noise).await.unwrap();
            fw.write(&mut wire, b"tiny").await.unwrap();
            write_frame(&mut wire, &[]).await.unwrap();
            assert!(wire.len() < text.len() / 2 + noise.len() + 100, "text frame was compressed");
            let mut r = &wire[..];
            let mut buf = Vec::new();
            assert_eq!(read_frame(&mut r, &mut buf).await.unwrap(), text.len());
            assert_eq!(buf, text);
            assert_eq!(read_frame(&mut r, &mut buf).await.unwrap(), noise.len());
            assert_eq!(buf, noise);
            assert_eq!(read_frame(&mut r, &mut buf).await.unwrap(), 4);
            assert_eq!(read_frame(&mut r, &mut buf).await.unwrap(), 0);
        });
    }

    #[test]
    fn incompressible_streams_stop_trying() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(async {
            let mut x = 0x9E37_79B9_7F4A_7C15u64;
            let noise: Vec<u8> = (0..65_536)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    (x >> 24) as u8
                })
                .collect();
            let mut wire: Vec<u8> = vec![];
            let mut fw = FrameWriter::new();
            for _ in 0..5 {
                fw.write(&mut wire, &noise).await.unwrap();
            }
            assert_eq!(fw.misses, 3, "gave up after three misses");
            assert_eq!(wire.len(), 5 * (noise.len() + 4));
        });
    }

    #[test]
    fn inflating_is_bounded() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(async {
            // 64 MiB of zeros compress to almost nothing; a reader must refuse to inflate it
            let bomb = zstd::bulk::compress(&vec![0u8; 64 << 20], 1).unwrap();
            let mut wire = ((bomb.len() as u32) | FRAME_ZSTD).to_be_bytes().to_vec();
            wire.extend_from_slice(&bomb);
            let mut buf = Vec::new();
            assert!(read_frame(&mut &wire[..], &mut buf).await.is_err());
        });
    }

    #[test]
    fn within() {
        assert!(is_within("a/b", "a"));
        assert!(is_within("a", "a"));
        assert!(!is_within("ab", "a"));
        assert!(is_within("x", ""));
    }
}
