//! `medialake-server sync …`: configure and inspect hosted syncs.
//!
//! Configuration commands edit `syncs.json` / `clients.json`, which a running `serve` re-reads when the
//! files change. Commands that change data (`snapshot`, `restore`) open the sync's database directly:
//! SQLite serialises them with the running server, and the server notices the new sequence number
//! within a couple of seconds and tells its subscribers.

use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::Subcommand;
use medialake_proto::sync::{Access, ChangeResult, RestoreWhat};
use medialake_sync::host::config::{BackupPolicy, HostedConfig, SyncsFile};
use medialake_sync::host::hosted::HostedSync;

use crate::serve::StateArgs;

#[derive(Subcommand, Clone)]
pub enum SyncCmd {
    /// Share a folder as a sync.
    Add {
        name: String,
        folder: PathBuf,
        /// `none`, `time[:24h[:7]]` or `file[:10]`
        #[arg(long, default_value = "file:10")]
        backup: String,
        /// Keep `.medialake-sync` (database, history) here instead of inside the folder.
        #[arg(long)]
        store: Option<PathBuf>,
        #[command(flatten)]
        state: StateArgs,
    },
    List(StateArgs),
    /// Stop sharing. The folder and its history are left alone.
    Remove {
        name: String,
        #[command(flatten)]
        state: StateArgs,
    },
    /// Change a sync's settings.
    Set {
        name: String,
        #[arg(long)]
        backup: Option<String>,
        /// Extra ignore pattern (`*`, `?`); repeatable. Replaces the current list.
        #[arg(long = "ignore")]
        ignore: Vec<String>,
        #[command(flatten)]
        state: StateArgs,
    },
    /// Give an already paired client access to a sync.
    Grant {
        client: String,
        name: String,
        #[arg(default_value = "rw")]
        access: String,
        #[command(flatten)]
        state: StateArgs,
    },
    /// Take access away again (takes effect on the client's next request).
    Revoke {
        client: String,
        name: String,
        #[command(flatten)]
        state: StateArgs,
    },
    Snapshots {
        name: String,
        #[command(flatten)]
        state: StateArgs,
    },
    /// Take a snapshot now.
    Snapshot {
        name: String,
        #[command(flatten)]
        state: StateArgs,
    },
    Versions {
        name: String,
        path: String,
        #[command(flatten)]
        state: StateArgs,
    },
    /// Restore `<path>[@rev]` from the file history, or `--snapshot <id>` (optionally limited to
    /// `<path>`). A restore is a normal new commit, so it reaches every member.
    Restore {
        name: String,
        path: Option<String>,
        #[arg(long)]
        snapshot: Option<u64>,
        /// Write `name (restored <date>).ext` instead of replacing the current file.
        #[arg(long)]
        as_copy: bool,
        /// With `--snapshot`: also delete files that are not in the snapshot (they stay in the history).
        #[arg(long)]
        prune: bool,
        #[command(flatten)]
        state: StateArgs,
    },
}

fn find(file: &SyncsFile, name: &str) -> anyhow::Result<HostedConfig> {
    file.find(name).cloned().with_context(|| format!("no sync called {name:?} (see `medialake-server sync list`)"))
}

fn open(state: &StateArgs, name: &str) -> anyhow::Result<std::sync::Arc<HostedSync>> {
    let st = state.open()?;
    let cfg = find(&SyncsFile::load(st.dir()), name)?;
    let hs = HostedSync::open(cfg, None)?;
    hs.scan()?;
    Ok(hs)
}

fn ts(secs: i64) -> String {
    let (y, mo, d, h, mi) = medialake_proto::sync::civil(secs);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02} UTC")
}

