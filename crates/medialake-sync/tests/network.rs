//! The real thing: TLS with a pinned certificate, pairing with grants, bierpc single calls plus the
//! three persistent calls, the notification stream, and revocation.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use bierpc::error::RpcResult;
use bierpc::rpc::{RpcServer, RpcServerHandler, ServerConfig};
use common::{tree, write};
use medialake_proto::pairing::hex;
use medialake_proto::sync::{Access, SubscribeItem};
use medialake_proto::{Action, MAX_MESSAGE_BYTES, Response, pinning};
use medialake_sync::host::SyncHost;
use medialake_sync::host::config::{BackupPolicy, SyncsFile};
use medialake_sync::host::state::{Grants, StateDir};
use medialake_sync::member::engine::{ConflictPolicy, Engine, EngineConfig};
use medialake_sync::member::frontend::Mirror;
use medialake_sync::member::transport::{HostClient, pair};

struct Handler {
    host: Arc<SyncHost>,
    state: Arc<StateDir>,
}

impl RpcServerHandler for Handler {
    type Action = Action;
    type Response = Response;

    async fn handle(&self, action: Action) -> RpcResult<Response> {
        Ok(match action {
            Action::Pair { token, client_name } => match self.state.consume_pairing(&token, &client_name) {
                Some(id) => Response::Paired { client_id: id, secret: String::new() },
                None => Response::Rejected("invalid or expired pairing token".into()),
            },
            Action::Sync { auth, device, req } => Response::Sync(self.host.handle(&auth, &device, req).await),
            _ => Response::Rejected("embedding is not enabled on this server".into()),
        })
    }
}

struct Fixture {
    _state_dir: tempfile::TempDir,
    _host_dir: tempfile::TempDir,
    host_root: std::path::PathBuf,
    state: Arc<StateDir>,
    host: Arc<SyncHost>,
    sync_id: String,
    addr: String,
}

async fn fixture() -> Fixture {
    let state_dir = tempfile::tempdir().unwrap();
    let host_dir = tempfile::tempdir().unwrap();
    let host_root = host_dir.path().canonicalize().unwrap();
    write(&host_root, "hello.txt", "hello from the host");
    write(&host_root, "docs/readme.md", "# readme");
    let state = Arc::new(StateDir::open(state_dir.path()).unwrap());
    let mut file = SyncsFile::default();
    let cfg = file.add("Docs", &host_root, None, BackupPolicy::default()).unwrap();
    file.save(state.dir()).unwrap();

    let st = state.clone();
    let host = tokio::task::spawn_blocking(move || SyncHost::new(st, "TestHost")).await.unwrap();
    host.start_background();
    let ident = state.identity().unwrap();
    let tls = pinning::server_tls(ident.cert_der.clone(), ident.key_der.clone()).unwrap();
    let server = RpcServer::new("127.0.0.1:0".parse().unwrap(), Handler { host: host.clone(), state: state.clone() })
        .await
        .unwrap()
        .with_persistence(host.persistent())
        .with_config(ServerConfig { max_message_bytes: MAX_MESSAGE_BYTES, tls: Some(tls), ..Default::default() });
    let addr = server.local_addr().unwrap().to_string();
    tokio::spawn(async move { server.run().await });
    Fixture { _state_dir: state_dir, _host_dir: host_dir, host_root, state, host, sync_id: cfg.id, addr }
}

fn code(f: &Fixture, access: &str) -> String {
    let mut g = Grants { embed: false, syncs: BTreeMap::new() };
    g.syncs.insert(f.sync_id.clone(), access.to_string());
    f.state.new_pairing(&f.addr, g).unwrap().encode()
}

