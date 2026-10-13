//! One mounted sync: the cache store, the NFS server on a loopback port, and the mount itself.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use medialake_sync::member::transport::Transport;
use nfsserve::tcp::{NFSTcp, NFSTcpListener};

use crate::frontend::MountFrontend;
use crate::fs::MountFs;
use crate::mountctl::{self, EXPORT_PREFIX, MountSpec};
use crate::store::Store;

pub struct MountConfig {
    pub sync_id: String,
    pub name: String,
    /// Where the volume appears (`~/MediaLake/<name>`).
    pub point: PathBuf,
    /// `<caches>/MediaLake/sync/<sync id>`: the tree and the index.
    pub cache_dir: PathBuf,
    pub read_only: bool,
    /// 0 = no limit.
    pub limit_bytes: u64,
}

pub struct MountSession {
    store: Arc<Store>,
    frontend: Arc<MountFrontend>,
    point: PathBuf,
    port: u16,
    export: String,
    read_only: bool,
    server: tokio::task::JoinHandle<()>,
    watchdog: tokio::task::JoinHandle<()>,
    stopping: Arc<AtomicBool>,
}

impl MountSession {
    /// Starts the server and mounts. Fails (and leaves nothing behind) if the folder is not usable or the
    /// system refuses the mount: the caller falls back to a mirror folder.
    pub async fn start(cfg: MountConfig, tr: Arc<dyn Transport>) -> Result<MountSession> {
        let point = cfg.point.clone();
        let p2 = point.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            if let Some(m) = mountctl::mount_at(&p2) {
                if m.is_ours() {
                    let _ = mountctl::unmount(&p2, true);
                }
            }
            mountctl::prepare_mount_point(&p2)
        })
        .await??;

        let (cache, limit) = (cfg.cache_dir.clone(), cfg.limit_bytes);
        let store = tokio::task::spawn_blocking(move || Store::open(&cache, tr, limit)).await??;
        let fs = MountFs::new(store.clone(), cfg.read_only);
        let export = format!("{EXPORT_PREFIX}{}", hex16());
        let mut listener = NFSTcpListener::bind("127.0.0.1:0", fs).await.context("starting the local file server")?;
        listener.with_export_name(&export);
        let port = listener.get_listen_port();
        let server = tokio::spawn(async move {
            let _ = listener.handle_forever().await;
        });

        let (p, e, ro) = (point.clone(), export.clone(), cfg.read_only);
        let mounted = tokio::task::spawn_blocking(move || mountctl::mount(&MountSpec { port, export: &e, point: &p, read_only: ro })).await?;
        if let Err(e) = mounted {
            server.abort();
            store.stop();
            return Err(e);
        }

        let stopping = Arc::new(AtomicBool::new(false));
        let watchdog = {
            let (point, export, stopping) = (point.clone(), export.clone(), stopping.clone());
            let read_only = cfg.read_only;
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                    if stopping.load(Ordering::SeqCst) {
                        return;
                    }
                    let (p, e) = (point.clone(), export.clone());
                    // the system drops a mount it considers dead, the user can eject it: bring it back
                    let _ = tokio::task::spawn_blocking(move || {
                        if mountctl::mount_at(&p).is_none() {
                            tracing::info!("remounting {}", p.display());
                            if let Err(err) = mountctl::mount(&MountSpec { port, export: &e, point: &p, read_only }) {
                                tracing::warn!("remount of {} failed: {err:#}", p.display());
                            }
                        }
                    })
                    .await;
                }
            })
        };

        let frontend = Arc::new(MountFrontend::new(store.clone()));
        store.resume_pins();
        Ok(MountSession { store, frontend, point, port, export, read_only: cfg.read_only, server, watchdog, stopping })
    }

    pub fn frontend(&self) -> Arc<MountFrontend> {
        self.frontend.clone()
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    pub fn point(&self) -> &std::path::Path {
        &self.point
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn export(&self) -> &str {
        &self.export
    }

    pub fn read_only(&self) -> bool {
        self.read_only
    }

    pub fn is_mounted(&self) -> bool {
        mountctl::mount_at(&self.point).is_some_and(|m| m.is_ours())
    }

    /// Unmounts and stops the server. Changes already written are in the cache tree and are picked up
    /// by the engine on its next round.
    pub async fn stop(self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.watchdog.abort();
        let point = self.point.clone();
        let _ = tokio::task::spawn_blocking(move || mountctl::unmount_ours(&point)).await;
        self.store.stop();
        self.server.abort();
    }
}

impl Drop for MountSession {
    fn drop(&mut self) {
        // reached when the owner forgot `stop` (a panic, a crash path): do not leave a dead mount
        if !self.stopping.swap(true, Ordering::SeqCst) {
            self.watchdog.abort();
            let _ = mountctl::unmount_ours(&self.point);
            self.store.stop();
            self.server.abort();
        }
    }
}

fn hex16() -> String {
    medialake_sync::util::random_bytes::<8>().iter().map(|b| format!("{b:02x}")).collect()
}
