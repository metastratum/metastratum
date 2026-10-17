//! WebAuthn security keys and passkeys as a second factor. FIDO U2F keys
//! take part the same way: browsers speak to them over WebAuthn (the old
//! U2F browser API is gone), they just cannot verify their user, which a
//! second factor does not ask for.
//!
//! Attestation is not asked for (`none`), so any authenticator will do:
//! what counts is that the key that signed up is the key that signs in.
//! Keys are ES256, EdDSA or RS256, verified with ring.

use anyhow::{Context, bail, ensure};
use base64::Engine;
use base64::alphabet::URL_SAFE;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use ciborium::Value;
use ring::{digest, signature};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Base64url as WebAuthn's JSON uses it: no padding written, either read.
const B64: GeneralPurpose = GeneralPurpose::new(
    &URL_SAFE,
    GeneralPurposeConfig::new()
        .with_encode_padding(false)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

pub fn b64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

pub fn unb64(s: &str) -> anyhow::Result<Vec<u8>> {
    B64.decode(s).context("not base64url")
}

/// How long the browser lets the user find and touch the key.
const TIMEOUT_MS: u64 = 120_000;

/// The relying party: this site, as the browser sees it.
pub struct Rp {
    /// The host name credentials are bound to (`mst.example.com`).
    pub id: String,
    /// `scheme://host[:port]`, which the browser reports signing for.
    pub origin: String,
}

/// A registered credential, as kept with the user's factor.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Credential {
    /// base64url
    pub credential_id: String,
    /// The COSE public key, base64url.
    pub public_key: String,
    pub rp_id: String,
    pub sign_count: u32,
    #[serde(default)]
    pub transports: Vec<String>,
}

impl Credential {
    fn descriptor(&self) -> serde_json::Value {
        let mut d = json!({"type": "public-key", "id": self.credential_id});
        if !self.transports.is_empty() {
            d["transports"] = json!(self.transports);
        }
        d
    }
}

/// A fresh challenge.
pub fn challenge() -> Vec<u8> {
    rand::random::<[u8; 32]>().to_vec()
}

/// Options for `navigator.credentials.create()`, in WebAuthn's JSON form.
pub fn creation_options(
    rp: &Rp,
    rp_name: &str,
    user_handle: &[u8],
    user_name: &str,
    display_name: &str,
    challenge: &[u8],
    existing: &[Credential],
) -> serde_json::Value {
    json!({
        "rp": {"id": rp.id, "name": rp_name},
        "user": {"id": b64(user_handle), "name": user_name, "displayName": display_name},
        "challenge": b64(challenge),
        "pubKeyCredParams": [
            {"type": "public-key", "alg": -7},
            {"type": "public-key", "alg": -8},
            {"type": "public-key", "alg": -257},
        ],
        "timeout": TIMEOUT_MS,
        "excludeCredentials": existing.iter().map(Credential::descriptor).collect::<Vec<_>>(),
        // A second factor: no PIN or fingerprint, and nothing stored on the
        // key, which U2F keys cannot do anyway.
        "authenticatorSelection": {
            "residentKey": "discouraged",
            "requireResidentKey": false,
            "userVerification": "discouraged",
        },
        "attestation": "none",
    })
}

/// Options for `navigator.credentials.get()`, in WebAuthn's JSON form.
pub fn request_options(rp_id: &str, challenge: &[u8], allow: &[Credential]) -> serde_json::Value {
    json!({
        "challenge": b64(challenge),
        "timeout": TIMEOUT_MS,
        "rpId": rp_id,
        "allowCredentials": allow.iter().map(Credential::descriptor).collect::<Vec<_>>(),
        "userVerification": "discouraged",
    })
}

