//! MediaLake file sync: the host (a plain folder, watched, with history), the member engine, and the
//! frontends that show a sync on a device. Never depends on `medialake-llama` or on the search index.
//!
//! `medialake-proto::sync` has the protocol types and path rules; this crate has everything that acts
//! on them.

pub mod host;
pub mod member;
pub mod util;
pub mod watch;

pub use medialake_proto::sync::{Access, Change, ChangeResult, Entry, EntryKind};
