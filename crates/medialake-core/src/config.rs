//! User configuration (`config.toml`) and well-known paths.

use std::path::{Path, PathBuf};

use anyhow::Context;
use medialake_proto::models::{Model, Quality};
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

/// A host this device is paired with for sync. The secret is protected like `RemoteCreds::secret_protected`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct SyncHostCfg {
    pub id: String,
    pub name: String,
    pub address: String,
    pub fingerprint: String,
    pub client_id: String,
    pub secret_protected: String,
}

/// A sync this device is a member of.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct SyncFolderCfg {
    pub sync_id: String,
    pub host_id: String,
    pub name: String,
    pub path: PathBuf,
    /// `auto` = Cloud Files / mount where available; `mirror` = a plain folder with full copies.
    pub frontend: String,
    /// `server_wins` or `ask`
    pub conflict: String,
    /// `rw` or `ro`, as granted by the host when joining.
    pub access: String,
    pub cache_limit_gb: u64,
    pub index_for_search: bool,
    pub paused: bool,
}

impl Default for SyncFolderCfg {
    fn default() -> Self {
        SyncFolderCfg {
            sync_id: String::new(),
            host_id: String::new(),
            name: String::new(),
            path: PathBuf::new(),
            frontend: "auto".into(),
            conflict: "server_wins".into(),
            access: "rw".into(),
            cache_limit_gb: 10,
            index_for_search: true,
            paused: false,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct SyncHostingCfg {
    pub bind: String,
    /// Empty = this computer's name.
    pub public_addr: String,
}

impl Default for SyncHostingCfg {
    fn default() -> Self {
        SyncHostingCfg { bind: "0.0.0.0:47900".into(), public_addr: String::new() }
    }
}

/// File sync (§9). Off by default: an existing install behaves exactly as before.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct SyncConfig {
    pub enabled: bool,
    /// Shown in conflict names and on the host. Empty = this computer's name.
    pub device_name: String,
    pub debounce_ms: u64,
    /// 0 = unlimited. (Stored; enforcement is part of the polish phase.)
    pub upload_limit_kbps: u64,
    pub download_limit_kbps: u64,
    pub hosts: Vec<SyncHostCfg>,
    pub folders: Vec<SyncFolderCfg>,
    /// Folders this device hosts for others.
    pub hosted: Vec<medialake_sync::host::config::HostedConfig>,
    pub hosting: SyncHostingCfg,
}

impl Default for SyncConfig {
    fn default() -> Self {
        SyncConfig {
            enabled: false,
            device_name: String::new(),
            debounce_ms: 2_000,
            upload_limit_kbps: 0,
            download_limit_kbps: 0,
            hosts: vec![],
            folders: vec![],
            hosted: vec![],
            hosting: SyncHostingCfg::default(),
        }
    }
}

impl SyncConfig {
    pub fn device(&self) -> String {
        if self.device_name.trim().is_empty() {
            medialake_proto::platform::host_name("MediaLake device")
        } else {
            self.device_name.clone()
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Config {
    /// `false` = no model, no indexer, no watcher, no search; the app is only a sync client / host.
    pub search_enabled: bool,
    pub sync: SyncConfig,
    /// The global shortcut to show the search window. Empty = none: nobody has to set one.
    pub hotkey: String,
    /// The welcome window (shortcut, model, sync) has been dealt with, by finishing or skipping it.
    /// A config file from before this existed counts as done; only "no config at all" is a first run.
    #[serde(default = "welcome_done_for_old_files")]
    pub welcome_done: bool,
    /// The embedding model; changing it (or the quality) re-indexes everything.
    pub model: Model,
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

/// What a config file without `welcome_done` means: it was written by a version that had no welcome
/// window, so the user is not new.
fn welcome_done_for_old_files() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        let model = Model::default();
        let cal = model.calibration();
        Config {
            search_enabled: true,
            sync: SyncConfig::default(),
            hotkey: String::new(),
            welcome_done: false,
            model,
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
            score_floor: cal.score_floor,
            score_ceiling: cal.score_ceiling,
            relative_cut: cal.relative_cut,
        }
    }
}

impl Config {
    /// Switches to `model`. The score mapping follows unless it was tuned away from the old model's defaults.
    pub fn set_model(&mut self, model: Model) {
        let old = self.model.calibration();
        if (self.score_floor, self.score_ceiling, self.relative_cut) == (old.score_floor, old.score_ceiling, old.relative_cut) {
            let new = model.calibration();
            (self.score_floor, self.score_ceiling, self.relative_cut) = (new.score_floor, new.score_ceiling, new.relative_cut);
        }
        self.model = model;
    }

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
    medialake_sync::util::plain_path(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_install_has_no_shortcut_and_has_not_seen_the_welcome_window() {
        let d = tempfile::tempdir().unwrap();
        let c = Config::load(&d.path().join("config.toml")).unwrap();
        assert!(!c.welcome_done);
        assert_eq!(c.hotkey, "");
    }

    #[test]
    fn a_config_from_before_the_welcome_window_counts_as_done() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.toml");
        // what the Windows installer used to write, and what an older app saved
        std::fs::write(&p, "hotkey = \"Ctrl+Alt+Space\"\n").unwrap();
        let c = Config::load(&p).unwrap();
        assert!(c.welcome_done);
        assert_eq!(c.hotkey, "Ctrl+Alt+Space");
        std::fs::write(&p, "threshold = 0.5\n").unwrap();
        assert!(Config::load(&p).unwrap().welcome_done);
    }

    #[test]
    fn the_welcome_flag_and_an_empty_shortcut_round_trip() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.toml");
        let mut c = Config::default();
        c.save(&p).unwrap();
        let back = Config::load(&p).unwrap();
        assert!(!back.welcome_done, "a saved first-run config is still a first run");
        assert_eq!(back.hotkey, "");
        c.welcome_done = true;
        c.hotkey = "Ctrl+Space".into();
        c.save(&p).unwrap();
        let back = Config::load(&p).unwrap();
        assert!(back.welcome_done);
        assert_eq!(back.hotkey, "Ctrl+Space");
    }
}
