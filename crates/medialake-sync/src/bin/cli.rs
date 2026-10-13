//! `medialake-sync`: a headless member. Pairs with a host, joins syncs, keeps mirror folders in step.
//! (The desktop app drives the same engine through `medialake-core`.)

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use medialake_proto::sync::{ChangeResult, RestoreWhat, civil};
use medialake_sync::member::engine::{Choice, ConflictPolicy, Report};
use medialake_sync::member::frontend::{Frontend, Mirror};
use medialake_sync::member::transport::{HostClient, HostCreds, Transport, pair};
use medialake_sync::member::{MemberConfig, MemberStore, RunningSync, default_sync_dir, join};

#[derive(Parser)]
#[command(name = "medialake-sync", version, about = "MediaLake file sync member (mirror folders)")]
struct Cli {
    /// Where paired hosts, joined syncs and their state live.
    #[arg(long, env = "MEDIALAKE_SYNC_DIR", global = true)]
    dir: Option<PathBuf>,
    /// This device's name (shown in conflict names and on the host).
    #[arg(long, env = "MEDIALAKE_DEVICE_NAME", global = true)]
    device: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Pair with a host using a code from `medialake-server pair`.
    Pair { code: String },
    /// List paired hosts.
    Hosts,
    /// Forget a paired host.
    Forget { host: String },
    /// List the syncs a host offers to this device.
    Available { host: String },
    /// Join a sync into a folder.
    Join {
        host: String,
        sync: String,
        path: PathBuf,
        /// `server` (server wins, the losing version is kept as a conflict copy) or `ask`.
        #[arg(long, default_value = "server")]
        conflict: String,
    },
    /// Leave a sync (the folder is kept unless `--delete-files`).
    Leave {
        sync: String,
        #[arg(long)]
        delete_files: bool,
    },
    /// Joined syncs with their state.
    Status,
    /// One complete pass over every joined sync (or one).
    Once { sync: Option<String> },
    /// Keep every joined sync up to date until interrupted.
    Run,
    /// Pending conflicts (policy `ask`).
    Conflicts { sync: Option<String> },
    /// Settle a conflict: mine | server | both.
    Resolve { sync: String, path: String, choice: String },
    /// Let one held mass delete through.
    ConfirmDeletes { sync: String },
    /// Versions of a file on the host.
    Versions { sync: String, path: String },
    Snapshots { sync: String },
    /// Restore `<path>@<rev>` (rev from `versions`) or `--snapshot <id>` [path] on the host.
    Restore {
        sync: String,
        path: Option<String>,
        #[arg(long)]
        snapshot: Option<u64>,
        #[arg(long)]
        as_copy: bool,
        /// With `--snapshot`: also delete files that are not in the snapshot (they stay in the history).
        #[arg(long)]
        prune: bool,
    },
}

struct Ctx {
    store: MemberStore,
    device: String,
    rt: tokio::runtime::Runtime,
}

impl Ctx {
    fn client(&self, host_id: &str) -> Result<Arc<HostClient>> {
        let creds = self.store.find_host(host_id).with_context(|| format!("host {host_id} is no longer paired"))?;
        Ok(HostClient::new(self.rt.handle().clone(), creds, &self.device))
    }

    fn member(&self, sync: &str) -> Result<MemberConfig> {
        self.store.find_member(sync).with_context(|| format!("no joined sync {sync:?} (see `medialake-sync status`)"))
    }

    fn engine(&self, m: &MemberConfig) -> Result<medialake_sync::member::engine::Engine> {
        let client = self.client(&m.host_id)?;
        self.store.engine(m, &client, &self.device)
    }

    fn transport(&self, m: &MemberConfig) -> Result<impl Transport> {
        Ok(self.client(&m.host_id)?.transport(&m.sync_id))
    }
}

