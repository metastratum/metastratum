//! Internal PKI for metastratum.
//!
//! The controller owns a CA. It issues its own TLS server certificate for the
//! agent endpoint and signs host certificates during enrollment. Hosts identify
//! themselves to the controller with that certificate (the host ID is the
//! subject CN).
//!
//! An agent bootstraps trust from a join token, which carries the SHA-256
//! fingerprint of the CA certificate alongside a one-time secret, the same idea
//! as kubeadm's discovery token CA hash.

pub mod sealing;

pub use sealing::{SealError, Sealer, is_sealed};

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use rcgen::{
    BasicConstraints, CertificateParams, CertificateRevocationListParams,
    CertificateSigningRequestParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyIdMethod, KeyPair, KeyUsagePurpose, RevokedCertParams, SerialNumber,
};
use sha2::{Digest, Sha256};
use thiserror::Error;
use time::OffsetDateTime;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Rcgen(#[from] rcgen::Error),
    #[error("invalid certificate: {0}")]
    InvalidCertificate(String),
    #[error("malformed join token")]
    MalformedToken,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// SHA-256 over a certificate's DER encoding, lowercase hex.
pub fn fingerprint(cert_der: &[u8]) -> String {
    hex::encode(Sha256::digest(cert_der))
}

/// Hex-encoded random bytes from the OS CSPRNG.
pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::fill(buf.as_mut_slice());
    hex::encode(buf)
}

