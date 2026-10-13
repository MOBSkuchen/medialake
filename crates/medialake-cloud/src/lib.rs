//! The Windows Cloud Files frontend of file sync: a joined sync is a sync root in Explorer whose files are
//! placeholders (full size, no content) that fill in when opened.
//!
//! Everything here is written against [`medialake_cloudapi::CloudApi`], so it builds and is tested on any
//! host with a small stand-in for the platform (see `tests/`); only [`CloudSession::start`] needs Windows.
//! The design and what is assumed about the platform: `docs/w0-cloud-files.md`.
//!
//! How it meets the sync engine (the same `Frontend` trait as the mirror and the macOS mount):
//!
//! * the sync root is the folder: the engine scans it like a mirror (names, sizes and times of
//!   placeholders are real NTFS metadata, reading them never fetches anything);
//! * **a placeholder's identity (`{rev, hash}`) answers `content_hash`**, so the engine never reads a clean
//!   placeholder, not even after its state database is lost (that would download the whole sync);
//! * a remote change to a clean file updates its placeholder and frees its content, unless it is pinned;
//! * a file that was pushed (or downloaded) is converted to an in-sync placeholder, so Explorer can free
//!   it later and shows it as synced.

mod frontend;
mod provider;
mod session;

/// Windows file attribute helpers (recall-on-access detection), re-exported for the search indexer.
pub use medialake_cloudapi::attr;

pub use frontend::{BrowseEntry, CloudFrontend};
pub use provider::{HostProvider, looks_offline};
pub use session::{CloudConfig, CloudSession, leave_root, leave_root_default};
