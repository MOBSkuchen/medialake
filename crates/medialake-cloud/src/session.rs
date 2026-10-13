//! One registered and connected sync root.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use medialake_cloudapi::{CloudApi, Connection, Registration};
use medialake_sync::member::transport::Transport;

use crate::frontend::CloudFrontend;
use crate::provider::HostProvider;

/// A file in the root that says it is ours (its name is one the sync ignores, so it is never synced).
const MARKER: &str = ".medialake-sync";

pub struct CloudConfig {
    pub sync_id: String,
    pub name: String,
    /// The sync root (`%USERPROFILE%\MediaLake\<name>`), on a local NTFS drive.
    pub root: PathBuf,
    pub read_only: bool,
}

pub struct CloudSession {
    api: Arc<dyn CloudApi>,
    frontend: Arc<CloudFrontend>,
    root: PathBuf,
    conn: Option<Box<dyn Connection>>,
}

#[cfg(windows)]
fn platform_api() -> Result<Arc<dyn CloudApi>> {
    Ok(Arc::new(medialake_cloudapi::win::WinCloud))
}

#[cfg(not(windows))]
fn platform_api() -> Result<Arc<dyn CloudApi>> {
    bail!("Cloud Files needs Windows")
}

/// A GUID for the provider from the sync id (a sync id is 32 hex digits = 16 bytes).
fn provider_id(sync_id: &str) -> [u8; 16] {
    let mut out = [0u8; 16];
    for (i, b) in out.iter_mut().enumerate() {
        *b = sync_id.get(i * 2..i * 2 + 2).and_then(|h| u8::from_str_radix(h, 16).ok()).unwrap_or(0);
    }
    out
}

impl CloudSession {
    /// Registers and connects the root. Fails (leaving nothing behind) if Cloud Files cannot be used
    /// here: the caller falls back to a mirror folder.
    pub fn start(cfg: CloudConfig, tr: Arc<dyn Transport>) -> Result<CloudSession> {
        CloudSession::start_with(platform_api()?, cfg, tr)
    }

    /// Like [`CloudSession::start`], over any platform implementation (tests).
    pub fn start_with(api: Arc<dyn CloudApi>, cfg: CloudConfig, tr: Arc<dyn Transport>) -> Result<CloudSession> {
        std::fs::create_dir_all(&cfg.root).with_context(|| format!("creating {}", cfg.root.display()))?;
        let root = cfg.root.canonicalize().unwrap_or(cfg.root.clone());
        api.available(&root).map_err(anyhow::Error::msg)?;
        // an ordinary folder with things in it is not turned into a sync root under the user's feet; one
        // that has our marker is ours from an earlier run
        let marker = root.join(MARKER);
        if !marker.exists() {
            let mut stray = vec![];
            for e in std::fs::read_dir(&root)?.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if !medialake_proto::sync::is_ignored(&name, &[]) {
                    stray.push(name);
                }
            }
            if !stray.is_empty() {
                bail!("{} is not empty ({}); a synced folder needs an empty folder", root.display(), stray.iter().take(3).cloned().collect::<Vec<_>>().join(", "));
            }
        }
        let reg = Registration {
            provider_name: "MediaLake".into(),
            provider_version: env!("CARGO_PKG_VERSION").into(),
            root_identity: cfg.sync_id.clone().into_bytes(),
            provider_id: provider_id(&cfg.sync_id),
        };
        api.register(&root, &reg).map_err(|e| anyhow::anyhow!("{e}"))?;
        let _ = std::fs::write(&marker, cfg.sync_id.as_bytes());
        let conn = api.connect(&root, Arc::new(HostProvider::new(tr))).map_err(|e| anyhow::anyhow!("{e}"))?;
        let frontend = Arc::new(CloudFrontend::new(api.clone(), &root, cfg.read_only));
        frontend.reconcile_pins();
        Ok(CloudSession { api, frontend, root, conn: Some(conn) })
    }

    pub fn frontend(&self) -> Arc<CloudFrontend> {
        self.frontend.clone()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Disconnects; the root stays registered (placeholders keep their place and fill in again when the
    /// app is back). Opening a placeholder meanwhile fails with "the cloud provider is not running".
    pub fn stop(mut self) {
        self.conn.take();
    }

    /// Leaves the sync for good: what was downloaded becomes ordinary files (or goes, if
    /// `keep_downloaded` is false), placeholders go, the root is unregistered.
    pub fn leave(mut self, keep_downloaded: bool) -> Result<()> {
        self.conn.take();
        leave_root(self.api.as_ref(), &self.root, keep_downloaded)
    }
}

/// [`leave_root`] over this platform's Cloud Files (an error off Windows).
pub fn leave_root_default(root: &Path, keep_downloaded: bool) -> Result<()> {
    leave_root(platform_api()?.as_ref(), root, keep_downloaded)
}

/// Unregisters `root` after turning every hydrated file back into a normal file and removing the
/// placeholders whose content is not here. Needs no connection (uninstall uses it too).
pub fn leave_root(api: &dyn CloudApi, root: &Path, keep_downloaded: bool) -> Result<()> {
    // deepest first, so that folders are empty when their turn comes
    let mut all: Vec<(PathBuf, bool)> = vec![];
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            all.push((p.clone(), is_dir));
            if is_dir {
                stack.push(p);
            }
        }
    }
    all.sort_by_key(|(p, _)| std::cmp::Reverse(p.components().count()));
    let mut first_error = None;
    for (p, is_dir) in all {
        if is_dir {
            if !keep_downloaded {
                let _ = std::fs::remove_dir(&p);
            }
            continue;
        }
        let st = api.state(&p).unwrap_or_default();
        let keep = keep_downloaded && (!st.placeholder || !st.dehydrated);
        if !keep {
            if let Err(e) = std::fs::remove_file(&p) {
                first_error.get_or_insert(format!("removing {}: {e}", p.display()));
            }
        } else if st.placeholder {
            if let Err(e) = api.revert(&p) {
                first_error.get_or_insert(format!("{e}"));
            }
        }
    }
    api.unregister(root).map_err(|e| anyhow::anyhow!("{e}"))?;
    match first_error {
        Some(e) => bail!("{e}"),
        None => Ok(()),
    }
}
