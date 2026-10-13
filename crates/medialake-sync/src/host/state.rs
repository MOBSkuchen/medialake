//! Persistent state of a host (server or app): TLS identity, pending pairings, paired clients and their
//! grants. Plain files in one directory, so `medialake-server pair|clients|sync grant` can run while
//! `serve` is running. Moved here from `medialake-server` so that the app hosts with the same code and
//! file formats.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use anyhow::{Context, bail};
use medialake_proto::pairing::{PairingCode, hex, unhex};
use medialake_proto::pinning;
use medialake_proto::sync::Access;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::util::{now_secs, random_bytes};

pub const PAIRING_TTL: Duration = Duration::from_secs(10 * 60);

fn yes() -> bool {
    true
}

/// Records without the sync fields mean "embedding only", which is exactly what they could do before.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ClientRecord {
    pub name: String,
    /// SHA-256 (hex) of the client's shared secret.
    pub secret_sha256: String,
    pub created: u64,
    #[serde(default = "yes")]
    pub embed: bool,
    /// sync id -> `"rw"` / `"ro"`
    #[serde(default)]
    pub syncs: BTreeMap<String, String>,
}

impl ClientRecord {
    pub fn access(&self, sync: &str) -> Option<Access> {
        self.syncs.get(sync).and_then(|a| Access::parse(a))
    }
}

/// What a pairing code is good for; stored on the host next to the pending token.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Grants {
    #[serde(default = "yes")]
    pub embed: bool,
    #[serde(default)]
    pub syncs: BTreeMap<String, String>,
}

