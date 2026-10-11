//! Shared client <-> server protocol (bierpc) and model manifest.

pub mod models;
pub mod pairing;
pub mod platform;
pub mod pinning;

use bier_derive::{Deserialize, Serialize};
use bierpc::serialize::{Deserialize, Serialize};

pub const PROTO_VERSION: u32 = 1;
/// Largest single bierpc message (an audio segment or a few downscaled images are far below this).
pub const MAX_MESSAGE_BYTES: u64 = 32 * 1024 * 1024;
/// Embedding dimensionality (EmbeddingGemma 2, full size).
pub const DIMS: usize = 768;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextKind {
    /// A search query: gets the `task: search result | query: ...` prefix.
    Query,
    /// Document content: gets the `title: ... | text: ...` prefix.
    Document,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TextItem {
    /// File name (documents only; empty = `none`).
    pub title: String,
    pub text: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct VideoFrames {
    /// Encoded frames (jpeg/png) of one clip; embedded together into a single vector.
    pub frames: Vec<Vec<u8>>,
}

/// Requests. Every request carries `auth` (bierpc handlers see no per-connection context).
#[derive(Serialize, Deserialize, Debug)]
pub enum Action {
    Hello { proto_version: u32, auth: String },
    EmbedText { auth: String, kind: TextKind, items: Vec<TextItem> },
    /// Each image becomes its own vector.
    EmbedImages { auth: String, images: Vec<Vec<u8>> },
    /// Mono f32 PCM at the sample rate reported by `ServerInfo`; each segment becomes a vector.
    EmbedAudio { auth: String, segments: Vec<Vec<f32>> },
    /// Each clip (a few frames) becomes one vector.
    EmbedVideo { auth: String, clips: Vec<VideoFrames> },
    /// Remote only: exchange a one-time token for a long-term credential (phase 7).
    Pair { token: String, client_name: String },
    /// Stop the server (local child mode only).
    Shutdown { auth: String },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ServerInfo {
    pub model_id: String,
    pub quality: String,
    pub dims: u32,
    pub audio_sample_rate: u32,
    pub supports_audio: bool,
    /// "cpu" or a GPU backend name.
    pub backend: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Response {
    Info(ServerInfo),
    /// One embedding (or error text) per input item, in order.
    Embeddings(Vec<Embedding>),
    Paired { client_id: String, secret: String },
    Rejected(String),
    Ok,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Embedding {
    Vector(Vec<f32>),
    Failed(String),
}

impl Embedding {
    pub fn into_result(self) -> Result<Vec<f32>, String> {
        match self {
            Embedding::Vector(v) => Ok(v),
            Embedding::Failed(e) => Err(e),
        }
    }
}

/// What the supervisor reads on the child's stdout: `READY <port>` once the model is loaded.
pub const READY_PREFIX: &str = "READY ";

/// Applies the EmbeddingGemma task prefixes.
pub fn format_text(kind: TextKind, item: &TextItem) -> String {
    match kind {
        TextKind::Query => format!("task: search result | query: {}", item.text),
        TextKind::Document => {
            let title = if item.title.is_empty() { "none" } else { item.title.as_str() };
            format!("title: {} | text: {}", title, item.text)
        }
    }
}