fn engine(root: &std::path::Path, state: &std::path::Path, client: &Arc<HostClient>, sync_id: &str, access: Access) -> Engine {
    std::fs::create_dir_all(root).unwrap();
    let cfg = EngineConfig {
        device: "Laptop".into(),
        policy: ConflictPolicy::ServerWins,
        access,
        state_dir: state.to_path_buf(),
        settle_ms: 0,
        ignore: vec![],
    };
    Engine::new(cfg, Arc::new(Mirror::new(root)), Arc::new(client.transport(sync_id))).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pair_list_sync_and_notify() {
    let f = fixture().await;

    // a wrong code is refused, a good one pairs
    let creds = pair(&code(&f, "rw"), "Laptop").await.unwrap();
    assert!(!creds.id.is_empty());
    assert_eq!(creds.name, "TestHost");
    let client = HostClient::new(tokio::runtime::Handle::current(), creds, "Laptop");
    let syncs = client.list_syncs().await.unwrap();
    assert_eq!(syncs.len(), 1);
    assert_eq!(syncs[0].name, "Docs");
    assert_eq!(syncs[0].access, Access::ReadWrite);
    assert_eq!(syncs[0].files, 2);

    // join + sync both ways, with a file that needs several frames
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let (root, st) = (base.join("files"), base.join("state"));
    let c2 = client.clone();
    let id = f.sync_id.clone();
    let (root2, st2) = (root.clone(), st.clone());
    let big: String = (0..(9 * 1024 * 1024 / 16)).map(|i| format!("{:015}\n", i)).collect();
    let big2 = big.clone();
    let (root3, host_root) = (root.clone(), f.host_root.clone());
    tokio::task::spawn_blocking(move || {
        let mut e = engine(&root2, &st2, &c2, &id, Access::ReadWrite);
        let r = e.sync_once().unwrap();
        assert!(r.downloaded >= 2, "{r:?}");
        assert_eq!(std::fs::read_to_string(root3.join("hello.txt")).unwrap(), "hello from the host");
        write(&root3, "from-laptop.bin", &big2);
        write(&root3, "notes/today.txt", "written on the laptop");
        let r = e.sync_once().unwrap();
        assert!(r.pushed >= 2 && r.uploaded >= 2, "{r:?}");
        // exactly what was written arrived on the host
        assert_eq!(std::fs::read_to_string(host_root.join("notes/today.txt")).unwrap(), "written on the laptop");
        assert_eq!(std::fs::read_to_string(host_root.join("from-laptop.bin")).unwrap().len(), big2.len());
        let r = e.sync_once().unwrap();
        assert_eq!(r.pushed + r.downloaded + r.conflicts, 0, "{r:?}");
    })
    .await
    .unwrap();

    // the host's watcher notices direct edits and the notification stream announces them
    let (ntx, nrx) = std::sync::mpsc::channel::<String>();
    let _sub = client.subscribe(vec![SubscribeItem { sync: f.sync_id.clone(), cursor: f.host.sync(&f.sync_id).unwrap().seq() }], ntx);
    // the first message is the "connected" marker
    let first = tokio::task::spawn_blocking(move || {
        let m = nrx.recv_timeout(Duration::from_secs(10)).unwrap();
        (m, nrx)
    })
    .await
    .unwrap();
    assert_eq!(first.0, "");
    tokio::time::sleep(Duration::from_millis(300)).await;
    write(&f.host_root, "edited-on-host.txt", "direct edit");
    let nrx = first.1;
    let got = tokio::task::spawn_blocking(move || nrx.recv_timeout(Duration::from_secs(15))).await.unwrap();
    assert_eq!(got.unwrap(), f.sync_id, "no notification for a direct host edit");

    // the second device converges to the same tree
    let creds2 = pair(&code(&f, "rw"), "Desktop").await.unwrap();
    let client2 = HostClient::new(tokio::runtime::Handle::current(), creds2, "Desktop");
    let dir2 = tempfile::tempdir().unwrap();
    let base2 = dir2.path().canonicalize().unwrap();
    let (root_b, st_b, id) = (base2.join("files"), base2.join("state"), f.sync_id.clone());
    let host_tree = tokio::task::spawn_blocking({
        let hr = f.host_root.clone();
        move || {
            std::thread::sleep(Duration::from_millis(1500)); // watcher debounce
            tree(&hr)
        }
    })
    .await
    .unwrap();
    let rb = root_b.clone();
    let tree_b = tokio::task::spawn_blocking(move || {
        let mut e = engine(&rb, &st_b, &client2, &id, Access::ReadWrite);
        e.sync_once().unwrap();
        tree(&rb)
    })
    .await
    .unwrap();
    assert_eq!(tree_b, host_tree);
    assert!(root.join("from-laptop.bin").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_only_grant_and_revocation() {
    let f = fixture().await;
    let creds = pair(&code(&f, "ro"), "Viewer").await.unwrap();
    let client_id = creds.client_id.clone();
    let client = HostClient::new(tokio::runtime::Handle::current(), creds, "Viewer");
    assert_eq!(client.list_syncs().await.unwrap()[0].access, Access::ReadOnly);

    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let (root, st, id) = (base.join("files"), base.join("state"), f.sync_id.clone());
    let c2 = client.clone();
    let r2 = root.clone();
    tokio::task::spawn_blocking(move || {
        let mut e = engine(&r2, &st, &c2, &id, Access::ReadOnly);
        e.sync_once().unwrap();
        assert!(r2.join("hello.txt").exists());
        // a read-only device cannot add files
        write(&r2, "sneaky.txt", "x");
        e.sync_once().unwrap();
    })
    .await
    .unwrap();
    assert!(!f.host_root.join("sneaky.txt").exists());

    // an upgrade needs no new code
    f.state.set_grant(&client_id, &f.sync_id, Some(Access::ReadWrite)).unwrap();
    assert_eq!(client.list_syncs().await.unwrap()[0].access, Access::ReadWrite);

    // revoking takes effect on the next request
    f.state.set_grant(&client_id, &f.sync_id, None).unwrap();
    assert!(client.list_syncs().await.unwrap().is_empty());
    f.state.revoke(&client_id).unwrap();
    assert!(client.list_syncs().await.is_err());
    let _ = hex(&[0u8]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pairing_is_one_time_and_pinned() {
    let f = fixture().await;
    let c = code(&f, "rw");
    pair(&c, "One").await.unwrap();
    assert!(pair(&c, "Two").await.is_err(), "a pairing code works once");

    // a code with the wrong certificate fingerprint cannot connect
    let mut parsed = medialake_proto::pairing::PairingCode::decode(&code(&f, "rw")).unwrap();
    parsed.fingerprint[0] ^= 0xff;
    assert!(pair(&parsed.encode(), "Evil").await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfers_resume_over_the_wire() {
    use medialake_sync::member::transport::{Fetch, Transport, Upload};
    let f = fixture().await;
    let creds = pair(&code(&f, "rw"), "Laptop").await.unwrap();
    let client = HostClient::new(tokio::runtime::Handle::current(), creds, "Laptop");
    let content: Vec<u8> = (0..6_000_000u32).map(|i| (i.wrapping_mul(31) % 251) as u8).collect();
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("big.bin");
    std::fs::write(&src, &content).unwrap();
    let (hash, size) = medialake_sync::util::hash_file(&src).unwrap();

    let hs = f.host.sync(&f.sync_id).unwrap();
    // an earlier attempt died after 2.5 MB
    std::fs::write(hs.upload_part_path(&hash), &content[..2_500_000]).unwrap();
    let t = client.transport(&f.sync_id);
    let (src2, hash2) = (src.clone(), hash.clone());
    let r = tokio::task::spawn_blocking(move || t.upload(&hash2, &src2, size)).await.unwrap().unwrap();
    assert_eq!(r, Upload::Done);
    assert_eq!(hs.upload_have(&hash, size), size);

    // put the file into the sync through the host so that there is something to download
    let r = hs
        .commit(
            Access::ReadWrite,
            "x",
            &[medialake_proto::sync::Change::Put { path: "big.bin".into(), hash: hash.clone(), size, mtime: 0, exec: false, base_rev: 0 }],
        )
        .unwrap();
    assert!(matches!(r[0], medialake_proto::sync::ChangeResult::Ok { .. }), "{r:?}");
    let rev = hs.entry("big.bin").unwrap().rev;

    // download side: a 4 MB prefix is already there
    let dest = dir.path().join("big.dl");
    std::fs::write(&dest, &content[..4_000_000]).unwrap();
    let t = client.transport(&f.sync_id);
    let d2 = dest.clone();
    let got = tokio::task::spawn_blocking(move || t.download("big.bin", rev, &d2)).await.unwrap().unwrap();
    assert_eq!(got, Fetch::Done);
    assert_eq!(std::fs::read(&dest).unwrap(), content);
}

/// Throughput of one transfer in each direction over loopback TLS:
/// `SYNC_BIG_MB=1024 cargo test --release -p medialake-sync --test network -- --ignored --nocapture`
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn throughput_localhost() {
    use medialake_sync::member::transport::{Fetch, Transport, Upload};
    let mb: usize = std::env::var("SYNC_BIG_MB").ok().and_then(|s| s.parse().ok()).unwrap_or(256);
    let f = fixture().await;
    let creds = pair(&code(&f, "rw"), "Laptop").await.unwrap();
    let client = HostClient::new(tokio::runtime::Handle::current(), creds, "Laptop");
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("big.bin");
    {
        use std::io::Write;
        let mut w = std::fs::File::create(&src).unwrap();
        let block: Vec<u8> = (0..1_048_576u32).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8).collect();
        for i in 0..mb {
            let mut b = block.clone();
            b[0] = i as u8;
            b[1] = (i >> 8) as u8;
            w.write_all(&b).unwrap();
        }
    }
    let (hash, size) = medialake_sync::util::hash_file(&src).unwrap();
    let t = client.transport(&f.sync_id);
    let (s2, h2) = (src.clone(), hash.clone());
    let started = std::time::Instant::now();
    let r = tokio::task::spawn_blocking(move || t.upload(&h2, &s2, size)).await.unwrap().unwrap();
    let up = started.elapsed();
    assert_eq!(r, Upload::Done);
    let hs = f.host.sync(&f.sync_id).unwrap();
    hs.commit(
        Access::ReadWrite,
        "x",
        &[medialake_proto::sync::Change::Put { path: "big.bin".into(), hash, size, mtime: 0, exec: false, base_rev: 0 }],
    )
    .unwrap();
    let rev = hs.entry("big.bin").unwrap().rev;
    let t = client.transport(&f.sync_id);
    let dest = dir.path().join("big.dl");
    let d2 = dest.clone();
    let started = std::time::Instant::now();
    let r = tokio::task::spawn_blocking(move || t.download("big.bin", rev, &d2)).await.unwrap().unwrap();
    let down = started.elapsed();
    assert_eq!(r, Fetch::Done);
    assert_eq!(std::fs::metadata(&dest).unwrap().len(), size);
    println!("upload   {mb} MiB in {:.2} s = {:.0} MiB/s", up.as_secs_f64(), mb as f64 / up.as_secs_f64());
    println!("download {mb} MiB in {:.2} s = {:.0} MiB/s", down.as_secs_f64(), mb as f64 / down.as_secs_f64());
}
