//! Types shared by the Windows implementation and the frontend.

use std::path::Path;
use std::sync::Arc;

/// What the platform stores with every placeholder (`FileIdentity`, at most 4 KiB) and hands back in every
/// callback: the host revision and the content hash the placeholder stands for. With it the sync engine
/// never has to read a clean placeholder to know what is in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub rev: u64,
    pub hash: String,
}

const MAGIC: &[u8; 4] = b"MLK1";

impl Identity {
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(12 + self.hash.len());
        v.extend_from_slice(MAGIC);
        v.extend_from_slice(&self.rev.to_le_bytes());
        v.extend_from_slice(self.hash.as_bytes());
        v
    }

    pub fn decode(b: &[u8]) -> Option<Identity> {
        if b.len() < 12 || &b[..4] != MAGIC {
            return None;
        }
        let rev = u64::from_le_bytes(b[4..12].try_into().ok()?);
        let hash = std::str::from_utf8(&b[12..]).ok()?.to_string();
        if hash.len() > 128 || !hash.bytes().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        Some(Identity { rev, hash })
    }
}

/// Explorer's "Always keep on this device" / "Free up space" as the platform stores them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Pin {
    #[default]
    Unspecified,
    Pinned,
    Unpinned,
    Excluded,
    Inherit,
}

/// What a placeholder is created or updated to look like.
#[derive(Debug, Clone)]
pub struct PlaceholderInfo {
    pub is_dir: bool,
    pub size: u64,
    /// Milliseconds since the Unix epoch.
    pub mtime_ms: i64,
    pub read_only: bool,
    pub identity: Option<Identity>,
    /// The content equals the host's: Explorer shows the "synced" state.
    pub in_sync: bool,
}

/// A file as the platform sees it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileState {
    pub exists: bool,
    pub is_dir: bool,
    /// A cloud placeholder (dehydrated, partial or hydrated), not a plain file.
    pub placeholder: bool,
    pub in_sync: bool,
    /// Not all of the content is on this computer.
    pub dehydrated: bool,
    pub pin: Pin,
    pub identity: Option<Identity>,
    pub size: u64,
    pub attributes: u32,
}

/// How a request failed; it decides the status that Explorer gets back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloudError {
    /// The host cannot be reached: Explorer's "network unavailable", at once instead of after a timeout.
    Offline(String),
    /// The content is not on the host any more (or not at that revision).
    NotFound(String),
    Failed(String),
}

