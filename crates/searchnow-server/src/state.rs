//! Persistent state of a remote-mode server: TLS identity, pending pairings, paired clients.
//! Everything is plain files in `state_dir` so that `searchnow-server pair|clients` can run while
//! `serve` is running.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use searchnow_proto::pairing::{PairingCode, hex, unhex};
use searchnow_proto::pinning;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const PAIRING_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ClientRecord {
    pub name: String,
    /// SHA-256 (hex) of the client's shared secret.
    pub secret_sha256: String,
    pub created: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Pending {
    secret_hex: String,
    expires: u64,
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

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("system random source");
    b
}

pub fn default_state_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("SearchNow").join("server")
}

impl StateDir {
    pub fn open(dir: &Path) -> anyhow::Result<StateDir> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(StateDir { dir: dir.to_path_buf(), clients: Mutex::new((None, BTreeMap::new())) })
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

    fn read_json<T: for<'a> Deserialize<'a> + Default>(&self, name: &str) -> T {
        std::fs::read_to_string(self.dir.join(name)).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    fn write_json<T: Serialize>(&self, name: &str, v: &T) -> anyhow::Result<()> {
        let tmp = self.dir.join(format!("{name}.tmp"));
        std::fs::write(&tmp, serde_json::to_string_pretty(v)?)?;
        std::fs::rename(tmp, self.dir.join(name))?;
        Ok(())
    }

    /// Public address (`host:port`) remembered from the last `serve`.
    pub fn public_addr(&self) -> Option<String> {
        std::fs::read_to_string(self.dir.join("public_addr.txt")).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    }

    pub fn set_public_addr(&self, addr: &str) {
        let _ = std::fs::write(self.dir.join("public_addr.txt"), addr);
    }

    /// Creates a one-time pairing and returns the pasteable code.
    pub fn new_pairing(&self, address: &str) -> anyhow::Result<PairingCode> {
        let id = self.identity()?;
        let code = PairingCode { address: address.to_string(), token: random_bytes(), secret: random_bytes(), fingerprint: id.fingerprint() };
        let mut pending: BTreeMap<String, Pending> = self.read_json("pending.json");
        let t = now();
        pending.retain(|_, p| p.expires > t);
        pending.insert(hex(&code.token), Pending { secret_hex: hex(&code.secret), expires: t + PAIRING_TTL.as_secs() });
        self.write_json("pending.json", &pending)?;
        Ok(code)
    }

    /// Burns a one-time token and registers the client. Returns the new client id.
    pub fn consume_pairing(&self, token_hex: &str, client_name: &str) -> Option<String> {
        let mut pending: BTreeMap<String, Pending> = self.read_json("pending.json");
        let p = pending.remove(token_hex)?;
        let _ = self.write_json("pending.json", &pending);
        if p.expires <= now() {
            return None;
        }
        let secret = unhex(&p.secret_hex)?;
        let client_id = hex(&random_bytes::<6>());
        let mut clients: BTreeMap<String, ClientRecord> = self.read_json("clients.json");
        clients.insert(
            client_id.clone(),
            ClientRecord { name: client_name.chars().take(64).collect(), secret_sha256: hex(&Sha256::digest(&secret)), created: now() },
        );
        self.write_json("clients.json", &clients).ok()?;
        Some(client_id)
    }

    pub fn list_clients(&self) -> BTreeMap<String, ClientRecord> {
        self.read_json("clients.json")
    }

    pub fn revoke(&self, id: &str) -> anyhow::Result<bool> {
        let mut clients: BTreeMap<String, ClientRecord> = self.read_json("clients.json");
        let existed = clients.remove(id).is_some();
        if existed {
            self.write_json("clients.json", &clients)?;
        }
        Ok(existed)
    }

    /// Checks `client_id:secret_hex`. The client list is re-read whenever the file changed.
    pub fn verify(&self, presented: &str) -> bool {
        let Some((id, secret_hex)) = presented.split_once(':') else { return false };
        let Some(secret) = unhex(secret_hex) else { return false };
        let mtime = std::fs::metadata(self.dir.join("clients.json")).and_then(|m| m.modified()).ok();
        let mut g = self.clients.lock().unwrap();
        if g.0 != mtime || g.1.is_empty() {
            g.1 = self.read_json("clients.json");
            g.0 = mtime;
        }
        match g.1.get(id) {
            Some(rec) => ct_eq(rec.secret_sha256.as_bytes(), hex(&Sha256::digest(&secret)).as_bytes()),
            None => false,
        }
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
