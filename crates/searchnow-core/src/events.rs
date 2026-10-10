//! Events pushed from the core to the UI (serialised as JSON across the FFI boundary).

use serde::Serialize;

#[derive(Serialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Indexing progress. `queued` = still waiting; `total` = files in the current batch.
    Progress { queued: u64, done: u64, total: u64, current: Option<String>, eta_secs: Option<u64> },
    /// Local/remote embedding server state: idle, starting, ready, failed.
    Server { state: String, detail: String, backend: String, quality: String },
    /// Model download progress (bytes).
    Download { file: String, done: u64, total: u64 },
    /// Something the user should know about.
    Error { message: String },
    /// Indexing of the whole queue finished.
    Idle { files: u64, chunks: u64 },
}