/// SHA-256 of a secret, for storing tokens and session IDs at rest.
pub fn hash_secret(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

fn serial() -> SerialNumber {
    let mut bytes = [0u8; 16];
    rand::fill(&mut bytes);
    bytes[0] &= 0x7f; // keep the DER integer positive
    SerialNumber::from(bytes.to_vec())
}

fn validity(params: &mut CertificateParams, lifetime: Duration) {
    let now = OffsetDateTime::now_utc();
    // Tolerate modest clock skew between controller and hosts.
    params.not_before = now - time::Duration::minutes(5);
    params.not_after = now + lifetime;
}

fn dn(common_name: &str) -> DistinguishedName {
    let mut dn = DistinguishedName::new();
    dn.push(DnType::OrganizationName, "metastratum");
    dn.push(DnType::CommonName, common_name);
    dn
}

pub struct Ca {
    cert_pem: String,
    cert_der: Vec<u8>,
    key: KeyPair,
}

impl Ca {
    pub fn generate(common_name: &str, lifetime: Duration) -> Result<Self> {
        let key = KeyPair::generate()?;
        let mut params = CertificateParams::default();
        params.distinguished_name = dn(common_name);
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params.serial_number = Some(serial());
        validity(&mut params, lifetime);
        let cert = params.self_signed(&key)?;
        Ok(Self {
            cert_pem: cert.pem(),
            cert_der: cert.der().to_vec(),
            key,
        })
    }

    pub fn from_pem(cert_pem: &str, key_pem: &str) -> Result<Self> {
        let key = KeyPair::from_pem(key_pem)?;
        let der = pem_to_der(cert_pem)?;
        Ok(Self {
            cert_pem: cert_pem.to_owned(),
            cert_der: der,
            key,
        })
    }

    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    pub fn cert_der(&self) -> &[u8] {
        &self.cert_der
    }

    pub fn key_pem(&self) -> String {
        self.key.serialize_pem()
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(&self.cert_der)
    }

    /// A revocation list naming `revoked` (serial in hex, revoked at as a
    /// Unix time), signed by this CA, valid for `valid_for`. `number`
    /// orders lists: a later one has a larger number.
    pub fn crl(
        &self,
        number: u64,
        revoked: &[(String, i64)],
        valid_for: Duration,
    ) -> Result<String> {
        let now = OffsetDateTime::now_utc();
        let revoked_certs = revoked
            .iter()
            .map(|(serial, at)| {
                Ok(RevokedCertParams {
                    serial_number: SerialNumber::from(
                        hex::decode(serial)
                            .map_err(|e| Error::InvalidCertificate(e.to_string()))?,
                    ),
                    revocation_time: OffsetDateTime::from_unix_timestamp(*at)
                        .map_err(|e| Error::InvalidCertificate(e.to_string()))?,
                    reason_code: None,
                    invalidity_date: None,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let params = CertificateRevocationListParams {
            this_update: now - time::Duration::minutes(5),
            next_update: now + valid_for,
            crl_number: SerialNumber::from(number.to_be_bytes().to_vec()),
            issuing_distribution_point: None,
            revoked_certs,
            key_identifier_method: KeyIdMethod::Sha256,
        };
        Ok(params.signed_by(&self.issuer()?)?.pem()?)
    }

    fn issuer(&self) -> Result<Issuer<'_, &KeyPair>> {
        Ok(Issuer::from_ca_cert_der(
            &self.cert_der.as_slice().into(),
            &self.key,
        )?)
    }

    /// Issues a client certificate (and a fresh key) with this common name.
    /// Returns `(cert_pem, key_pem)`.
    pub fn issue_client(&self, common_name: &str, lifetime: Duration) -> Result<(String, String)> {
        let key = KeyPair::generate()?;
        let mut params = CertificateParams::default();
        params.distinguished_name = dn(common_name);
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.serial_number = Some(serial());
        validity(&mut params, lifetime);
        let cert = params.signed_by(&key, &self.issuer()?)?;
        Ok((cert.pem(), key.serialize_pem()))
    }

    /// Issues a server certificate (and a fresh key) for the given DNS names
    /// and IP addresses. Returns `(cert_pem, key_pem)`.
    pub fn issue_server(&self, sans: &[String], lifetime: Duration) -> Result<(String, String)> {
        let key = KeyPair::generate()?;
        let mut params = CertificateParams::new(sans.to_vec())?;
        params.distinguished_name = dn(sans.first().map_or("mst-controller", String::as_str));
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.serial_number = Some(serial());
        validity(&mut params, lifetime);
        let cert = params.signed_by(&key, &self.issuer()?)?;
        Ok((cert.pem(), key.serialize_pem()))
    }

    /// Issues a certificate for a server that also connects to its peers
    /// with it (clustered OVN databases): valid for server and client auth.
    /// Returns `(cert_pem, key_pem)`.
    pub fn issue_peer(&self, sans: &[String], lifetime: Duration) -> Result<(String, String)> {
        let key = KeyPair::generate()?;
        let mut params = CertificateParams::new(sans.to_vec())?;
        params.distinguished_name = dn(sans.first().map_or("mst-controller", String::as_str));
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ];
        params.serial_number = Some(serial());
        validity(&mut params, lifetime);
        let cert = params.signed_by(&key, &self.issuer()?)?;
        Ok((cert.pem(), key.serialize_pem()))
    }

    /// Signs a host certificate for the public key in `csr_pem`. Only the key
    /// is taken from the CSR: subject and SANs are decided by the controller,
    /// so an agent cannot request an identity it was not given. Host
    /// certificates are valid for both client and server auth: hosts serve
    /// QEMU's TLS with them too.
    pub fn sign_host_csr(
        &self,
        csr_pem: &str,
        host_id: &str,
        sans: &[String],
        lifetime: Duration,
    ) -> Result<String> {
        let csr = CertificateSigningRequestParams::from_pem(csr_pem)?;
        let mut params = CertificateParams::new(sans.to_vec())?;
        params.distinguished_name = dn(host_id);
        params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyEncipherment,
        ];
        params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ClientAuth,
            ExtendedKeyUsagePurpose::ServerAuth,
        ];
        params.serial_number = Some(serial());
        validity(&mut params, lifetime);
        let signed = CertificateSigningRequestParams {
            params,
            public_key: csr.public_key,
        };
        Ok(signed.signed_by(&self.issuer()?)?.pem())
    }
}

/// Generates a key pair and a CSR for it. Returns `(key_pem, csr_pem)`.
pub fn new_key_and_csr(common_name: &str) -> Result<(String, String)> {
    let key = KeyPair::generate()?;
    let mut params = CertificateParams::default();
    params.distinguished_name = dn(common_name);
    let csr = params.serialize_request(&key)?;
    Ok((key.serialize_pem(), csr.pem()?))
}

/// Decodes the first certificate in a PEM string.
pub fn pem_to_der(pem: &str) -> Result<Vec<u8>> {
    let (_, parsed) = x509_parser::pem::parse_x509_pem(pem.as_bytes())
        .map_err(|e| Error::InvalidCertificate(e.to_string()))?;
    Ok(parsed.contents)
}

/// The subject common name of a DER certificate. The controller uses this to
/// identify hosts from their (already verified) client certificate.
pub fn common_name(cert_der: &[u8]) -> Result<String> {
    let (_, cert) = x509_parser::parse_x509_certificate(cert_der)
        .map_err(|e| Error::InvalidCertificate(e.to_string()))?;
    cert.subject()
        .iter_common_name()
        .next()
        .and_then(|cn| cn.as_str().ok())
        .map(str::to_owned)
        .ok_or_else(|| Error::InvalidCertificate("no common name".into()))
}

/// Not-after time of a DER certificate, as a Unix timestamp.
/// Credentials for one live migration's TLS: a CA made for it alone, a
/// server certificate for the destination and a client certificate for the
/// source. Each side trusts only this CA while the migration's TLS is set
/// up, so the destination accepts only that source, and the source talks
/// only to that destination. PEM throughout.
pub struct MigrationCredentials {
    pub ca_pem: String,
    pub server_cert_pem: String,
    pub server_key_pem: String,
    pub client_cert_pem: String,
    pub client_key_pem: String,
}

impl MigrationCredentials {
    /// `dest_names`: the names and addresses the source reaches the
    /// destination by (it checks the server certificate for them).
    /// `source`: names the source in its certificate.
    pub fn generate(dest_names: &[String], source: &str, lifetime: Duration) -> Result<Self> {
        let ca = Ca::generate("metastratum migration", lifetime)?;
        let (server_cert_pem, server_key_pem) = ca.issue_server(dest_names, lifetime)?;
        let (client_cert_pem, client_key_pem) = ca.issue_client(source, lifetime)?;
        Ok(Self {
            ca_pem: ca.cert_pem().to_owned(),
            server_cert_pem,
            server_key_pem,
            client_cert_pem,
            client_key_pem,
        })
    }
}

/// A certificate's serial number, as lowercase hex.
pub fn serial_of(cert_der: &[u8]) -> Result<String> {
    let (_, cert) = x509_parser::parse_x509_certificate(cert_der)
        .map_err(|e| Error::InvalidCertificate(e.to_string()))?;
    Ok(hex::encode(cert.raw_serial()))
}

pub fn not_after(cert_der: &[u8]) -> Result<i64> {
    let (_, cert) = x509_parser::parse_x509_certificate(cert_der)
        .map_err(|e| Error::InvalidCertificate(e.to_string()))?;
    Ok(cert.validity().not_after.timestamp())
}

/// Validity of a DER certificate: `(not_before, not_after)` as Unix
/// timestamps.
pub fn validity_of(cert_der: &[u8]) -> Result<(i64, i64)> {
    let (_, cert) = x509_parser::parse_x509_certificate(cert_der)
        .map_err(|e| Error::InvalidCertificate(e.to_string()))?;
    Ok((
        cert.validity().not_before.timestamp(),
        cert.validity().not_after.timestamp(),
    ))
}

/// A host join token: `mstjoin.<id>.<secret>.<ca-sha256>`.
///
/// The controller stores only the ID and a hash of the secret. The CA
/// fingerprint lets the agent authenticate the controller on first contact.
#[derive(Clone, PartialEq, Eq)]
pub struct JoinToken {
    pub id: String,
    pub secret: String,
    pub ca_fingerprint: String,
}

impl JoinToken {
    pub fn generate(ca_fingerprint: &str) -> Self {
        Self {
            id: random_hex(8),
            secret: random_hex(24),
            ca_fingerprint: ca_fingerprint.into(),
        }
    }

    pub fn secret_hash(&self) -> String {
        hash_secret(&self.secret)
    }
}

impl fmt::Display for JoinToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "mstjoin.{}.{}.{}",
            self.id, self.secret, self.ca_fingerprint
        )
    }
}

