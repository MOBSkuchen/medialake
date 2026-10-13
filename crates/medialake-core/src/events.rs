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
    /// A joined sync changed state: `idle`, `syncing`, `paused`, `conflict`, `held` (deletes waiting
    /// for confirmation) or `error`.
    SyncState { sync_id: String, name: String, state: String, detail: String },
    /// Transfers of the last round.
    SyncProgress { sync_id: String, downloaded: u64, uploaded: u64, pushed: u64 },
    /// A conflict is waiting for a decision (policy "ask").
    SyncConflict { sync_id: String, name: String, path: String },
    SyncError { sync_id: String, message: String },
    /// Something the Sync page shows changed without a folder changing state: `hosts` (a paired computer
    /// was renamed or removed this device) or `access` (this device may now only read a folder, or
    /// write to it again). The page reads the status again.
    SyncChanged { what: String, sync_id: String },
}