impl Default for Grants {
    fn default() -> Self {
        Grants { embed: true, syncs: BTreeMap::new() }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Pending {
    secret_hex: String,
    expires: u64,
    #[serde(default)]
    grants: Grants,
}

pub struct Identity {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
}

impl Identity {
    pub fn fingerprint(&self) -> [u8; 32] {
        pinning::fingerprint(&self.cert_der)
    }
}

pub struct StateDir {
    dir: PathBuf,
    /// Cached clients plus the file mtime they were read at (re-read when `clients revoke` runs).
    clients: Mutex<(Option<SystemTime>, BTreeMap<String, ClientRecord>)>,
}

pub fn default_state_dir() -> PathBuf {
    medialake_proto::platform::data_dir().join("server")
}

impl StateDir {
    pub fn open(dir: &Path) -> anyhow::Result<StateDir> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(StateDir { dir: dir.to_path_buf(), clients: Mutex::new((None, BTreeMap::new())) })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn identity(&self) -> anyhow::Result<Identity> {
        let (c, k) = (self.dir.join("cert.der"), self.dir.join("key.der"));
        if c.exists() && k.exists() {
            return Ok(Identity { cert_der: std::fs::read(c)?, key_der: std::fs::read(k)? });
        }
        let cert = rcgen::generate_simple_self_signed(vec![pinning::SERVER_NAME.to_string()]).context("generating certificate")?;
        let id = Identity { cert_der: cert.cert.der().to_vec(), key_der: cert.signing_key.serialize_der() };
        std::fs::write(&c, &id.cert_der)?;
        std::fs::write(&k, &id.key_der)?;
        Ok(id)
    }

    /// A stable random id for this host (members key their per-host state on it).
    pub fn host_id(&self) -> String {
        let p = self.dir.join("host_id.txt");
        if let Ok(s) = std::fs::read_to_string(&p) {
            let s = s.trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
        let id = hex(&random_bytes::<8>());
        let _ = std::fs::write(&p, &id);
        id
    }

    /// One attempt. `None`: the file is there but could not be read just now. A writer replaces these
    /// files with a rename, and on Windows a read can collide with that for a moment; "could not read" must
    /// never be taken for "empty" (an empty list of clients means that nobody is paired).
    fn try_read_json<T: for<'a> Deserialize<'a> + Default>(&self, name: &str) -> Option<T> {
        match std::fs::read_to_string(self.dir.join(name)) {
            Ok(s) => serde_json::from_str(&s).ok(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(T::default()),
            Err(_) => None,
        }
    }

    /// For changing a file: its content, or an error rather than a default that would be written back.
    fn load_json<T: for<'a> Deserialize<'a> + Default>(&self, name: &str) -> anyhow::Result<T> {
        for _ in 0..10 {
            if let Some(v) = self.try_read_json(name) {
                return Ok(v);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        bail!("{name} cannot be read right now")
    }

    /// For looking only: what is there, or nothing.
    fn read_json<T: for<'a> Deserialize<'a> + Default>(&self, name: &str) -> T {
        self.load_json(name).unwrap_or_default()
    }

    fn write_json<T: Serialize>(&self, name: &str, v: &T) -> anyhow::Result<()> {
        let tmp = self.dir.join(format!("{name}.tmp"));
        std::fs::write(&tmp, serde_json::to_string_pretty(v)?)?;
        // a reader may have the old file open for an instant (Windows refuses the rename meanwhile)
        let mut last = None;
        for _ in 0..10 {
            match std::fs::rename(&tmp, self.dir.join(name)) {
                Ok(()) => return Ok(()),
                Err(e) => last = Some(e),
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Err(last.unwrap().into())
    }

    /// Public address (`host:port`) remembered from the last `serve`.
    pub fn public_addr(&self) -> Option<String> {
        std::fs::read_to_string(self.dir.join("public_addr.txt")).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    }

    pub fn set_public_addr(&self, addr: &str) {
        let _ = std::fs::write(self.dir.join("public_addr.txt"), addr);
    }

    /// Creates a one-time pairing and returns the pasteable code. `grants` is what the code is good for.
    pub fn new_pairing(&self, address: &str, grants: Grants) -> anyhow::Result<PairingCode> {
        let id = self.identity()?;
        let code = PairingCode { address: address.to_string(), token: random_bytes(), secret: random_bytes(), fingerprint: id.fingerprint() };
        let mut pending: BTreeMap<String, Pending> = self.load_json("pending.json")?;
        let t = now_secs() as u64;
        pending.retain(|_, p| p.expires > t);
        pending.insert(hex(&code.token), Pending { secret_hex: hex(&code.secret), expires: t + PAIRING_TTL.as_secs(), grants });
        self.write_json("pending.json", &pending)?;
        Ok(code)
    }

    /// Burns a one-time token and registers the client. Returns the new client id.
    pub fn consume_pairing(&self, token_hex: &str, client_name: &str) -> Option<String> {
        // the list of clients first: if it cannot be read now, the code is not used up for nothing
        let mut clients: BTreeMap<String, ClientRecord> = self.load_json("clients.json").ok()?;
        let mut pending: BTreeMap<String, Pending> = self.load_json("pending.json").ok()?;
        let p = pending.remove(token_hex)?;
        let _ = self.write_json("pending.json", &pending);
        if p.expires <= now_secs() as u64 {
            return None;
        }
        let secret = unhex(&p.secret_hex)?;
        let client_id = hex(&random_bytes::<6>());
        clients.insert(
            client_id.clone(),
            ClientRecord {
                name: client_name.chars().take(64).collect(),
                secret_sha256: hex(&Sha256::digest(&secret)),
                created: now_secs() as u64,
                embed: p.grants.embed,
                syncs: p.grants.syncs,
            },
        );
        self.write_json("clients.json", &clients).ok()?;
        Some(client_id)
    }

    pub fn list_clients(&self) -> BTreeMap<String, ClientRecord> {
        self.read_json("clients.json")
    }

    /// Finds a client by id or, if unambiguous, by name.
    pub fn find_client(&self, id_or_name: &str) -> Option<String> {
        let clients = self.list_clients();
        if clients.contains_key(id_or_name) {
            return Some(id_or_name.to_string());
        }
        let mut by_name = clients.iter().filter(|(_, c)| c.name == id_or_name).map(|(i, _)| i.clone());
        let first = by_name.next()?;
        by_name.next().is_none().then_some(first)
    }

    /// Records the name a paired device now calls itself. Writes only when it differs.
    pub fn rename_client(&self, id: &str, name: &str) -> anyhow::Result<()> {
        let mut clients: BTreeMap<String, ClientRecord> = self.load_json("clients.json")?;
        let name: String = name.chars().take(64).collect();
        match clients.get_mut(id) {
            Some(rec) if rec.name != name => {
                rec.name = name;
                self.write_json("clients.json", &clients)
            }
            _ => Ok(()),
        }
    }

    pub fn revoke(&self, id: &str) -> anyhow::Result<bool> {
        let mut clients: BTreeMap<String, ClientRecord> = self.load_json("clients.json")?;
        let existed = clients.remove(id).is_some();
        if existed {
            self.write_json("clients.json", &clients)?;
        }
        Ok(existed)
    }

    /// Grants (or changes) a client's access to a sync. `None` removes the grant.
    pub fn set_grant(&self, client: &str, sync: &str, access: Option<Access>) -> anyhow::Result<()> {
        let mut clients: BTreeMap<String, ClientRecord> = self.load_json("clients.json")?;
        let Some(rec) = clients.get_mut(client) else { bail!("no client {client}") };
        match access {
            Some(a) => {
                rec.syncs.insert(sync.to_string(), a.as_str().to_string());
            }
            None => {
                rec.syncs.remove(sync);
            }
        }
        self.write_json("clients.json", &clients)
    }

    /// Removes every grant of a sync (the sync was deleted).
    pub fn drop_sync_grants(&self, sync: &str) -> anyhow::Result<()> {
        let mut clients: BTreeMap<String, ClientRecord> = self.load_json("clients.json")?;
        for c in clients.values_mut() {
            c.syncs.remove(sync);
        }
        self.write_json("clients.json", &clients)
    }

    /// Checks `client_id:secret_hex` and returns the client. The client list is re-read whenever the
    /// file changed, so revocations and new grants take effect on the next request.
    pub fn authenticate(&self, presented: &str) -> Option<(String, ClientRecord)> {
        let (id, secret_hex) = presented.split_once(':')?;
        let secret = unhex(secret_hex)?;
        let mtime = std::fs::metadata(self.dir.join("clients.json")).and_then(|m| m.modified()).ok();
        let mut g = self.clients.lock().unwrap();
        if g.0 != mtime || g.1.is_empty() {
            // a file that cannot be read this instant leaves what is known in place (and is looked at
            // again with the next request): otherwise every paired device would be turned away, and a
            // device that is told so forgets its pairing
            if let Ok(c) = self.load_json("clients.json") {
                g.1 = c;
                g.0 = mtime;
            }
        }
        let rec = g.1.get(id)?;
        ct_eq(rec.secret_sha256.as_bytes(), hex(&Sha256::digest(&secret)).as_bytes()).then(|| (id.to_string(), rec.clone()))
    }

    /// Embedding access: a valid client that is allowed to embed.
    pub fn verify(&self, presented: &str) -> bool {
        self.authenticate(presented).is_some_and(|(_, c)| c.embed)
    }
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn parse_hostport(s: &str) -> anyhow::Result<(String, u16)> {
    let (h, p) = s.rsplit_once(':').context("expected host:port")?;
    let port: u16 = p.parse().context("bad port")?;
    if h.is_empty() {
        bail!("empty host");
    }
    Ok((h.to_string(), port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_carries_grants() {
        let d = tempfile::tempdir().unwrap();
        let s = StateDir::open(d.path()).unwrap();
        let mut g = Grants { embed: false, syncs: BTreeMap::new() };
        g.syncs.insert("abc".into(), "ro".into());
        let code = s.new_pairing("host:1", g).unwrap();
        let id = s.consume_pairing(&hex(&code.token), "Laptop").unwrap();
        let auth = format!("{id}:{}", hex(&code.secret));
        let (cid, rec) = s.authenticate(&auth).unwrap();
        assert_eq!(cid, id);
        assert!(!rec.embed);
        assert!(!s.verify(&auth));
        assert_eq!(rec.access("abc"), Some(Access::ReadOnly));
        assert_eq!(rec.access("zzz"), None);
        s.set_grant(&id, "abc", Some(Access::ReadWrite)).unwrap();
        assert_eq!(s.authenticate(&auth).unwrap().1.access("abc"), Some(Access::ReadWrite));
        assert!(s.revoke(&id).unwrap());
        assert!(s.authenticate(&auth).is_none());
    }

    #[test]
    fn old_records_mean_embedding_only() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("clients.json"), r#"{"c1":{"name":"x","secret_sha256":"00","created":1}}"#).unwrap();
        let s = StateDir::open(d.path()).unwrap();
        let rec = &s.list_clients()["c1"];
        assert!(rec.embed);
        assert!(rec.syncs.is_empty());
    }
}