pub fn run(cmd: SyncCmd) -> anyhow::Result<()> {
    match cmd {
        SyncCmd::Add { name, folder, backup, store, state } => {
            let st = state.open()?;
            let policy = BackupPolicy::parse(&backup).map_err(anyhow::Error::msg)?;
            let mut file = SyncsFile::load(st.dir());
            let cfg = file.add(&name, &folder, store, policy)?;
            file.save(st.dir())?;
            println!("sharing {} as {:?} (id {}, backup {})", cfg.root.display(), cfg.name, cfg.id, cfg.backup.describe());
            println!("Invite a device with: medialake-server pair --sync {}:rw", cfg.name);
        }
        SyncCmd::List(state) => {
            let st = state.open()?;
            let file = SyncsFile::load(st.dir());
            if file.syncs.is_empty() {
                println!("no syncs");
            }
            let clients = st.list_clients();
            for s in &file.syncs {
                let members: Vec<String> =
                    clients.values().filter_map(|c| c.syncs.get(&s.id).map(|a| format!("{}:{a}", c.name))).collect();
                println!("{}  {}  {}  backup {}  members [{}]", s.name, s.id, s.root.display(), s.backup.describe(), members.join(", "));
            }
        }
        SyncCmd::Remove { name, state } => {
            let st = state.open()?;
            let mut file = SyncsFile::load(st.dir());
            let cfg = find(&file, &name)?;
            file.syncs.retain(|s| s.id != cfg.id);
            file.save(st.dir())?;
            st.drop_sync_grants(&cfg.id)?;
            println!("no longer sharing {:?}; {} is untouched", cfg.name, cfg.root.display());
        }
        SyncCmd::Set { name, backup, ignore, state } => {
            let st = state.open()?;
            let mut file = SyncsFile::load(st.dir());
            let id = find(&file, &name)?.id;
            let cfg = file.syncs.iter_mut().find(|s| s.id == id).unwrap();
            if let Some(b) = backup {
                cfg.backup = BackupPolicy::parse(&b).map_err(anyhow::Error::msg)?;
            }
            if !ignore.is_empty() {
                cfg.ignore = ignore;
            }
            println!("{}: backup {}, ignore {:?}", cfg.name, cfg.backup.describe(), cfg.ignore);
            file.save(st.dir())?;
        }
        SyncCmd::Grant { client, name, access, state } => {
            let st = state.open()?;
            let cfg = find(&SyncsFile::load(st.dir()), &name)?;
            let acc = Access::parse(&access).with_context(|| format!("{access:?}: expected rw or ro"))?;
            let Some(id) = st.find_client(&client) else { bail!("no client {client:?} (see `medialake-server clients list`)") };
            st.set_grant(&id, &cfg.id, Some(acc))?;
            println!("{client} may now {} {:?}", if acc == Access::ReadWrite { "read and write" } else { "read" }, cfg.name);
        }
        SyncCmd::Revoke { client, name, state } => {
            let st = state.open()?;
            let cfg = find(&SyncsFile::load(st.dir()), &name)?;
            let Some(id) = st.find_client(&client) else { bail!("no client {client:?}") };
            st.set_grant(&id, &cfg.id, None)?;
            println!("{client} no longer has access to {:?}", cfg.name);
        }
        SyncCmd::Snapshots { name, state } => {
            let hs = open(&state, &name)?;
            let snaps = hs.snapshots();
            if snaps.is_empty() {
                println!("no snapshots");
            }
            for s in snaps {
                println!("{}  {}  seq {}  {} entries", s.id, ts(s.created), s.seq, s.entries);
            }
        }
        SyncCmd::Snapshot { name, state } => {
            let hs = open(&state, &name)?;
            let id = hs.snapshot_now()?;
            println!("snapshot {id} taken");
        }
        SyncCmd::Versions { name, path, state } => {
            let hs = open(&state, &name)?;
            let vs = hs.versions(&path);
            if vs.is_empty() {
                println!("no versions of {path}");
            }
            for v in vs {
                println!("rev {}  {}  {} bytes  by {}  {}", v.rev, ts(v.created), v.size, v.author, &v.hash[..12]);
            }
        }
        SyncCmd::Restore { name, path, snapshot, as_copy, prune, state } => {
            let hs = open(&state, &name)?;
            let what = match (snapshot, path) {
                (Some(id), p) => RestoreWhat::Snapshot { id, path: p.unwrap_or_default(), prune },
                (None, Some(p)) => {
                    let (path, rev) = match p.rsplit_once('@') {
                        Some((a, r)) => (a.to_string(), r.parse::<u64>().with_context(|| format!("bad revision {r:?}"))?),
                        None => (p.clone(), 0),
                    };
                    // no revision given: the newest version that differs from the current file
                    let rev = if rev != 0 {
                        rev
                    } else {
                        let cur = hs.entry(&path).filter(|e| !e.deleted).map(|e| e.hash);
                        hs.versions(&path)
                            .into_iter()
                            .find(|v| Some(&v.hash) != cur.as_ref())
                            .map(|v| v.rev)
                            .with_context(|| format!("no earlier version of {path}"))?
                    };
                    RestoreWhat::File { path, rev }
                }
                (None, None) => bail!("give a path (path[@rev]) or --snapshot <id>"),
            };
            let results = hs.restore("medialake-server", &what, as_copy)?;
            let ok = results.iter().filter(|r| matches!(r, ChangeResult::Ok { .. })).count();
            println!("restored {ok} of {} item(s)", results.len());
            for r in &results {
                if !matches!(r, ChangeResult::Ok { .. }) {
                    println!("  not restored: {r:?}");
                }
            }
        }
    }
    Ok(())
}