/// What `create()` resolves to, as `PublicKeyCredential.toJSON()` gives it.
#[derive(Deserialize, utoipa::ToSchema)]
pub struct RegistrationResponse {
    /// base64url
    #[serde(rename = "rawId")]
    pub raw_id: String,
    pub response: AttestationResponse,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct AttestationResponse {
    #[serde(rename = "clientDataJSON")]
    pub client_data_json: String,
    #[serde(rename = "attestationObject")]
    pub attestation_object: String,
    #[serde(default)]
    pub transports: Vec<String>,
}

/// What `get()` resolves to, as `PublicKeyCredential.toJSON()` gives it.
#[derive(Deserialize, utoipa::ToSchema)]
pub struct AuthenticationResponse {
    /// base64url
    #[serde(rename = "rawId")]
    pub raw_id: String,
    pub response: AssertionResponse,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct AssertionResponse {
    #[serde(rename = "clientDataJSON")]
    pub client_data_json: String,
    #[serde(rename = "authenticatorData")]
    pub authenticator_data: String,
    pub signature: String,
}

#[derive(Deserialize)]
struct ClientData {
    #[serde(rename = "type")]
    ty: String,
    challenge: String,
    origin: String,
    #[serde(rename = "crossOrigin", default)]
    cross_origin: bool,
}

fn check_client_data(raw: &[u8], ty: &str, challenge: &[u8], origin: &str) -> anyhow::Result<()> {
    let cd: ClientData = serde_json::from_slice(raw).context("reading clientDataJSON")?;
    ensure!(cd.ty == ty, "the browser answered a {}, not a {ty}", cd.ty);
    ensure!(
        super::same(&unb64(&cd.challenge)?, challenge),
        "the answer is to another challenge"
    );
    ensure!(
        cd.origin == origin,
        "the browser signed for {}, not {origin}",
        cd.origin
    );
    ensure!(
        !cd.cross_origin,
        "the request came from another site's frame"
    );
    Ok(())
}

const USER_PRESENT: u8 = 0x01;
const ATTESTED: u8 = 0x40;

struct AuthData<'a> {
    rp_id_hash: &'a [u8],
    flags: u8,
    sign_count: u32,
    /// The credential ID and COSE public key, at registration.
    attested: Option<(&'a [u8], &'a [u8])>,
}

fn parse_auth_data(data: &[u8]) -> anyhow::Result<AuthData<'_>> {
    ensure!(data.len() >= 37, "authenticator data is too short");
    let flags = data[32];
    let sign_count = u32::from_be_bytes(data[33..37].try_into()?);
    let attested = if flags & ATTESTED != 0 {
        let rest = &data[37..];
        ensure!(rest.len() >= 18, "attested credential data is too short");
        let len = u16::from_be_bytes([rest[16], rest[17]]) as usize;
        ensure!(
            len <= 1023 && rest.len() >= 18 + len,
            "bad credential ID length"
        );
        let id = &rest[18..18 + len];
        let key_start = &rest[18 + len..];
        let mut after = key_start;
        let _: Value = ciborium::from_reader(&mut after).context("reading the public key")?;
        Some((id, &key_start[..key_start.len() - after.len()]))
    } else {
        None
    };
    Ok(AuthData {
        rp_id_hash: &data[..32],
        flags,
        sign_count,
        attested,
    })
}

enum PublicKey {
    Es256(Vec<u8>),
    Ed25519(Vec<u8>),
    Rs256 { n: Vec<u8>, e: Vec<u8> },
}

