//! macOS mount frontend for MediaLake file sync: an NFSv3 server on a loopback port, mounted with the
//! system's `mount_nfs`. See `docs/a0-macos-mount.md` for why.
//!
//! A sync is shown as a folder whose files exist at full size but are fetched block by block when
//! something reads them. The sync engine sees the same cache tree as sparse files; everything that a
//! plain folder cannot express (which blocks are present, what is dirty or pinned) lives in the
//! [`index`].

#[cfg(unix)]
pub mod frontend;
#[cfg(unix)]
pub mod fs;
#[cfg(unix)]
pub mod index;
#[cfg(unix)]
pub mod mountctl;
#[cfg(unix)]
pub mod session;
#[cfg(unix)]
pub mod store;

#[cfg(unix)]
pub use frontend::MountFrontend;
#[cfg(unix)]
pub use session::{MountConfig, MountSession};
#[cfg(unix)]
pub use store::{BrowseItem, Store, StoreStats};
