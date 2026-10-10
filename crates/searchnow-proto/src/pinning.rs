//! TLS with a pinned self-signed certificate: the client trusts exactly the certificate whose
//! SHA-256 fingerprint is in the pairing code, and nothing else.

use std::sync::Arc;

use bierpc::rpc::ClientTlsConfig;
use bierpc::rustls;
use sha2::{Digest, Sha256};

/// Name the server certificate is issued for (the name is not what is verified, the pin is).
pub const SERVER_NAME: &str = "searchnow-server";

#[derive(Debug)]
struct PinnedCert {
    fingerprint: [u8; 32],
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if Sha256::digest(end_entity.as_ref()).as_slice() == self.fingerprint {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("server certificate does not match the pairing code".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        msg: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(msg, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        msg: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(msg, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

pub fn client_tls(fingerprint: [u8; 32]) -> ClientTlsConfig {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let cfg = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .expect("TLS protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCert { fingerprint, provider }))
        .with_no_client_auth();
    ClientTlsConfig::new(cfg, rustls::pki_types::ServerName::try_from(SERVER_NAME).unwrap())
}

pub fn server_tls(cert_der: Vec<u8>, key_pkcs8_der: Vec<u8>) -> Result<Arc<rustls::ServerConfig>, String> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(key_pkcs8_der.into());
    let cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(vec![rustls::pki_types::CertificateDer::from(cert_der)], key)
        .map_err(|e| e.to_string())?;
    Ok(Arc::new(cfg))
}

pub fn fingerprint(cert_der: &[u8]) -> [u8; 32] {
    Sha256::digest(cert_der).into()
}
