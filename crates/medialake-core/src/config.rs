//! User configuration (`config.toml`) and well-known paths.

use std::path::{Path, PathBuf};

use anyhow::Context;
use medialake_proto::models::Quality;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct IndexedDir {
    pub path: PathBuf,
    pub recursive: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ServerMode {
    #[default]
    Local,
    Remote,
}

/// Credentials of a paired remote server. The secret is stored DPAPI-protected (Windows) or as a `keychain:<id>` reference (macOS).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct RemoteCreds {
    /// `host:port`, resolved at connect time.
    pub address: String,
    /// SHA-256 of the server certificate, lowercase hex.
    pub fingerprint: String,
    pub client_id: String,
    pub secret_protected: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Config {
    pub hotkey: String,
    pub quality: Quality,
    pub server_mode: ServerMode,
    pub remote: Option<RemoteCreds>,
    /// Minimum confidence (0..1) for a result to be shown.
    pub threshold: f32,
    pub dirs: Vec<IndexedDir>,
    /// How long a file must be quiet after a modification before it is (re-)indexed.
    pub debounce_ms: u64,
    /// A modified file is only re-embedded when at least this many bytes changed.
    pub min_change_bytes: u64,
    /// Files larger than this are indexed by name/path only.
    pub max_file_bytes: u64,
    /// Experimental: embed video content (frames).
    pub video_enabled: bool,
    pub max_results: usize,
    /// Stop the local server after this many idle minutes (0 = never).
    pub idle_shutdown_minutes: u32,
    /// Raw cosine mapped to confidence 0 / 1 (calibrated, see docs/calibration.md).
    pub score_floor: f32,
    pub score_ceiling: f32,
    /// Hits more than this (raw cosine) below the best hit are dropped.
    pub relative_cut: f32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            hotkey: if cfg!(target_os = "macos") { "Alt+Space" } else { "Ctrl+Alt+Space" }.into(),
            quality: Quality::Normal,
            server_mode: ServerMode::Local,
            remote: None,
            threshold: 0.4,
            dirs: vec![],
            debounce_ms: 2_000,
            min_change_bytes: 512,
            max_file_bytes: 256 * 1024 * 1024,
            video_enabled: false,
            max_results: 20,
            idle_shutdown_minutes: 30,
            score_floor: 0.58,
            score_ceiling: 0.74,
            relative_cut: 0.06,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, toml::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// Where everything lives. `root` defaults to `%LOCALAPPDATA%\MediaLake` (macOS: `~/Library/Application Support/MediaLake`).
#[derive(Clone, Debug)]
pub struct Paths {
    pub root: PathBuf,
    pub config_file: PathBuf,
    pub db_file: PathBuf,
    pub index_file: PathBuf,
    pub models_dir: PathBuf,
    pub logs_dir: PathBuf,
}

impl Paths {
    pub fn new(root: impl Into<PathBuf>) -> Paths {
        let root = root.into();
        Paths {
            config_file: root.join("config.toml"),
            db_file: root.join("index.db"),
            index_file: root.join("vectors.usearch"),
            models_dir: root.join("models"),
            logs_dir: root.join("logs"),
            root,
        }
    }

    pub fn default_root() -> PathBuf {
        if let Ok(p) = std::env::var("MEDIALAKE_HOME") {
            return PathBuf::from(p);
        }
        medialake_proto::platform::data_dir()
    }

    pub fn ensure(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        std::fs::create_dir_all(&self.logs_dir)
    }
}

/// Strips the verbatim path prefix that `canonicalize` adds on Windows.
pub fn normalize_path(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => p.to_path_buf(),
    }
}
