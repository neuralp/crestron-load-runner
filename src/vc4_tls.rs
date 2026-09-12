//! Certificate pinning for VC-4. An unapproved certificate aborts the TLS
//! handshake, before reqwest can send the Authorization header or HTTP data.
use std::sync::{Arc, Mutex};

use rustls::{
    DigitallySignedStruct, Error, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use sha2::{Digest, Sha256};

pub type UntrustedCertificate = Arc<Mutex<Option<String>>>;

pub fn fingerprint(der: &[u8]) -> String {
    let bytes = Sha256::digest(der);
    let hex: Vec<_> = bytes.iter().map(|byte| format!("{byte:02X}")).collect();
    format!("SHA256:{}", hex.join(":"))
}

#[derive(Debug)]
struct PinnedCertificate {
    trusted: Option<String>,
    untrusted: UntrustedCertificate,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for PinnedCertificate {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        let actual = fingerprint(end_entity.as_ref());
        if self.trusted.as_deref() == Some(&actual) {
            // Trust is the exact leaf certificate approved for this origin,
            // not a CA/name/expiry assertion. Signature checks below still
            // require the server to prove possession of its private key.
            Ok(ServerCertVerified::assertion())
        } else {
            *self
                .untrusted
                .lock()
                .map_err(|_| Error::General("Certificate state unavailable".into()))? =
                Some(actual);
            Err(Error::General(
                "HTTPS certificate has not been approved".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

pub fn config(
    trusted: Option<String>,
    untrusted: UntrustedCertificate,
) -> Result<rustls::ClientConfig, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = PinnedCertificate {
        trusted,
        untrusted,
        provider: provider.clone(),
    };
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|_| "Could not configure HTTPS protocols")?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    // Every new TLS connection must present the certificate, including after
    // rotation. Do not resume a session that could bypass the pin verifier.
    config.resumption = rustls::client::Resumption::disabled();
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_sha256_of_the_entire_der_certificate() {
        // Standard SHA-256 test vector; formatting matches the UI and storage.
        assert_eq!(
            fingerprint(b"abc"),
            "SHA256:BA:78:16:BF:8F:01:CF:EA:41:41:40:DE:5D:AE:22:23:B0:03:61:A3:96:17:7A:9C:B4:10:FF:61:F2:00:15:AD"
        );
    }
}