fn parse_cose(bytes: &[u8]) -> anyhow::Result<PublicKey> {
    let Value::Map(map) = ciborium::from_reader::<Value, _>(bytes).context("reading COSE key")?
    else {
        bail!("the COSE key is not a map");
    };
    let get = |label: i64| {
        map.iter().find_map(|(k, v)| match k {
            Value::Integer(i) if i128::from(*i) == i128::from(label) => Some(v),
            _ => None,
        })
    };
    let int = |label| match get(label) {
        Some(Value::Integer(i)) => Some(i128::from(*i)),
        _ => None,
    };
    let bytes = |label| match get(label) {
        Some(Value::Bytes(b)) => Some(b.clone()),
        _ => None,
    };
    // kty 1, alg 3; then per type: crv -1, x -2, y -3 (EC2), or n -1, e -2.
    match (int(1), int(3)) {
        (Some(2), Some(-7)) => {
            ensure!(int(-1) == Some(1), "ES256 keys are on P-256");
            let (x, y) = (bytes(-2).context("no x")?, bytes(-3).context("no y")?);
            ensure!(x.len() == 32 && y.len() == 32, "bad P-256 point");
            Ok(PublicKey::Es256([&[4u8][..], &x, &y].concat()))
        }
        (Some(1), Some(-8)) => {
            ensure!(int(-1) == Some(6), "EdDSA keys are Ed25519");
            let x = bytes(-2).context("no x")?;
            ensure!(x.len() == 32, "bad Ed25519 key");
            Ok(PublicKey::Ed25519(x))
        }
        (Some(3), Some(-257)) => Ok(PublicKey::Rs256 {
            n: bytes(-1).context("no n")?,
            e: bytes(-2).context("no e")?,
        }),
        (kty, alg) => bail!("unsupported key (type {kty:?}, algorithm {alg:?})"),
    }
}

fn verify_signature(key: &PublicKey, message: &[u8], sig: &[u8]) -> anyhow::Result<()> {
    let ok = match key {
        PublicKey::Es256(point) => {
            signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_ASN1, point)
                .verify(message, sig)
        }
        PublicKey::Ed25519(k) => {
            signature::UnparsedPublicKey::new(&signature::ED25519, k).verify(message, sig)
        }
        PublicKey::Rs256 { n, e } => signature::RsaPublicKeyComponents { n, e }.verify(
            &signature::RSA_PKCS1_2048_8192_SHA256,
            message,
            sig,
        ),
    };
    ok.map_err(|_| anyhow::anyhow!("the signature does not match"))
}

fn rp_id_hash(rp_id: &str) -> digest::Digest {
    digest::digest(&digest::SHA256, rp_id.as_bytes())
}

/// Checks a new credential and returns what to keep of it.
pub fn register(
    rp: &Rp,
    challenge: &[u8],
    resp: &RegistrationResponse,
) -> anyhow::Result<Credential> {
    check_client_data(
        &unb64(&resp.response.client_data_json)?,
        "webauthn.create",
        challenge,
        &rp.origin,
    )?;
    let Value::Map(att) =
        ciborium::from_reader::<Value, _>(&unb64(&resp.response.attestation_object)?[..])
            .context("reading the attestation object")?
    else {
        bail!("the attestation object is not a map");
    };
    let auth_data = att
        .iter()
        .find_map(|(k, v)| match (k, v) {
            (Value::Text(k), Value::Bytes(b)) if k == "authData" => Some(b),
            _ => None,
        })
        .context("no authenticator data")?;
    let ad = parse_auth_data(auth_data)?;
    ensure!(
        ad.rp_id_hash == rp_id_hash(&rp.id).as_ref(),
        "the key signed up for another site"
    );
    ensure!(ad.flags & USER_PRESENT != 0, "the key was not touched");
    let (id, key) = ad.attested.context("no credential in the answer")?;
    ensure!(
        id == unb64(&resp.raw_id)?.as_slice(),
        "the credential ID does not match"
    );
    parse_cose(key)?;
    Ok(Credential {
        credential_id: b64(id),
        public_key: b64(key),
        rp_id: rp.id.clone(),
        sign_count: ad.sign_count,
        transports: resp
            .response
            .transports
            .iter()
            .filter(|t| t.len() <= 16)
            .take(8)
            .cloned()
            .collect(),
    })
}