fn print_report(name: &str, r: &Result<Report>) {
    match r {
        Ok(r) => {
            if r.pulled + r.downloaded + r.pushed + r.conflicts + r.removed_local > 0 || !r.errors.is_empty() || r.held_deletes.is_some() {
                println!(
                    "{name}: {} downloaded, {} pushed ({} uploaded), {} removed here, {} conflict(s)",
                    r.downloaded, r.pushed, r.uploaded, r.removed_local, r.conflicts
                );
            }
            if let Some(n) = r.held_deletes {
                println!("{name}: {n} deletes are held back; `medialake-sync confirm-deletes {name}` lets them through");
            }
            for e in &r.errors {
                eprintln!("{name}: {e}");
            }
        }
        Err(e) => eprintln!("{name}: {e:#}"),
    }
}

fn ts(secs: i64) -> String {
    let (y, mo, d, h, mi) = civil(secs);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02} UTC")
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let store = MemberStore::open(&cli.dir.clone().unwrap_or_else(default_sync_dir))?;
    let device = cli.device.clone().unwrap_or_else(|| medialake_proto::platform::host_name("MediaLake device"));
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let cx = Ctx { store, device, rt };

    match cli.cmd {
        Cmd::Pair { code } => {
            let creds = cx.rt.block_on(pair(&code, &cx.device))?;
            println!("paired with {} ({})", creds.name, creds.address);
            cx.store.save_host(creds)?;
            println!("`medialake-sync available <host>` shows what it offers");
        }
        Cmd::Hosts => {
            let hosts = cx.store.hosts();
            if hosts.is_empty() {
                println!("no paired hosts");
            }
            for h in hosts {
                println!("{}  {}  {}", h.id, h.name, h.address);
            }
        }
        Cmd::Forget { host } => {
            let h = cx.store.find_host(&host).with_context(|| format!("no host {host:?}"))?;
            if cx.store.members().iter().any(|m| m.host_id == h.id) {
                bail!("syncs of this host are still joined; leave them first");
            }
            cx.store.forget_host(&h.id)?;
            println!("forgot {}", h.name);
        }
        Cmd::Available { host } => {
            let creds: HostCreds = cx.store.find_host(&host).with_context(|| format!("no host {host:?}"))?;
            let client = HostClient::new(cx.rt.handle().clone(), creds, &cx.device);
            let syncs = cx.rt.block_on(client.list_syncs())?;
            if syncs.is_empty() {
                println!("this host offers no syncs to this device");
            }
            for s in syncs {
                println!("{}  {}  {}  {} files  {} bytes  backup {}", s.id, s.name, s.access.as_str(), s.files, s.bytes, s.backup);
            }
        }
        Cmd::Join { host, sync, path, conflict } => {
            let creds = cx.store.find_host(&host).with_context(|| format!("no host {host:?}"))?;
            let policy = ConflictPolicy::parse(&conflict).with_context(|| "conflict policy must be `server` or `ask`")?;
            let m = cx.rt.block_on(join(&cx.store, &creds, &cx.device, &sync, &path, policy))?;
            println!("joined {:?} ({}) into {}", m.name, m.access, m.path.display());
            let mut e = cx.engine(&m)?;
            let r = e.sync_once();
            print_report(&m.name, &r);
        }
        Cmd::Leave { sync, delete_files } => {
            let m = cx.member(&sync)?;
            cx.store.remove_member(&m.sync_id, true)?;
            if delete_files {
                std::fs::remove_dir_all(&m.path).with_context(|| format!("removing {}", m.path.display()))?;
            }
            println!("left {:?}{}", m.name, if delete_files { "" } else { "; the folder is kept" });
        }
        Cmd::Status => {
            let members = cx.store.members();
            if members.is_empty() {
                println!("no joined syncs");
            }
            for m in members {
                let e = cx.engine(&m)?;
                let host = cx.store.find_host(&m.host_id).map(|h| h.name).unwrap_or_else(|| "?".into());
                println!(
                    "{}  {}  {}  host {}  {}  conflict policy {}  cursor {}  {} pending conflict(s){}",
                    m.name,
                    m.sync_id,
                    m.path.display(),
                    host,
                    m.access,
                    m.conflict,
                    e.cursor(),
                    e.conflicts().len(),
                    if m.paused { "  PAUSED" } else { "" }
                );
            }
        }
        Cmd::Once { sync } => {
            let members: Vec<MemberConfig> = match sync {
                Some(s) => vec![cx.member(&s)?],
                None => cx.store.members(),
            };
            for m in members {
                let mut e = cx.engine(&m)?;
                // a round that had to retry (host busy, file still being written) is run again
                let mut r = e.sync_once();
                for _ in 0..3 {
                    if !matches!(&r, Ok(rep) if rep.retry) {
                        break;
                    }
                    print_report(&m.name, &r);
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    r = e.sync_once();
                }
                print_report(&m.name, &r);
            }
        }
        Cmd::Run => {
            let members = cx.store.members();
            if members.is_empty() {
                bail!("no joined syncs: use `medialake-sync join`");
            }
            let mut running = vec![];
            for m in members.into_iter().filter(|m| !m.paused) {
                let client = cx.client(&m.host_id)?;
                let engine = cx.store.engine(&m, &client, &cx.device)?;
                let fe: Arc<dyn Frontend> = Arc::new(Mirror::new(&m.path));
                let name = m.name.clone();
                println!("keeping {} in step with {}", m.path.display(), m.name);
                running.push(RunningSync::start(engine, fe, &client, &m.sync_id, move |r| print_report(&name, r)));
            }
            cx.rt.block_on(tokio::signal::ctrl_c())?;
            println!("stopping");
            drop(running);
        }
        Cmd::Conflicts { sync } => {
            let members: Vec<MemberConfig> = match sync {
                Some(s) => vec![cx.member(&s)?],
                None => cx.store.members(),
            };
            let mut any = false;
            for m in members {
                for c in cx.engine(&m)?.conflicts() {
                    any = true;
                    println!("{}: {}  {}  since {}", m.name, c.path, if c.host_deleted { "deleted on the host" } else { "changed on the host" }, ts(c.since));
                }
            }
            if !any {
                println!("no conflicts");
            }
        }
        Cmd::Resolve { sync, path, choice } => {
            let m = cx.member(&sync)?;
            let c = Choice::parse(&choice).with_context(|| "choice must be mine, server or both")?;
            let mut e = cx.engine(&m)?;
            if e.resolve(&path, c)? {
                let r = e.sync_once();
                print_report(&m.name, &r);
            } else {
                println!("the host changed {path} again meanwhile: run `medialake-sync once` and resolve again");
            }
        }
        Cmd::ConfirmDeletes { sync } => {
            let m = cx.member(&sync)?;
            let mut e = cx.engine(&m)?;
            e.confirm_deletes()?;
            let r = e.sync_once();
            print_report(&m.name, &r);
        }
        Cmd::Versions { sync, path } => {
            let m = cx.member(&sync)?;
            let vs = cx.transport(&m)?.versions(&path)?;
            if vs.is_empty() {
                println!("no versions of {path}");
            }
            for v in vs {
                println!("rev {}  {}  {} bytes  by {}", v.rev, ts(v.created), v.size, v.author);
            }
        }
        Cmd::Snapshots { sync } => {
            let m = cx.member(&sync)?;
            for s in cx.transport(&m)?.snapshots()? {
                println!("{}  {}  {} entries", s.id, ts(s.created), s.entries);
            }
        }
        Cmd::Restore { sync, path, snapshot, as_copy, prune } => {
            let m = cx.member(&sync)?;
            let what = match (snapshot, path) {
                (Some(id), p) => RestoreWhat::Snapshot { id, path: p.unwrap_or_default(), prune },
                (None, Some(p)) => {
                    let (path, rev) = p.rsplit_once('@').context("expected <path>@<rev> (see `versions`)")?;
                    RestoreWhat::File { path: path.to_string(), rev: rev.parse().context("bad revision")? }
                }
                (None, None) => bail!("give <path>@<rev> or --snapshot <id>"),
            };
            let results = cx.transport(&m)?.restore(what, as_copy)?;
            let ok = results.iter().filter(|r| matches!(r, ChangeResult::Ok { .. })).count();
            println!("restored {ok} of {} item(s); `medialake-sync once` brings them here", results.len());
        }
    }
    Ok(())
}
