//! Internal PKI for hypermon.
//!
//! The controller owns a CA. It issues its own TLS server certificate for the
//! agent endpoint and signs host certificates during enrollment. Hosts identify
//! themselves to the controller with that certificate (the host ID is the
//! subject CN), and later use it for libvirt TLS between hosts.
//!
//! An agent bootstraps trust from a join token, which carries the SHA-256
//! fingerprint of the CA certificate alongside a one-time secret, the same idea
//! as kubeadm's discovery token CA hash.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, DistinguishedName,
    DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose, SerialNumber,
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
    dn.push(DnType::OrganizationName, "hypermon");
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

    fn issuer(&self) -> Result<Issuer<'_, &KeyPair>> {
        Ok(Issuer::from_ca_cert_der(
            &self.cert_der.as_slice().into(),
            &self.key,
        )?)
    }

    /// Issues a server certificate (and a fresh key) for the given DNS names
    /// and IP addresses. Returns `(cert_pem, key_pem)`.
    pub fn issue_server(&self, sans: &[String], lifetime: Duration) -> Result<(String, String)> {
        let key = KeyPair::generate()?;
        let mut params = CertificateParams::new(sans.to_vec())?;
        params.distinguished_name = dn(sans.first().map_or("hm-controller", String::as_str));
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.serial_number = Some(serial());
        validity(&mut params, lifetime);
        let cert = params.signed_by(&key, &self.issuer()?)?;
        Ok((cert.pem(), key.serialize_pem()))
    }

    /// Signs a host certificate for the public key in `csr_pem`. Only the key
    /// is taken from the CSR: subject and SANs are decided by the controller,
    /// so an agent cannot request an identity it was not given. Host
    /// certificates are valid for both client and server auth, as libvirt TLS
    /// between hosts needs both.
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
pub fn not_after(cert_der: &[u8]) -> Result<i64> {
    let (_, cert) = x509_parser::parse_x509_certificate(cert_der)
        .map_err(|e| Error::InvalidCertificate(e.to_string()))?;
    Ok(cert.validity().not_after.timestamp())
}

/// A host join token: `hmjoin.<id>.<secret>.<ca-sha256>`.
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
            "hmjoin.{}.{}.{}",
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
            ["hmjoin", id, secret, fp] => Ok(Self {
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
    use super::*;

    const DAY: Duration = Duration::from_secs(86400);

    #[test]
    fn ca_signs_host_csr_with_controller_chosen_identity() {
        let ca = Ca::generate("hypermon test CA", DAY).unwrap();
        let (_key, csr) = new_key_and_csr("i-would-like-to-be-root").unwrap();
        let cert = ca
            .sign_host_csr(
                &csr,
                "host-123",
                &["hm-host1".into(), "10.77.0.11".into()],
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
        let ca = Ca::generate("hypermon test CA", DAY).unwrap();
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
        assert!("hmjoin.x.y.z".parse::<JoinToken>().is_err());
        assert!(
            t.to_string()
                .replace("hmjoin", "nope")
                .parse::<JoinToken>()
                .is_err()
        );
    }
}
