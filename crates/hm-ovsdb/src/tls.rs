//! TLS for `ssl:` remotes, as OVN's own tools do it: the server's
//! certificate must chain to the given CA, and we present a client
//! certificate. Like OVS, the server's name is not checked; certificates
//! from `ovs-pki` do not carry one.

use std::sync::Arc;

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{CertificateError, DigitallySignedStruct, SignatureScheme};

use crate::client::{Error, Result};

/// The TLS client configuration for `ssl:` remotes, from PEM: the CA that
/// signed the servers' certificates, and our certificate and key.
pub fn client_config(
    ca_pem: &[u8],
    cert_pem: &[u8],
    key_pem: &[u8],
) -> Result<Arc<rustls::ClientConfig>> {
    let bad = |what: &str, e: &dyn std::fmt::Display| Error::Tls(format!("{what}: {e}"));
    let mut roots = rustls::RootCertStore::empty();
    for c in CertificateDer::pem_slice_iter(ca_pem) {
        roots
            .add(c.map_err(|e| bad("CA certificate", &e))?)
            .map_err(|e| bad("CA certificate", &e))?;
    }
    if roots.is_empty() {
        return Err(Error::Tls("no CA certificate found".into()));
    }
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert_pem)
        .collect::<Result<_, _>>()
        .map_err(|e| bad("certificate", &e))?;
    let key = PrivateKeyDer::from_pem_slice(key_pem).map_err(|e| bad("private key", &e))?;
    let verifier = WebPkiServerVerifier::builder(Arc::new(roots))
        .build()
        .map_err(|e| bad("CA certificate", &e))?;
    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyName(verifier)))
        .with_client_auth_cert(chain, key)
        .map_err(|e| bad("certificate and key", &e))?;
    Ok(Arc::new(config))
}

/// Checks the chain to the CA, but accepts any name in the certificate.
#[derive(Debug)]
struct AnyName(Arc<WebPkiServerVerifier>);

impl ServerCertVerifier for AnyName {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match self
            .0
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
        {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}
