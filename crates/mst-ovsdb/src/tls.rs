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

/// What an `ssl:` server presents.
pub struct Presented {
    /// Its certificate (DER).
    pub certificate: Vec<u8>,
    /// Whether that chains to the CA asked about, and if not, why.
    pub trusted: std::result::Result<(), String>,
}

/// Opens a TLS handshake with an `ssl:HOST:PORT` remote just far enough to
/// see its certificate, and checks it against `ca_pem` as connections do.
/// No client certificate is sent: this works whatever the server requires.
/// It reads the certificate (to watch its expiry); it does not authenticate
/// the server, and nothing is sent over the connection.
pub async fn presented(remote: &str, ca_pem: &[u8]) -> Result<Presented> {
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
    let hostport = remote
        .strip_prefix("ssl:")
        .ok_or_else(|| Error::Address(remote.into()))?;
    let host = hostport
        .rsplit_once(':')
        .map(|(h, _)| h.trim_matches(['[', ']']))
        .ok_or_else(|| Error::Address(remote.into()))?;
    let name = ServerName::try_from(host.to_owned()).map_err(|_| Error::Address(remote.into()))?;

    let mut roots = rustls::RootCertStore::empty();
    for c in CertificateDer::pem_slice_iter(ca_pem) {
        roots
            .add(c.map_err(|e| Error::Tls(format!("CA certificate: {e}")))?)
            .map_err(|e| Error::Tls(format!("CA certificate: {e}")))?;
    }
    let verifier = AnyName(
        WebPkiServerVerifier::builder(Arc::new(roots))
            .build()
            .map_err(|e| Error::Tls(format!("CA certificate: {e}")))?,
    );
    let seen = Arc::new(Seen::default());
    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(seen.clone())
        .with_no_client_auth();
    let stream = tokio::time::timeout(TIMEOUT, tokio::net::TcpStream::connect(hostport))
        .await
        .map_err(|_| Error::Timeout(TIMEOUT))??;
    // The server may refuse us for want of a client certificate; by then it
    // has shown its own.
    let _ = tokio::time::timeout(
        TIMEOUT,
        tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name.clone(), stream),
    )
    .await;
    let Some((leaf, intermediates)) = seen.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    else {
        return Err(Error::Tls(format!("{remote} presented no certificate")));
    };
    let trusted = verifier
        .verify_server_cert(&leaf, &intermediates, &name, &[], UnixTime::now())
        .map(|_| ())
        .map_err(|e| e.to_string());
    Ok(Presented {
        certificate: leaf.to_vec(),
        trusted,
    })
}

/// Records the server's certificates and accepts them: [`presented`] judges
/// them afterwards.
#[derive(Debug, Default)]
struct Seen(std::sync::Mutex<Option<(CertificateDer<'static>, Vec<CertificateDer<'static>>)>>);

impl ServerCertVerifier for Seen {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some((
            end_entity.clone().into_owned(),
            intermediates
                .iter()
                .map(|c| c.clone().into_owned())
                .collect(),
        ));
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::CryptoProvider::get_default()
            .map(|p| p.signature_verification_algorithms.supported_schemes())
            .unwrap_or_default()
    }
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
