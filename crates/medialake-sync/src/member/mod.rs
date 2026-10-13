//! The member side: engine, frontends, transports, and the on-disk records of paired hosts and joined
//! syncs used by the command line (the app keeps the same facts in `config.toml`).

pub mod db;
pub mod engine;
pub mod frontend;
pub mod transport;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use medialake_proto::sync::{Access, SubscribeItem};
use serde::{Deserialize, Serialize};

use self::engine::{ConflictPolicy, Engine, EngineConfig, Wake};
use self::frontend::{Frontend, Mirror};
use self::transport::{HostClient, HostCreds, Subscription};

pub fn default_sync_dir() -> PathBuf {
    medialake_proto::platform::data_dir().join("sync")
}

/// A sync this device has joined.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MemberConfig {
    pub sync_id: String,
    pub host_id: String,
    pub name: String,
    pub path: PathBuf,
    /// `server_wins` or `ask`
    pub conflict: String,
    /// `rw` or `ro` as granted when joining (the host's answer wins at run time)
    pub access: String,
    #[serde(default)]
    pub paused: bool,
}

#[derive(Serialize, Deserialize, Default)]
struct Hosts {
    hosts: Vec<HostCreds>,
}

#[derive(Serialize, Deserialize, Default)]
struct Members {
    syncs: Vec<MemberConfig>,
}

/// `<data dir>/sync/`: `hosts.json`, `members.json` and one state folder per sync.
pub struct MemberStore {
    dir: PathBuf,
}

impl MemberStore {
    pub fn open(dir: &Path) -> Result<MemberStore> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(MemberStore { dir: dir.to_path_buf() })
    }

    fn read<T: for<'a> Deserialize<'a> + Default>(&self, name: &str) -> T {
        std::fs::read_to_string(self.dir.join(name)).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    fn write<T: Serialize>(&self, name: &str, v: &T) -> Result<()> {
        let tmp = self.dir.join(format!("{name}.tmp"));
        std::fs::write(&tmp, serde_json::to_string_pretty(v)?)?;
        // the file holds host secrets
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(tmp, self.dir.join(name))?;
        Ok(())
    }

    pub fn hosts(&self) -> Vec<HostCreds> {
        self.read::<Hosts>("hosts.json").hosts
    }

    pub fn find_host(&self, id_or_name: &str) -> Option<HostCreds> {
        let hosts = self.hosts();
        hosts.iter().find(|h| h.id == id_or_name).or_else(|| hosts.iter().find(|h| h.name == id_or_name)).cloned()
    }

    pub fn save_host(&self, h: HostCreds) -> Result<()> {
        let mut f: Hosts = self.read("hosts.json");
        f.hosts.retain(|x| x.id != h.id);
        f.hosts.push(h);
        self.write("hosts.json", &f)
    }

    pub fn forget_host(&self, id: &str) -> Result<()> {
        let mut f: Hosts = self.read("hosts.json");
        f.hosts.retain(|x| x.id != id);
        self.write("hosts.json", &f)
    }

    pub fn members(&self) -> Vec<MemberConfig> {
        self.read::<Members>("members.json").syncs
    }

    pub fn find_member(&self, id_or_name: &str) -> Option<MemberConfig> {
        let all = self.members();
        all.iter().find(|m| m.sync_id == id_or_name).or_else(|| all.iter().find(|m| m.name == id_or_name)).cloned()
    }

    pub fn save_member(&self, m: MemberConfig) -> Result<()> {
        let mut f: Members = self.read("members.json");
        f.syncs.retain(|x| x.sync_id != m.sync_id);
        f.syncs.push(m);
        self.write("members.json", &f)
    }

    pub fn remove_member(&self, sync_id: &str, delete_state: bool) -> Result<()> {
        let mut f: Members = self.read("members.json");
        f.syncs.retain(|x| x.sync_id != sync_id);
        self.write("members.json", &f)?;
        if delete_state {
            let _ = std::fs::remove_dir_all(self.state_dir(sync_id));
        }
        Ok(())
    }

    pub fn state_dir(&self, sync_id: &str) -> PathBuf {
        self.dir.join(sync_id)
    }

    /// Builds an engine for a joined sync over a mirror folder and a live host connection.
    pub fn engine(&self, m: &MemberConfig, client: &Arc<HostClient>, device: &str) -> Result<Engine> {
        let access = Access::parse(&m.access).unwrap_or(Access::ReadOnly);
        let policy = ConflictPolicy::parse(&m.conflict).unwrap_or(ConflictPolicy::ServerWins);
        let cfg = EngineConfig {
            device: device.to_string(),
            policy,
            access,
            state_dir: self.state_dir(&m.sync_id),
            settle_ms: 1000,
            ignore: vec![],
        };
        Engine::new(cfg, Arc::new(Mirror::new(&m.path)), Arc::new(client.transport(&m.sync_id)))
    }

    /// Like [`MemberStore::engine`], with the frontend and transport chosen by the caller (a mount
    /// shares its transport with the engine).
    pub fn engine_with(&self, m: &MemberConfig, device: &str, fe: Arc<dyn Frontend>, tr: Arc<dyn transport::Transport>) -> Result<Engine> {
        let cfg = EngineConfig {
            device: device.to_string(),
            policy: ConflictPolicy::parse(&m.conflict).unwrap_or(ConflictPolicy::ServerWins),
            access: Access::parse(&m.access).unwrap_or(Access::ReadOnly),
            state_dir: self.state_dir(&m.sync_id),
            settle_ms: 1000,
            ignore: vec![],
        };
        Engine::new(cfg, fe, tr)
    }
}