/// Checks a sign-in with one of the user's credentials (the one whose ID
/// the answer carries; the caller finds it). Returns the new signature
/// counter.
pub fn authenticate(
    rp: &Rp,
    challenge: &[u8],
    cred: &Credential,
    resp: &AuthenticationResponse,
) -> anyhow::Result<u32> {
    let client_data = unb64(&resp.response.client_data_json)?;
    check_client_data(&client_data, "webauthn.get", challenge, &rp.origin)?;
    ensure!(
        cred.rp_id == rp.id,
        "the key is registered for another site"
    );
    let auth_data = unb64(&resp.response.authenticator_data)?;
    let ad = parse_auth_data(&auth_data)?;
    ensure!(
        ad.rp_id_hash == rp_id_hash(&rp.id).as_ref(),
        "the key signed for another site"
    );
    ensure!(ad.flags & USER_PRESENT != 0, "the key was not touched");
    let key = parse_cose(&unb64(&cred.public_key)?)?;
    let message = [
        &auth_data[..],
        digest::digest(&digest::SHA256, &client_data).as_ref(),
    ]
    .concat();
    verify_signature(&key, &message, &unb64(&resp.response.signature)?)?;
    // Keys that count signatures must count up; one that goes back may
    // have been cloned.
    if (ad.sign_count != 0 || cred.sign_count != 0) && ad.sign_count <= cred.sign_count {
        bail!("the key's signature counter went back; it may have been cloned");
    }
    Ok(ad.sign_count)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ring::rand::SystemRandom;
    use ring::signature::{EcdsaKeyPair, Ed25519KeyPair, KeyPair};

    pub enum Key {
        P256(EcdsaKeyPair),
        Ed(Ed25519KeyPair),
    }

    /// A software authenticator.
    pub struct Soft {
        pub key: Key,
        pub id: Vec<u8>,
        pub count: u32,
    }

    impl Soft {
        pub fn p256() -> Self {
            let rng = SystemRandom::new();
            let alg = &signature::ECDSA_P256_SHA256_ASN1_SIGNING;
            let pkcs8 = EcdsaKeyPair::generate_pkcs8(alg, &rng).unwrap();
            Self {
                key: Key::P256(EcdsaKeyPair::from_pkcs8(alg, pkcs8.as_ref(), &rng).unwrap()),
                id: rand::random::<[u8; 16]>().to_vec(),
                count: 0,
            }
        }

        pub fn ed25519() -> Self {
            let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
            Self {
                key: Key::Ed(Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap()),
                id: rand::random::<[u8; 16]>().to_vec(),
                count: 0,
            }
        }

        fn cose(&self) -> Vec<u8> {
            let i = |n: i64| Value::Integer(n.into());
            let map = match &self.key {
                Key::P256(k) => {
                    let p = k.public_key().as_ref();
                    vec![
                        (i(1), i(2)),
                        (i(3), i(-7)),
                        (i(-1), i(1)),
                        (i(-2), Value::Bytes(p[1..33].to_vec())),
                        (i(-3), Value::Bytes(p[33..].to_vec())),
                    ]
                }
                Key::Ed(k) => vec![
                    (i(1), i(1)),
                    (i(3), i(-8)),
                    (i(-1), i(6)),
                    (i(-2), Value::Bytes(k.public_key().as_ref().to_vec())),
                ],
            };
            let mut out = vec![];
            ciborium::into_writer(&Value::Map(map), &mut out).unwrap();
            out
        }

        fn client_data(ty: &str, challenge: &[u8], origin: &str) -> String {
            b64(
                json!({"type": ty, "challenge": b64(challenge), "origin": origin})
                    .to_string()
                    .as_bytes(),
            )
        }

        pub fn create(&self, rp_id: &str, origin: &str, challenge: &[u8]) -> RegistrationResponse {
            let mut ad = rp_id_hash(rp_id).as_ref().to_vec();
            ad.push(USER_PRESENT | ATTESTED);
            ad.extend(self.count.to_be_bytes());
            ad.extend([0u8; 16]);
            ad.extend((self.id.len() as u16).to_be_bytes());
            ad.extend(&self.id);
            ad.extend(self.cose());
            let att = Value::Map(vec![
                (Value::Text("fmt".into()), Value::Text("none".into())),
                (Value::Text("attStmt".into()), Value::Map(vec![])),
                (Value::Text("authData".into()), Value::Bytes(ad)),
            ]);
            let mut att_bytes = vec![];
            ciborium::into_writer(&att, &mut att_bytes).unwrap();
            RegistrationResponse {
                raw_id: b64(&self.id),
                response: AttestationResponse {
                    client_data_json: Self::client_data("webauthn.create", challenge, origin),
                    attestation_object: b64(&att_bytes),
                    transports: vec!["usb".into()],
                },
            }
        }

        pub fn get(
            &mut self,
            rp_id: &str,
            origin: &str,
            challenge: &[u8],
        ) -> AuthenticationResponse {
            self.count += 1;
            let mut ad = rp_id_hash(rp_id).as_ref().to_vec();
            ad.push(USER_PRESENT);
            ad.extend(self.count.to_be_bytes());
            let cd = Self::client_data("webauthn.get", challenge, origin);
            let msg = [
                &ad[..],
                digest::digest(&digest::SHA256, &unb64(&cd).unwrap()).as_ref(),
            ]
            .concat();
            let sig = match &self.key {
                Key::P256(k) => k
                    .sign(&SystemRandom::new(), &msg)
                    .unwrap()
                    .as_ref()
                    .to_vec(),
                Key::Ed(k) => k.sign(&msg).as_ref().to_vec(),
            };
            AuthenticationResponse {
                raw_id: b64(&self.id),
                response: AssertionResponse {
                    client_data_json: cd,
                    authenticator_data: b64(&ad),
                    signature: b64(&sig),
                },
            }
        }
    }

    fn rp() -> Rp {
        Rp {
            id: "mst.example.com".into(),
            origin: "https://mst.example.com".into(),
        }
    }

    #[test]
    fn registers_and_signs_in() {
        for mut key in [Soft::p256(), Soft::ed25519()] {
            let rp = rp();
            let c = challenge();
            let cred = register(&rp, &c, &key.create(&rp.id, &rp.origin, &c)).unwrap();
            assert_eq!(cred.credential_id, b64(&key.id));
            assert_eq!(cred.transports, ["usb"]);
            let c = challenge();
            let count = authenticate(&rp, &c, &cred, &key.get(&rp.id, &rp.origin, &c)).unwrap();
            assert_eq!(count, 1);
        }
    }

    #[test]
    fn refuses_what_does_not_add_up() {
        let rp = rp();
        let mut key = Soft::p256();
        let c = challenge();
        // Another challenge, origin or site.
        assert!(register(&rp, &challenge(), &key.create(&rp.id, &rp.origin, &c)).is_err());
        assert!(register(&rp, &c, &key.create(&rp.id, "https://evil.example", &c)).is_err());
        assert!(register(&rp, &c, &key.create("evil.example", &rp.origin, &c)).is_err());
        let cred = register(&rp, &c, &key.create(&rp.id, &rp.origin, &c)).unwrap();

        let c = challenge();
        let resp = key.get(&rp.id, &rp.origin, &c);
        assert!(authenticate(&rp, &challenge(), &cred, &resp).is_err());
        // Another key's signature.
        let mut other = Soft::p256();
        other.id = key.id.clone();
        let forged = other.get(&rp.id, &rp.origin, &c);
        assert!(authenticate(&rp, &c, &cred, &forged).is_err());
        // A tampered signature.
        let mut bad = key.get(&rp.id, &rp.origin, &c);
        let mut sig = unb64(&bad.response.signature).unwrap();
        let last = sig.len() - 1;
        sig[last] ^= 1;
        bad.response.signature = b64(&sig);
        assert!(authenticate(&rp, &c, &cred, &bad).is_err());
        // A counter that does not go up.
        let mut seen = cred.clone();
        seen.sign_count = 10;
        assert!(authenticate(&rp, &c, &seen, &key.get(&rp.id, &rp.origin, &c)).is_err());
        // A credential kept for another site.
        let mut elsewhere = cred.clone();
        elsewhere.rp_id = "other.example.com".into();
        assert!(authenticate(&rp, &c, &elsewhere, &key.get(&rp.id, &rp.origin, &c)).is_err());
    }
}
