//! MediaLake client core: config, index, watcher, extractors, search, server supervision.

pub mod client;
pub mod config;
pub mod core;
pub mod events;
pub mod extract;
pub mod extract_ext;
#[cfg(target_os = "macos")]
pub mod apple;
pub mod video;
pub mod indexer;
pub mod models;
pub mod remote;
pub mod search;
pub mod store;
pub mod supervisor;
pub mod vindex;
pub mod watcher;

pub use crate::core::Core;