/// A joined sync running on its own thread: engine, local watcher and host notifications.
pub struct RunningSync {
    stop: Arc<AtomicBool>,
    wake: mpsc::Sender<Wake>,
    thread: Option<std::thread::JoinHandle<()>>,
    _subscription: Subscription,
    _watch: Option<Box<dyn std::any::Any + Send>>,
}

impl RunningSync {
    pub fn start(
        mut engine: Engine,
        fe: Arc<dyn Frontend>,
        client: &Arc<HostClient>,
        sync_id: &str,
        on_report: impl Fn(&Result<engine::Report>) + Send + 'static,
    ) -> RunningSync {
        let (wtx, wrx) = mpsc::channel::<Wake>();
        let stop = Arc::new(AtomicBool::new(false));
        // host notifications -> wake-ups
        let (ntx, nrx) = mpsc::channel::<String>();
        let w2 = wtx.clone();
        std::thread::spawn(move || {
            while nrx.recv().is_ok() {
                if w2.send(Wake::Remote).is_err() {
                    return;
                }
            }
        });
        let sub = client.subscribe(vec![SubscribeItem { sync: sync_id.to_string(), cursor: engine.cursor() }], ntx);
        // local changes -> wake-ups
        let w3 = wtx.clone();
        let watch = fe.watch(Duration::from_secs(2), Box::new(move |paths| {
            let _ = w3.send(Wake::Local(paths));
        }));
        let stop2 = stop.clone();
        let thread = std::thread::Builder::new()
            .name("medialake-sync".into())
            .spawn(move || engine.run(wrx, &stop2, Duration::from_secs(60), Duration::from_secs(1), &on_report))
            .ok();
        RunningSync { stop, wake: wtx, thread, _subscription: sub, _watch: watch }
    }

    pub fn nudge(&self) {
        let _ = self.wake.send(Wake::Remote);
    }

    /// Full look at everything, from cursor 0 (the "Verify" button).
    pub fn verify(&self) {
        let _ = self.wake.send(Wake::Verify);
    }

    /// Queues `f` to run on the engine's thread between rounds; the result arrives on the receiver.
    pub fn call_async<R: Send + 'static>(&self, f: impl FnOnce(&mut Engine) -> R + Send + 'static) -> Result<mpsc::Receiver<R>> {
        let (tx, rx) = mpsc::channel();
        self.wake
            .send(Wake::Command(Box::new(move |e| {
                let _ = tx.send(f(e));
            })))
            .map_err(|_| anyhow::anyhow!("the sync is not running"))?;
        Ok(rx)
    }

    /// Runs `f` on the engine's thread between rounds and waits for its result.
    pub fn call<R: Send + 'static>(&self, f: impl FnOnce(&mut Engine) -> R + Send + 'static) -> Result<R> {
        self.call_async(f)?.recv_timeout(Duration::from_secs(120)).map_err(|_| anyhow::anyhow!("the sync did not answer in time"))
    }
}

impl Drop for RunningSync {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = self.wake.send(Wake::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Joins: checks that the host lists the sync for this client and records it.
pub async fn join(store: &MemberStore, host: &HostCreds, device: &str, sync: &str, path: &Path, policy: ConflictPolicy) -> Result<MemberConfig> {
    let client = HostClient::new(tokio::runtime::Handle::current(), host.clone(), device);
    let syncs = client.list_syncs().await?;
    let Some(info) = syncs.iter().find(|s| s.id == sync || s.name == sync) else {
        bail!("this host does not offer a sync called {sync:?} to this device");
    };
    if store.members().iter().any(|m| m.sync_id == info.id) {
        bail!("already joined {:?}", info.name);
    }
    std::fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    let path = crate::util::plain_path(&path.canonicalize()?);
    let m = MemberConfig {
        sync_id: info.id.clone(),
        host_id: host.id.clone(),
        name: info.name.clone(),
        path,
        conflict: policy.as_str().into(),
        access: info.access.as_str().into(),
        paused: false,
    };
    store.save_member(m.clone())?;
    Ok(m)
}
