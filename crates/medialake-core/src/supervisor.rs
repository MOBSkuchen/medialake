//! Supervises the local `medialake-server` child process: spawn, health, restart with backoff,
//! idle shutdown. Clients observe the result through a `watch` channel of [`ServerState`].

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use bierpc::rpc::ClientTlsConfig;
use medialake_proto::models::{Model, Quality};
use medialake_proto::{READY_PREFIX, ServerInfo};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Notify, watch};

static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Process-wide unique connection generation (local restarts and remote reconnects share the counter).
pub fn next_generation() -> u64 {
    GENERATION.fetch_add(1, Ordering::SeqCst) + 1
}

/// What the embedding client needs from whatever provides the server.
pub trait Backend: Send + Sync {
    fn ensure_running(&self);
    fn touch(&self);
    fn report_failure(&self, generation: u64);
}

impl Backend for Supervisor {
    fn ensure_running(&self) {
        Supervisor::ensure_running(self)
    }
    fn touch(&self) {
        Supervisor::touch(self)
    }
    fn report_failure(&self, generation: u64) {
        Supervisor::report_failure(self, generation)
    }
}

/// A reachable server.
#[derive(Clone)]
pub struct ConnTarget {
    pub addr: SocketAddr,
    pub tls: Option<ClientTlsConfig>,
    pub auth: String,
    /// Bumped on every (re)start so that clients know to reconnect.
    pub generation: u64,
    pub info: Option<ServerInfo>,
}

#[derive(Clone)]
pub enum ServerState {
    /// Not running (never started, or stopped for being idle); starts on demand.
    Idle,
    Starting,
    Ready(Arc<ConnTarget>),
    /// Cannot run (e.g. model files missing); `retry_at` restarts are scheduled by the supervisor.
    Failed(String),
}

impl ServerState {
    pub fn label(&self) -> &'static str {
        match self {
            ServerState::Idle => "idle",
            ServerState::Starting => "starting",
            ServerState::Ready(_) => "ready",
            ServerState::Failed(_) => "failed",
        }
    }
}

#[derive(Clone, Debug)]
pub struct SupervisorConfig {
    pub exe: PathBuf,
    /// CPU-only build used when exe cannot start (e.g. no Vulkan runtime on this machine).
    pub fallback_exe: Option<PathBuf>,
    pub model_dir: PathBuf,
    pub log_dir: Option<PathBuf>,
    pub model: Model,
    pub quality: Quality,
    pub gpu_layers: Option<i32>,
    /// 0 = never stop for idleness.
    pub idle_shutdown: Duration,
}

pub struct Supervisor {
    state_tx: watch::Sender<ServerState>,
    cfg: tokio::sync::Mutex<SupervisorConfig>,
    wake: Notify,
    restart: Notify,
    stop: Notify,
    last_activity: AtomicI64,
    using_fallback: std::sync::atomic::AtomicBool,
}

fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn random_token() -> String {
    // 32 hex chars from two process-random hashers + clock; only has to be unguessable by other
    // local processes for the lifetime of this launch
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::new();
    for _ in 0..4 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

impl Supervisor {
    pub fn start(cfg: SupervisorConfig) -> (Arc<Supervisor>, watch::Receiver<ServerState>) {
        let (state_tx, rx) = watch::channel(ServerState::Idle);
        let s = Arc::new(Supervisor {
            state_tx,
            cfg: tokio::sync::Mutex::new(cfg),
            wake: Notify::new(),
            restart: Notify::new(),
            stop: Notify::new(),
            last_activity: AtomicI64::new(now_secs()),
            using_fallback: std::sync::atomic::AtomicBool::new(false),
        });
        let s2 = s.clone();
        tokio::spawn(async move { s2.run().await });
        (s, rx)
    }

    /// Starts the server if it is idle. Cheap to call on every request.
    pub fn ensure_running(&self) {
        self.last_activity.store(now_secs(), Ordering::Relaxed);
        if matches!(*self.state_tx.borrow(), ServerState::Idle | ServerState::Failed(_)) {
            self.wake.notify_one();
        }
    }

    pub fn touch(&self) {
        self.last_activity.store(now_secs(), Ordering::Relaxed);
    }

    /// A client could not talk to generation `generation_id`: restart it.
    pub fn report_failure(&self, generation_id: u64) {
        if let ServerState::Ready(t) = &*self.state_tx.borrow() {
            if t.generation == generation_id {
                self.restart.notify_one();
            }
        }
    }

    pub async fn reconfigure(&self, f: impl FnOnce(&mut SupervisorConfig)) {
        f(&mut *self.cfg.lock().await);
        self.restart.notify_one();
    }

    pub async fn shutdown(&self) {
        self.stop.notify_one();
    }

    fn set(&self, s: ServerState) {
        let _ = self.state_tx.send(s);
    }

    async fn run(self: Arc<Self>) {
        let mut backoff = Duration::from_secs(1);
        loop {
            // Idle: wait for demand
            self.set(ServerState::Idle);
            tokio::select! {
                _ = self.wake.notified() => {}
                _ = self.stop.notified() => return,
            }
            loop {
                let mut cfg = self.cfg.lock().await.clone();
                if self.using_fallback.load(Ordering::Relaxed) {
                    if let Some(f) = cfg.fallback_exe.clone() {
                        cfg.exe = f;
                    }
                }
                self.set(ServerState::Starting);
                match self.spawn(&cfg).await {
                    Ok((mut child, target)) => {
                        backoff = Duration::from_secs(1);
                        self.last_activity.store(now_secs(), Ordering::Relaxed);
                        self.set(ServerState::Ready(Arc::new(target)));
                        let outcome = self.monitor(&mut child, &cfg).await;
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        match outcome {
                            Exit::Stop => return,
                            Exit::Idle => break,
                            Exit::Restart => continue,
                            Exit::Crashed(why) => {
                                tracing::error!("server child exited: {why}");
                                self.set(ServerState::Failed(format!("server crashed: {why}")));
                                tokio::select! {
                                    _ = tokio::time::sleep(backoff) => {}
                                    _ = self.stop.notified() => return,
                                }
                                backoff = (backoff * 2).min(Duration::from_secs(30));
                                continue;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("cannot start server: {e}");
                        if cfg.fallback_exe.as_ref().is_some_and(|f| *f != cfg.exe && f.exists()) {
                            tracing::warn!("falling back to the CPU server build");
                            self.using_fallback.store(true, Ordering::Relaxed);
                            continue;
                        }
                        self.set(ServerState::Failed(e));
                        // wait for demand (a retry, a settings change) instead of spinning
                        tokio::select! {
                            _ = self.wake.notified() => {}
                            _ = self.restart.notified() => {}
                            _ = tokio::time::sleep(Duration::from_secs(15)) => {}
                            _ = self.stop.notified() => return,
                        }
                    }
                }
            }
        }
    }

    async fn monitor(&self, child: &mut Child, cfg: &SupervisorConfig) -> Exit {
        let mut tick = tokio::time::interval(Duration::from_secs(10));
        loop {
            tokio::select! {
                st = child.wait() => return Exit::Crashed(format!("{st:?}")),
                _ = self.restart.notified() => return Exit::Restart,
                _ = self.stop.notified() => return Exit::Stop,
                _ = tick.tick() => {
                    if !cfg.idle_shutdown.is_zero() {
                        let idle = now_secs() - self.last_activity.load(Ordering::Relaxed);
                        if idle as u64 >= cfg.idle_shutdown.as_secs() {
                            tracing::info!("stopping idle server");
                            return Exit::Idle;
                        }
                    }
                }
            }
        }
    }

    async fn spawn(&self, cfg: &SupervisorConfig) -> Result<(Child, ConnTarget), String> {
        let set = medialake_proto::models::model_set(cfg.model, cfg.quality);
        for f in [&set.model, &set.mmproj] {
            if !cfg.model_dir.join(f.file_name).exists() {
                return Err(format!("model file {} is missing ({}, quality {})", f.file_name, cfg.model.display_name(), cfg.quality.as_str()));
            }
        }
        if !cfg.exe.exists() {
            return Err(format!("server executable not found: {}", cfg.exe.display()));
        }
        let token = random_token();
        let mut cmd = Command::new(&cfg.exe);
        cmd.arg("--local")
            .arg("--port")
            .arg("0")
            .arg("--model-dir")
            .arg(&cfg.model_dir)
            .arg("--model")
            .arg(cfg.model.as_str())
            .arg("--quality")
            .arg(cfg.quality.as_str());
        if let Some(g) = cfg.gpu_layers {
            cmd.arg("--gpu-layers").arg(g.to_string());
        }
        if let Some(l) = &cfg.log_dir {
            cmd.arg("--log-dir").arg(l);
        }
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        let mut child = cmd.spawn().map_err(|e| format!("spawn {}: {e}", cfg.exe.display()))?;
        job::assign(&child);

        let mut stdin = child.stdin.take().ok_or("no stdin")?;
        stdin.write_all(format!("{token}\n").as_bytes()).await.map_err(|e| e.to_string())?;
        stdin.flush().await.map_err(|e| e.to_string())?;
        // keep stdin open: the child exits when it closes (i.e. when we die)
        tokio::spawn(async move {
            let _keep = stdin;
            std::future::pending::<()>().await;
        });

        if let Some(err) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(err).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    tracing::debug!(target: "server", "{l}");
                }
            });
        }
        let out = child.stdout.take().ok_or("no stdout")?;
        let mut lines = BufReader::new(out).lines();
        let port = tokio::time::timeout(Duration::from_secs(180), async {
            while let Some(l) = lines.next_line().await.map_err(|e| e.to_string())? {
                if let Some(p) = l.strip_prefix(READY_PREFIX) {
                    return p.trim().parse::<u16>().map_err(|e| e.to_string());
                }
            }
            Err("server exited before becoming ready".to_string())
        })
        .await
        .map_err(|_| "server did not become ready within 180 s".to_string())??;
        // drain remaining stdout so the pipe can never fill up
        tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });

        let generation = next_generation();
        let target = ConnTarget {
            addr: SocketAddr::from(([127, 0, 0, 1], port)),
            tls: None,
            auth: token,
            generation,
            info: None,
        };
        tracing::info!("server ready on 127.0.0.1:{port} (generation {generation})");
        Ok((child, target))
    }
}

enum Exit {
    Stop,
    Idle,
    Restart,
    Crashed(String),
}

/// Windows Job Object: the child is killed when the client process goes away. Elsewhere nothing is
/// needed: the server exits when its stdin closes (the pipe closes with the client) and the child is
/// `kill_on_drop`.
mod job {
    use tokio::process::Child;

    #[cfg(windows)]
    pub fn assign(child: &Child) {
        use std::sync::OnceLock;
        use windows_sys::Win32::System::JobObjects::*;

        static JOB: OnceLock<isize> = OnceLock::new();
        let job = *JOB.get_or_init(|| unsafe {
            let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if h.is_null() {
                return 0;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            h as isize
        });
        if job != 0 {
            if let Some(h) = child.raw_handle() {
                unsafe {
                    AssignProcessToJobObject(job as *mut _, h as *mut _);
                }
            }
        }
    }

    #[cfg(not(windows))]
    pub fn assign(_child: &Child) {}

}
