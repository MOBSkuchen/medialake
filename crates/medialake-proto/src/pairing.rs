//! Pairing code: everything a client needs to connect to a remote server, as one pasteable string.
//!
//! `SN1-XXXXX-XXXXX-...` = base32( version | cert fingerprint (32) | one-time token (16) | shared secret (16)
//!                                  | address length (1) | address | checksum (2) )

use sha2::{Digest, Sha256};

pub const VERSION: u8 = 1;
const PREFIX: &str = "SN1-";
const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingCode {
    /// `host:port`, resolved by the client.
    pub address: String,
    pub token: [u8; 16],
    pub secret: [u8; 16],
    /// SHA-256 of the server's self-signed certificate (DER).
    pub fingerprint: [u8; 32],
}

#[derive(Debug, PartialEq, Eq)]
pub enum PairingError {
    Malformed,
    Checksum,
    Version(u8),
}

impl std::fmt::Display for PairingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PairingError::Malformed => write!(f, "this is not a MediaLake pairing code"),
            PairingError::Checksum => write!(f, "the pairing code has a typo (checksum mismatch); copy it again"),
            PairingError::Version(v) => write!(f, "pairing code version {v} is not supported; update MediaLake"),
        }
    }
}

impl std::error::Error for PairingError {}

fn checksum(data: &[u8]) -> [u8; 2] {
    let h = Sha256::digest(data);
    [h[0], h[1]]
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

fn base32_encode(data: &[u8]) -> String {
    let mut out = String::new();
    let (mut buf, mut bits) = (0u32, 0u32);
    for &b in data {
        buf = (buf << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            out.push(ALPHABET[((buf >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
            buf &= (1 << bits) - 1;
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut buf, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = ALPHABET.iter().position(|&a| a == c)? as u32;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            out.push((buf >> (bits - 8)) as u8);
            bits -= 8;
            buf &= (1 << bits) - 1;
        }
    }
    Some(out)
}

impl PairingCode {
    pub fn encode(&self) -> String {
        let mut raw = vec![VERSION];
        raw.extend_from_slice(&self.fingerprint);
        raw.extend_from_slice(&self.token);
        raw.extend_from_slice(&self.secret);
        let addr = self.address.as_bytes();
        let n = addr.len().min(255);
        raw.push(n as u8);
        raw.extend_from_slice(&addr[..n]);
        let c = checksum(&raw);
        raw.extend_from_slice(&c);
        let b32 = base32_encode(&raw);
        let groups: Vec<&str> = b32.as_bytes().chunks(5).map(|c| std::str::from_utf8(c).unwrap()).collect();
        format!("{PREFIX}{}", groups.join("-"))
    }

    pub fn decode(text: &str) -> Result<PairingCode, PairingError> {
        let cleaned: String = text.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_uppercase();
        let body = cleaned.strip_prefix(PREFIX).ok_or(PairingError::Malformed)?;
        let body: String = body.chars().filter(|c| *c != '-').collect();
        let raw = base32_decode(&body).ok_or(PairingError::Malformed)?;
        // version + fingerprint + token + secret + address length + checksum
        if raw.len() < 1 + 32 + 16 + 16 + 1 + 2 {
            return Err(PairingError::Malformed);
        }
        let (payload, sum) = raw.split_at(raw.len() - 2);
        if checksum(payload) != [sum[0], sum[1]] {
            return Err(PairingError::Checksum);
        }
        if payload[0] != VERSION {
            return Err(PairingError::Version(payload[0]));
        }
        let mut fingerprint = [0u8; 32];
        fingerprint.copy_from_slice(&payload[1..33]);
        let mut token = [0u8; 16];
        token.copy_from_slice(&payload[33..49]);
        let mut secret = [0u8; 16];
        secret.copy_from_slice(&payload[49..65]);
        let n = payload[65] as usize;
        let addr = payload.get(66..66 + n).ok_or(PairingError::Malformed)?;
        let address = String::from_utf8(addr.to_vec()).map_err(|_| PairingError::Malformed)?;
        Ok(PairingCode { address, token, secret, fingerprint })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PairingCode {
        PairingCode { address: "my-pc.local:47900".into(), token: [7; 16], secret: [9; 16], fingerprint: [0xAB; 32] }
    }

    #[test]
    fn roundtrip() {
        let c = sample();
        let text = c.encode();
        assert!(text.starts_with("SN1-"));
        assert_eq!(PairingCode::decode(&text).unwrap(), c);
        // pasted with whitespace and in lowercase
        assert_eq!(PairingCode::decode(&format!("  {}\n", text.to_lowercase())).unwrap(), c);
    }

    #[test]
    fn detects_typos() {
        let mut text = sample().encode().into_bytes();
        let i = text.len() - 3;
        text[i] = if text[i] == b'A' { b'B' } else { b'A' };
        assert!(PairingCode::decode(std::str::from_utf8(&text).unwrap()).is_err());
        assert_eq!(PairingCode::decode("hello"), Err(PairingError::Malformed));
    }
}