impl std::fmt::Display for CloudError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CloudError::Offline(m) | CloudError::NotFound(m) | CloudError::Failed(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for CloudError {}

pub type CloudResult<T> = Result<T, CloudError>;

/// A request for content, as the platform makes it (FETCH_DATA).
#[derive(Debug, Clone)]
pub struct FetchRequest {
    /// Path inside the sync root, `/`-separated.
    pub rel: String,
    pub identity: Option<Identity>,
    pub file_size: u64,
    /// The range that must be delivered. More may be sent; the platform accepts what it did not ask for.
    pub offset: u64,
    pub length: u64,
    /// The program that reads the file (its image path; empty if the platform does not say).
    pub process: String,
}

/// Where a provider puts the bytes of a [`FetchRequest`].
pub trait Transfer {
    /// Delivers `bytes` at `offset`. The platform wants 4 KiB-aligned offsets and lengths except for the
    /// piece that ends at the end of the file ([`crate::plan_chunks`] makes such pieces).
    fn data(&self, offset: u64, bytes: &[u8]) -> CloudResult<()>;
    /// Progress of the whole request, so that the platform does not give up on a slow transfer.
    fn progress(&self, total: u64, done: u64);
}

/// The side that has the content (the sync engine's transport).
pub trait Provider: Send + Sync {
    /// Serves one FETCH_DATA. Must return (with `Ok` or an error): the requesting application waits.
    fn fetch(&self, req: &FetchRequest, out: &dyn Transfer) -> CloudResult<()>;
    /// The platform cancelled the request for `rel` (the application gave up).
    fn cancel(&self, _rel: &str) {}
}

/// A live connection to a sync root; dropping it disconnects.
pub trait Connection: Send {}

/// What the root registers as.
#[derive(Debug, Clone)]
pub struct Registration {
    pub provider_name: String,
    pub provider_version: String,
    /// Stored with the root; the sync id.
    pub root_identity: Vec<u8>,
    /// A stable GUID for the provider (16 bytes, RFC 4122 field order).
    pub provider_id: [u8; 16],
}

/// The platform operations the frontend needs. Implemented over cfapi by [`crate::win::WinCloud`].
pub trait CloudApi: Send + Sync {
    /// `Ok` if Cloud Files can be used for `root` (platform present, NTFS volume).
    fn available(&self, root: &Path) -> Result<(), String>;
    fn register(&self, root: &Path, reg: &Registration) -> CloudResult<()>;
    fn connect(&self, root: &Path, provider: Arc<dyn Provider>) -> CloudResult<Box<dyn Connection>>;
    /// Removes the registration (placeholders that were not reverted stop working).
    fn unregister(&self, root: &Path) -> CloudResult<()>;

    /// Creates the placeholder `name` in the existing directory `parent`.
    fn create_placeholder(&self, parent: &Path, name: &str, info: &PlaceholderInfo) -> CloudResult<()>;
    /// New metadata and identity for an existing placeholder; `dehydrate` drops its content too.
    fn update_placeholder(&self, path: &Path, info: &PlaceholderInfo, dehydrate: bool) -> CloudResult<()>;
    /// Makes `path` an in-sync placeholder carrying `identity` (a file that was just pushed, or a
    /// downloaded one); a normal file is converted.
    fn mark_synced(&self, path: &Path, identity: &Identity) -> CloudResult<()>;

    /// The state of `path`, including the identity of a placeholder (that costs an open of the file).
    fn state(&self, path: &Path) -> CloudResult<FileState>;
    /// Like [`CloudApi::state`] without the identity: attributes only, cheap enough for a whole tree.
    fn state_light(&self, path: &Path) -> CloudResult<FileState> {
        self.state(path)
    }
    fn hydrate(&self, path: &Path) -> CloudResult<()>;
    fn dehydrate(&self, path: &Path) -> CloudResult<()>;
    fn set_pin(&self, path: &Path, pin: Pin) -> CloudResult<()>;
    /// Turns a placeholder back into a normal file (it must be hydrated).
    fn revert(&self, path: &Path) -> CloudResult<()>;
}

/// Windows file attribute bits (kept here so that platform-neutral code can read them).
pub mod attr {
    pub const READONLY: u32 = 0x1;
    pub const DIRECTORY: u32 = 0x10;
    pub const OFFLINE: u32 = 0x1000;
    pub const REPARSE_POINT: u32 = 0x400;
    pub const RECALL_ON_OPEN: u32 = 0x40000;
    pub const PINNED: u32 = 0x80000;
    pub const UNPINNED: u32 = 0x100000;
    pub const RECALL_ON_DATA_ACCESS: u32 = 0x400000;

    /// Reading the file would first have to fetch it from somewhere (OneDrive, MediaLake, …).
    pub fn would_download(attrs: u32) -> bool {
        attrs & (RECALL_ON_DATA_ACCESS | RECALL_ON_OPEN | OFFLINE) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_round_trips_and_rejects_foreign_blobs() {
        let id = Identity { rev: 0x0102_0304_0506_0708, hash: "ab12cd".into() };
        assert_eq!(Identity::decode(&id.encode()), Some(id.clone()));
        assert_eq!(Identity::decode(b""), None);
        assert_eq!(Identity::decode(b"XXXX\x01\0\0\0\0\0\0\0abc"), None);
        let mut bad = id.encode();
        bad.push(b'!');
        assert_eq!(Identity::decode(&bad), None, "a hash is hex");
        assert!(id.encode().len() < 4096);
    }

    #[test]
    fn recall_attributes_mean_a_download() {
        assert!(attr::would_download(attr::RECALL_ON_DATA_ACCESS));
        assert!(attr::would_download(attr::RECALL_ON_OPEN | attr::READONLY));
        assert!(attr::would_download(attr::OFFLINE));
        assert!(!attr::would_download(attr::PINNED | attr::READONLY));
        assert!(!attr::would_download(0));
    }
}
