//! Remote server support: pairing, credential protection (DPAPI) and a reconnecting backend.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bierpc::rpc::{ClientConfig, RpcClient};
use searchnow_proto::pairing::{PairingCode, hex, unhex};
use searchnow_proto::{Action, MAX_MESSAGE_BYTES, PROTO_VERSION, Response, ServerInfo, pinning};
use tokio::sync::{Notify, watch};

use crate::config::RemoteCreds;
use crate::supervisor::{Backend, ConnTarget, ServerState, next_generation};

// ---------------- DPAPI ----------------

#[cfg(windows)]
pub fn protect(data: &[u8]) -> Result<String, String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CRYPT_INTEGER_BLOB, CryptProtectData};
    unsafe {
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        let ok = CryptProtectData(&input, std::ptr::null(), std::ptr::null(), std::ptr::null(), std::ptr::null(), 0, &mut out);
        if ok == 0 {
            return Err("CryptProtectData failed".into());
        }
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(out.pbData as *mut _);
        Ok(hex(&v))
    }
}

#[cfg(windows)]
pub fn unprotect(text: &str) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CRYPT_INTEGER_BLOB, CryptUnprotectData};
    let data = unhex(text).ok_or("corrupt credential")?;
    unsafe {
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        let ok = CryptUnprotectData(&input, std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), std::ptr::null(), 0, &mut out);
        if ok == 0 {
            return Err("cannot decrypt the stored credential (different Windows user?)".into());
        }
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        LocalFree(out.pbData as *mut _);
        Ok(v)
    }
}

#[cfg(not(windows))]
pub fn protect(data: &[u8]) -> Result<String, String> {
    Ok(hex(data))
}

#[cfg(not(windows))]
pub fn unprotect(text: &str) -> Result<Vec<u8>, String> {
    unhex(text).ok_or_else(|| "corrupt credential".into())
}

// ---------------- connection ----------------

pub struct RemoteConn {
    pub address: String,
    pub fingerprint: [u8; 32],
    /// `client_id:secret_hex`
    pub auth: String,
}

impl RemoteConn {
    pub fn from_creds(c: &RemoteCreds) -> Result<RemoteConn, String> {
        let secret = unprotect(&c.secret_protected)?;
        let fp = unhex(&c.fingerprint).filter(|v| v.len() == 32).ok_or("corrupt fingerprint")?;
        let mut fingerprint = [0u8; 32];
        fingerprint.copy_from_slice(&fp);
        Ok(RemoteConn { address: c.address.clone(), fingerprint, auth: format!("{}:{}", c.client_id, hex(&secret)) })
    }
}

/// Resolves `host:port` (RpcClient wants a SocketAddr). Prefers IPv4 when both exist.
pub async fn resolve(address: &str) -> Result<SocketAddr, String> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(address).await.map_err(|e| format!("cannot resolve {address}: {e}"))?.collect();
    addrs.iter().find(|a| a.is_ipv4()).or(addrs.first()).copied().ok_or_else(|| format!("no address for {address}"))
}

async fn connect(addr: SocketAddr, fp: [u8; 32]) -> Result<RpcClient<Action>, String> {
    let cfg = ClientConfig {
        max_message_bytes: MAX_MESSAGE_BYTES,
        tls: Some(pinning::client_tls(fp)),
        connect_timeout: Duration::from_secs(10),
        call_timeout: Some(Duration::from_secs(30)),
        ..Default::default()
    };
    RpcClient::<Action>::new_with_config(addr, cfg).await.map_err(|e| format!("cannot connect to {addr}: {e:?}"))
}

pub struct Paired {
    pub creds: RemoteCreds,
    pub info: ServerInfo,
}

/// Redeems a pairing code: connects with certificate pinning, exchanges the one-time token for a
/// client id, and verifies the resulting credentials.
pub async fn pair(code_text: &str, client_name: &str) -> Result<Paired, String> {
    let code = PairingCode::decode(code_text).map_err(|e| e.to_string())?;
    let addr = resolve(&code.address).await?;
    let mut c = connect(addr, code.fingerprint).await?;
    let r = c
        .call::<Response>(Action::Pair { token: hex(&code.token), client_name: client_name.to_string() })
        .await
        .map_err(|e| format!("pairing failed: {e:?}"))?;
    let client_id = match r {
        Response::Paired { client_id, .. } => client_id,
        Response::Rejected(m) => return Err(m),
        other => return Err(format!("unexpected response {other:?}")),
    };
    let auth = format!("{client_id}:{}", hex(&code.secret));
    let info = match c.call::<Response>(Action::Hello { proto_version: PROTO_VERSION, auth }).await {
        Ok(Response::Info(i)) => i,
        Ok(Response::Rejected(m)) => return Err(m),
        Ok(other) => return Err(format!("unexpected response {other:?}")),
        Err(e) => return Err(format!("handshake failed: {e:?}")),
    };
    Ok(Paired {
        creds: RemoteCreds {
            address: code.address,
            fingerprint: hex(&code.fingerprint),
            client_id,
            secret_protected: protect(&code.secret)?,
        },
        info,
    })
}

// ---------------- backend ----------------

/// Publishes the remote server as a [`ServerState`]; re-resolves the address after failures.
pub struct RemoteBackend {
    failure: Notify,
    stop: Notify,
    current: std::sync::atomic::AtomicU64,
}

impl RemoteBackend {
    pub fn start(conn: RemoteConn) -> (Arc<RemoteBackend>, watch::Receiver<ServerState>) {
        let (tx, rx) = watch::channel(ServerState::Starting);
        let b = Arc::new(RemoteBackend { failure: Notify::new(), stop: Notify::new(), current: Default::default() });
        let b2 = b.clone();
        tokio::spawn(async move { b2.run(conn, tx).await });
        (b, rx)
    }

    pub fn stop(&self) {
        self.stop.notify_one();
    }

    async fn run(self: Arc<Self>, conn: RemoteConn, tx: watch::Sender<ServerState>) {
        let mut backoff = Duration::from_secs(2);
        loop {
            let _ = tx.send(ServerState::Starting);
            match resolve(&conn.address).await {
                Ok(addr) => {
                    let generation = next_generation();
                    self.current.store(generation, std::sync::atomic::Ordering::SeqCst);
                    let target = ConnTarget {
                        addr,
                        tls: Some(pinning::client_tls(conn.fingerprint)),
                        auth: conn.auth.clone(),
                        generation,
                        info: None,
                    };
                    let _ = tx.send(ServerState::Ready(Arc::new(target)));
                    tokio::select! {
                        _ = self.failure.notified() => {
                            // let the caller see the failure, then re-resolve (the host may have a new IP)
                            tokio::time::sleep(backoff).await;
                            backoff = (backoff * 2).min(Duration::from_secs(30));
                        }
                        _ = self.stop.notified() => return,
                    }
                }
                Err(e) => {
                    let _ = tx.send(ServerState::Failed(e));
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = self.stop.notified() => return,
                    }
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
        }
    }
}

impl Backend for RemoteBackend {
    fn ensure_running(&self) {}
    fn touch(&self) {}
    fn report_failure(&self, generation: u64) {
        if self.current.load(std::sync::atomic::Ordering::SeqCst) == generation {
            self.failure.notify_one();
        }
    }
}
