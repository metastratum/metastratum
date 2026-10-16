//! First contact with the controller, and the identity it hands out.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use mst_proto::enrollment_client::EnrollmentClient;
use mst_proto::{EnrollRequest, HostFacts};
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, RootCertStore, SignatureScheme};
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity};

/// TLS server name the controller's certificate always carries.
pub const SERVER_NAME: &str = "mst-controller";

/// Accepts the controller only if the CA in its certificate chain has the
/// fingerprint from the join token, then verifies the chain against that CA
/// as usual. This authenticates the controller before the agent trusts it
/// with anything.
#[derive(Debug)]
struct PinnedCa {
    fingerprint: String,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedCa {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let ca = intermediates
            .iter()
            .find(|c| mst_pki::fingerprint(c) == self.fingerprint)
            .ok_or_else(|| {
                rustls::Error::General("controller CA does not match the join token".into())
            })?;
        let mut roots = RootCertStore::empty();
        roots.add(ca.clone().into_owned())?;
        let inner =
            WebPkiServerVerifier::builder_with_provider(Arc::new(roots), self.provider.clone())
                .build()
                .map_err(|e| rustls::Error::General(e.to_string()))?;
        inner.verify_server_cert(end_entity, intermediates, server_name, ocsp, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
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
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
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

/// The host's credentials, persisted in the agent's state directory.
#[derive(Clone)]
pub struct HostIdentity {
    pub host_id: String,
    pub key_pem: String,
    pub cert_pem: String,
    pub ca_pem: String,
}

fn paths(dir: &Path) -> [PathBuf; 4] {
    ["host_id", "agent.key", "agent.crt", "ca.crt"].map(|f| dir.join(f))
}

impl HostIdentity {
    pub fn load(dir: &Path) -> Result<Option<Self>> {
        let [id, key, cert, ca] = paths(dir);
        if !id.exists() {
            return Ok(None);
        }
        let read = |p: &Path| {
            std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))
        };
        Ok(Some(Self {
            host_id: read(&id)?.trim().to_owned(),
            key_pem: read(&key)?,
            cert_pem: read(&cert)?,
            ca_pem: read(&ca)?,
        }))
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::create_dir_all(dir)?;
        let [id, key, cert, ca] = paths(dir);
        // Write the key first and the host ID last: the ID's presence marks a
        // complete identity.
        for (path, data, mode) in [
            (&key, &self.key_pem, 0o600),
            (&cert, &self.cert_pem, 0o644),
            (&ca, &self.ca_pem, 0o644),
            (&id, &self.host_id, 0o644),
        ] {
            let tmp = path.with_extension("tmp");
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(mode)
                .open(&tmp)?;
            std::io::Write::write_all(&mut f, data.as_bytes())?;
            f.sync_all()?;
            std::fs::rename(&tmp, path)?;
        }
        Ok(())
    }

    pub fn tls(&self) -> ClientTlsConfig {
        ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(&self.ca_pem))
            .identity(Identity::from_pem(&self.cert_pem, &self.key_pem))
            .domain_name(SERVER_NAME)
    }
}

/// Enrolls this host with a join token and stores the resulting identity.
pub async fn enroll(
    controller: &str,
    token: &str,
    facts: HostFacts,
    state_dir: &Path,
) -> Result<HostIdentity> {
    let parsed: mst_pki::JoinToken = token.parse().context("malformed join token")?;
    let (key_pem, csr_pem) = mst_pki::new_key_and_csr(&facts.hostname)?;

    let verifier = Arc::new(PinnedCa {
        fingerprint: parsed.ca_fingerprint.clone(),
        provider: Arc::new(rustls::crypto::ring::default_provider()),
    });
    let channel = Endpoint::from_shared(controller.to_owned())?
        .tls_config_with_verifier(ClientTlsConfig::new().domain_name(SERVER_NAME), verifier)?
        .connect()
        .await
        .with_context(|| format!("connecting to {controller}"))?;

    let resp = EnrollmentClient::new(channel)
        .enroll(EnrollRequest {
            token: token.to_owned(),
            csr_pem,
            facts: Some(facts),
        })
        .await
        .map_err(|s| anyhow::anyhow!("enrollment rejected: {}", s.message()))?
        .into_inner();
    if mst_pki::fingerprint(&mst_pki::pem_to_der(&resp.ca_pem)?) != parsed.ca_fingerprint {
        bail!("controller returned a CA that does not match the join token");
    }

    let identity = HostIdentity {
        host_id: resp.host_id,
        key_pem,
        cert_pem: resp.cert_pem,
        ca_pem: resp.ca_pem,
    };
    identity.save(state_dir)?;
    Ok(identity)
}