// Never print the secret in debug output.
impl fmt::Debug for JoinToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JoinToken")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl FromStr for JoinToken {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let hex_of = |part: &str, len: usize| {
            (part.len() == len && part.bytes().all(|b| b.is_ascii_hexdigit()))
                .then(|| part.to_ascii_lowercase())
                .ok_or(Error::MalformedToken)
        };
        match s.trim().split('.').collect::<Vec<_>>().as_slice() {
            ["mstjoin", id, secret, fp] => Ok(Self {
                id: hex_of(id, 16)?,
                secret: hex_of(secret, 48)?,
                ca_fingerprint: hex_of(fp, 64)?,
            }),
            _ => Err(Error::MalformedToken),
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn migration_credentials_trust_only_each_other() {
        let m = MigrationCredentials::generate(
            &["10.0.0.2".into(), "host2".into()],
            "source-host",
            Duration::from_secs(3600),
        )
        .unwrap();
        let other = MigrationCredentials::generate(
            &["10.0.0.2".into()],
            "source-host",
            Duration::from_secs(3600),
        )
        .unwrap();
        let issuer = |pem: &str| {
            let der = pem_to_der(pem).unwrap();
            let (_, c) = x509_parser::parse_x509_certificate(&der).unwrap();
            c.issuer().to_string()
        };
        let subject = |pem: &str| {
            let der = pem_to_der(pem).unwrap();
            let (_, c) = x509_parser::parse_x509_certificate(&der).unwrap();
            c.subject().to_string()
        };
        assert_eq!(issuer(&m.server_cert_pem), subject(&m.ca_pem));
        assert_eq!(issuer(&m.client_cert_pem), subject(&m.ca_pem));
        assert_eq!(
            common_name(&pem_to_der(&m.client_cert_pem).unwrap()).unwrap(),
            "source-host"
        );
        // A fresh CA each time: one migration's certificates open no other.
        assert_ne!(
            fingerprint(&pem_to_der(&m.ca_pem).unwrap()),
            fingerprint(&pem_to_der(&other.ca_pem).unwrap())
        );
    }

    #[test]
    fn revocation_lists_name_revoked_serials() {
        let ca = Ca::generate("test CA", Duration::from_secs(3600)).unwrap();
        let (cert, _) = ca.issue_client("host-1", Duration::from_secs(600)).unwrap();
        let serial = serial_of(&pem_to_der(&cert).unwrap()).unwrap();
        let crl = ca
            .crl(
                7,
                &[(serial.clone(), 1_800_000_000)],
                Duration::from_secs(86400),
            )
            .unwrap();
        let der = pem_to_der(&crl).unwrap();
        let (_, parsed) = x509_parser::parse_x509_crl(&der).unwrap();
        let listed: Vec<String> = parsed
            .iter_revoked_certificates()
            .map(|r| hex::encode(r.raw_serial()))
            .collect();
        assert_eq!(listed, [serial]);
        assert_eq!(parsed.crl_number().map(|n| n.to_string()), Some("7".into()));
    }

    use super::*;

    const DAY: Duration = Duration::from_secs(86400);

    #[test]
    fn ca_signs_host_csr_with_controller_chosen_identity() {
        let ca = Ca::generate("metastratum test CA", DAY).unwrap();
        let (_key, csr) = new_key_and_csr("i-would-like-to-be-root").unwrap();
        let cert = ca
            .sign_host_csr(
                &csr,
                "host-123",
                &["mst-host1".into(), "10.77.0.11".into()],
                DAY,
            )
            .unwrap();
        let der = pem_to_der(&cert).unwrap();
        assert_eq!(common_name(&der).unwrap(), "host-123");

        let (_, parsed) = x509_parser::parse_x509_certificate(&der).unwrap();
        let (_, ca_parsed) = x509_parser::parse_x509_certificate(ca.cert_der()).unwrap();
        assert_eq!(parsed.issuer(), ca_parsed.subject());
        parsed
            .verify_signature(Some(ca_parsed.public_key()))
            .unwrap();
    }

    #[test]
    fn ca_roundtrips_through_pem() {
        let ca = Ca::generate("metastratum test CA", DAY).unwrap();
        let again = Ca::from_pem(ca.cert_pem(), &ca.key_pem()).unwrap();
        assert_eq!(again.fingerprint(), ca.fingerprint());
        again
            .issue_server(&["localhost".into(), "127.0.0.1".into()], DAY)
            .unwrap();
    }

    #[test]
    fn join_token_roundtrip_and_validation() {
        let t = JoinToken::generate(&"ab".repeat(32));
        let parsed: JoinToken = t.to_string().parse().unwrap();
        assert!(parsed == t);
        assert!(!format!("{t:?}").contains(&t.secret));
        assert!("mstjoin.x.y.z".parse::<JoinToken>().is_err());
        assert!(
            t.to_string()
                .replace("mstjoin", "nope")
                .parse::<JoinToken>()
                .is_err()
        );
    }
}
