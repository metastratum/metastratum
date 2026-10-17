//! Two-factor sign-in: authenticator apps, security keys (with a software
//! authenticator, at `localhost`, where browsers allow them without HTTPS),
//! backup codes, a tenant requiring a second factor, and administrators
//! resetting them.
//!
//! YubiKey OTPs are tested when MST_E2E_YUBICO names an address
//! (`127.0.0.1:5557`) for a stand-in validation service, which this test
//! runs; the controller's configuration must point at it:
//!
//! ```toml
//! [yubico]
//! client_id = "e2e"
//! secret_key = "bWV0YXN0cmF0dW0tZTJlLXl1YmljbyE="
//! servers = ["http://127.0.0.1:5557/wsapi/2.0/verify"]
//! ```

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ciborium::Value as Cbor;
use mst_e2e::Api;
use reqwest::Method;
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};
use ring::{digest, hmac};
use serde_json::{Value, json};

const PASSWORD: &str = "e2e-password-1";
const YUBICO_KEY: &str = "bWV0YXN0cmF0dW0tZTJlLXl1YmljbyE=";

fn b64(b: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(b)
}

fn unb64(s: &str) -> Vec<u8> {
    URL_SAFE_NO_PAD.decode(s.trim_end_matches('=')).unwrap()
}

fn now_step() -> u64 {
    chrono::Utc::now().timestamp() as u64 / 30
}

/// RFC 6238, as authenticator apps do it.
fn totp(secret_b32: &str, step: u64) -> String {
    let mut bytes = vec![];
    let (mut buf, mut bits) = (0u32, 0);
    for c in secret_b32.bytes() {
        let v = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567"
            .iter()
            .position(|&a| a == c)
            .unwrap() as u32;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            bytes.push((buf >> bits) as u8);
        }
    }
    let key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, &bytes);
    let mac = hmac::sign(&key, &step.to_be_bytes());
    let m = mac.as_ref();
    let o = (m[19] & 0x0f) as usize;
    let n = u32::from_be_bytes([m[o], m[o + 1], m[o + 2], m[o + 3]]) & 0x7fff_ffff;
    format!("{:06}", n % 1_000_000)
}

/// A software security key (P-256), as a browser would present it.
struct SoftKey {
    key: EcdsaKeyPair,
    id: Vec<u8>,
    count: u32,
}

impl SoftKey {
    fn new() -> Self {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
        Self {
            key: EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng)
                .unwrap(),
            id: rand_bytes(16),
            count: 0,
        }
    }

    fn client_data(ty: &str, challenge: &str, origin: &str) -> String {
        b64(
            json!({"type": ty, "challenge": challenge, "origin": origin})
                .to_string()
                .as_bytes(),
        )
    }

    fn create(&self, options: &Value, origin: &str) -> Value {
        let rp_id = options["rp"]["id"].as_str().unwrap();
        let p = self.key.public_key().as_ref();
        let i = |n: i64| Cbor::Integer(n.into());
        let cose = Cbor::Map(vec![
            (i(1), i(2)),
            (i(3), i(-7)),
            (i(-1), i(1)),
            (i(-2), Cbor::Bytes(p[1..33].to_vec())),
            (i(-3), Cbor::Bytes(p[33..].to_vec())),
        ]);
        let mut ad = digest::digest(&digest::SHA256, rp_id.as_bytes())
            .as_ref()
            .to_vec();
        ad.push(0x41); // user present, credential attached
        ad.extend(self.count.to_be_bytes());
        ad.extend([0u8; 16]);
        ad.extend((self.id.len() as u16).to_be_bytes());
        ad.extend(&self.id);
        ciborium::into_writer(&cose, &mut ad).unwrap();
        let att = Cbor::Map(vec![
            (Cbor::Text("fmt".into()), Cbor::Text("none".into())),
            (Cbor::Text("attStmt".into()), Cbor::Map(vec![])),
            (Cbor::Text("authData".into()), Cbor::Bytes(ad)),
        ]);
        let mut att_bytes = vec![];
        ciborium::into_writer(&att, &mut att_bytes).unwrap();
        let challenge = options["challenge"].as_str().unwrap();
        json!({
            "id": b64(&self.id), "rawId": b64(&self.id), "type": "public-key",
            "response": {
                "clientDataJSON": Self::client_data("webauthn.create", challenge, origin),
                "attestationObject": b64(&att_bytes),
                "transports": ["usb"],
            },
        })
    }

    fn get(&mut self, options: &Value, origin: &str) -> Value {
        self.count += 1;
        let rp_id = options["rpId"].as_str().unwrap();
        let mut ad = digest::digest(&digest::SHA256, rp_id.as_bytes())
            .as_ref()
            .to_vec();
        ad.push(0x01);
        ad.extend(self.count.to_be_bytes());
        let cd = Self::client_data(
            "webauthn.get",
            options["challenge"].as_str().unwrap(),
            origin,
        );
        let msg = [
            &ad[..],
            digest::digest(&digest::SHA256, &unb64(&cd)).as_ref(),
        ]
        .concat();
        let sig = self.key.sign(&SystemRandom::new(), &msg).unwrap();
        json!({
            "id": b64(&self.id), "rawId": b64(&self.id), "type": "public-key",
            "response": {
                "clientDataJSON": cd,
                "authenticatorData": b64(&ad),
                "signature": b64(sig.as_ref()),
            },
        })
    }
}

fn rand_bytes(n: usize) -> Vec<u8> {
    let mut v = vec![0u8; n];
    ring::rand::SecureRandom::fill(&SystemRandom::new(), &mut v).unwrap();
    v
}

/// The password step. Returns the status and body: 204 when signed in,
/// 200 with the second factor's challenge.
async fn password_step(api: &Api, user: &str, run: &str) -> anyhow::Result<(u16, Value)> {
    let body = json!({"username": user, "password": PASSWORD, "tenant": run});
    api.try_send(Method::POST, "/auth/login", Some(body)).await
}

async fn second_step(api: &Api, token: &Value, answer: Value) -> anyhow::Result<(u16, Value)> {
    let mut body = answer;
    body["token"] = token.clone();
    api.try_send(Method::POST, "/auth/login/second-factor", Some(body))
        .await
}

/// Signs in with a code as the second factor; returns the second step's
/// status.
async fn sign_in_with_code(api: &Api, user: &str, run: &str, code: &str) -> anyhow::Result<u16> {
    let (st, c) = password_step(api, user, run).await?;
    assert_eq!(st, 200, "{user} has a second factor: {c}");
    let (st, _) = second_step(api, &c["second_factor"]["token"], json!({"code": code})).await?;
    Ok(st)
}

#[tokio::test]
async fn second_factors() -> anyhow::Result<()> {
    let Some(admin) = mst_e2e::connect().await? else {
        return Ok(());
    };
    let run = mst_e2e::run_name("two-factor");
    let root = admin.get("/auth/me").await?["tenant_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let tenant = admin
        .post(
            "/tenants",
            json!({"parent_id": root, "slug": run, "name": run}),
        )
        .await?["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let result = tokio::spawn({
        let (admin, run, tenant) = (admin.clone(), run.clone(), tenant.clone());
        async move { scenario(&admin, &run, &tenant).await }
    })
    .await;

    let (st, body) = admin
        .try_send(Method::DELETE, &format!("/tenants/{tenant}"), None)
        .await?;
    assert_eq!(st, 204, "deleting tenant: {body}");
    match result {
        Ok(r) => r,
        Err(panic) => std::panic::resume_unwind(panic.into_panic()),
    }
}

async fn scenario(admin: &Api, run: &str, tenant: &str) -> anyhow::Result<()> {
    let mut ids = BTreeMap::new();
    for name in ["alice", "bob", "carol"] {
        let u = admin
            .post(
                "/users",
                json!({"tenant_id": tenant, "username": name, "display_name": name,
                       "password": PASSWORD, "role": "operator"}),
            )
            .await?;
        ids.insert(name, u["id"].as_str().unwrap().to_owned());
    }
    // Roles only at a user's own tenant or below: whoever manages the
    // account (resetting its password, its second factors) manages there.
    let root = admin.get("/auth/me").await?["tenant_id"].clone();
    let (st, body) = admin
        .try_send(
            Method::POST,
            "/role-bindings",
            Some(json!({"user_id": ids["alice"], "tenant_id": root, "role": "admin"})),
        )
        .await?;
    assert_eq!(st, 400, "a role above the user's tenant: {body}");
    let tf = |name: &str| format!("/users/{}/two-factor", ids[name]);
    // Security keys need a name the browser accepts: localhost will do.
    let local = || Api::anonymous_at(&admin.base, "localhost");
    let by_ip = || Api::anonymous_at(&admin.base, "127.0.0.1");

    // --- Alice: an authenticator app.
    let alice = local()?;
    assert_eq!(password_step(&alice, "alice", run).await?.0, 204);
    let me = alice.get("/auth/me").await?;
    assert_eq!(
        (
            me["two_factor"].as_bool(),
            me["two_factor_needed"].as_bool()
        ),
        (Some(false), Some(false))
    );
    let status = alice.get(&tf("alice")).await?;
    assert_eq!(status["factors"], json!([]), "{status}");
    let (st, body) = alice
        .try_send(
            Method::POST,
            &format!("{}/totp/start", tf("alice")),
            Some(json!({"password": "wrong-password"})),
        )
        .await?;
    assert_eq!(
        (st, body["error"].as_str()),
        (403, Some("wrong_password")),
        "{body}"
    );
    // A session opened with the password alone, by someone who learnt it,
    // ends once a factor is added.
    let eavesdropper = by_ip()?;
    assert_eq!(password_step(&eavesdropper, "alice", run).await?.0, 204);
    let e = alice
        .post(
            &format!("{}/totp/start", tf("alice")),
            json!({"password": PASSWORD}),
        )
        .await?;
    let secret = e["secret"].as_str().unwrap().to_owned();
    assert!(
        e["uri"]
            .as_str()
            .unwrap()
            .starts_with("otpauth://totp/localhost:alice%40"),
        "{e}"
    );
    let (st, _) = alice
        .try_send(Method::POST, &format!("{}/totp", tf("alice")),
            Some(json!({"token": e["token"], "code": totp(&secret, now_step() + 7), "name": "Phone"})))
        .await?;
    assert_eq!(st, 400, "a wrong first code");
    let step = now_step();
    let added = alice
        .post(
            &format!("{}/totp", tf("alice")),
            json!({"token": e["token"], "code": totp(&secret, step), "name": "Phone"}),
        )
        .await?;
    let codes: Vec<String> = serde_json::from_value(added["backup_codes"].clone())?;
    assert_eq!(codes.len(), 10, "{added}");
    assert_eq!(added["factor"]["kind"], "totp");
    assert_eq!(eavesdropper.status_of("/auth/me").await?, 401, "signed out");
    assert_eq!(
        alice.status_of("/auth/me").await?,
        200,
        "the session adding it stays"
    );
    // Others cannot add factors for her.
    let (st, _) = admin
        .try_send(
            Method::POST,
            &format!("{}/totp/start", tf("alice")),
            Some(json!({"password": PASSWORD})),
        )
        .await?;
    assert_eq!(st, 403);

    // Signing in now takes a second step.
    let api = by_ip()?;
    let (st, c) = password_step(&api, "alice", run).await?;
    assert_eq!(st, 200, "{c}");
    let methods: HashSet<_> = c["second_factor"]["methods"]
        .as_array()
        .unwrap()
        .iter()
        .cloned()
        .collect();
    assert_eq!(
        methods,
        HashSet::from([json!("totp"), json!("backup_code")]),
        "{c}"
    );
    assert_eq!(
        api.status_of("/auth/me").await?,
        401,
        "half signed in is not signed in"
    );
    let token = &c["second_factor"]["token"];
    let (st, _) = second_step(&api, token, json!({"code": totp(&secret, step + 9)})).await?;
    assert_eq!(st, 401, "a wrong code");
    // The code used to add the app is spent; the next one works, once.
    let (st, _) = second_step(&api, token, json!({"code": totp(&secret, step)})).await?;
    assert_eq!(st, 401, "a spent code");
    let (st, _) = second_step(&api, token, json!({"code": totp(&secret, step + 1)})).await?;
    assert_eq!(st, 204);
    assert_eq!(api.get("/auth/me").await?["username"], "alice");
    let (st, _) = second_step(&api, token, json!({"code": totp(&secret, step + 1)})).await?;
    assert_eq!(st, 401, "the challenge is used up");

    // A backup code works once.
    assert_eq!(
        sign_in_with_code(&by_ip()?, "alice", run, &codes[0].to_uppercase()).await?,
        204
    );
    assert_eq!(alice.get(&tf("alice")).await?["backup_codes_left"], 9);

    // --- Alice: a security key, at localhost.
    let origin = alice.base.clone();
    let e = alice
        .post(
            &format!("{}/webauthn/start", tf("alice")),
            json!({"password": PASSWORD}),
        )
        .await?;
    assert_eq!(e["options"]["rp"]["id"], "localhost", "{e}");
    assert_eq!(
        e["options"]["authenticatorSelection"]["userVerification"],
        "discouraged"
    );
    let mut key = SoftKey::new();
    let added = alice
        .post(&format!("{}/webauthn", tf("alice")),
            json!({"token": e["token"], "credential": key.create(&e["options"], &origin), "name": "Soft key"}))
        .await?;
    assert_eq!(
        added["backup_codes"],
        Value::Null,
        "codes come with the first factor only"
    );
    assert_eq!(added["factor"]["site"], "localhost");
    // Not at an IP address: browsers would not use the key there.
    let (st, body) = by_ip()?
        .try_send(
            Method::POST,
            "/auth/login",
            Some(json!({"username": "alice", "password": PASSWORD, "tenant": run})),
        )
        .await?;
    assert_eq!(st, 200);
    assert_eq!(body["second_factor"]["webauthn"], Value::Null, "{body}");
    let api = local()?;
    let (_, c) = password_step(&api, "alice", run).await?;
    let opts = &c["second_factor"]["webauthn"];
    assert_eq!(
        opts["allowCredentials"].as_array().map(Vec::len),
        Some(1),
        "{c}"
    );
    let (st, body) = second_step(
        &api,
        &c["second_factor"]["token"],
        json!({"webauthn": key.get(opts, &origin)}),
    )
    .await?;
    assert_eq!(st, 204, "{body}");
    let status = alice.get(&tf("alice")).await?;
    assert_eq!(status["factors"].as_array().map(Vec::len), Some(2));
    assert!(status["factors"][1]["last_used_at"].is_string(), "{status}");

    // --- Bob: two apps; he removes one himself, with his password.
    let bob = by_ip()?;
    assert_eq!(password_step(&bob, "bob", run).await?.0, 204);
    let mut bob_factors = vec![];
    let mut bob_secret = String::new();
    for name in ["Phone", "Tablet"] {
        let e = bob
            .post(
                &format!("{}/totp/start", tf("bob")),
                json!({"password": PASSWORD}),
            )
            .await?;
        bob_secret = e["secret"].as_str().unwrap().to_owned();
        let added = bob
            .post(
                &format!("{}/totp", tf("bob")),
                json!({"token": e["token"], "code": totp(&bob_secret, now_step()), "name": name}),
            )
            .await?;
        bob_factors.push(added["factor"]["id"].as_str().unwrap().to_owned());
    }
    let path = format!("{}/factors/{}", tf("bob"), bob_factors[0]);
    let (st, _) = bob.try_send(Method::DELETE, &path, Some(json!({}))).await?;
    assert_eq!(st, 400, "without his password");
    let (st, body) = bob
        .try_send(Method::DELETE, &path, Some(json!({"password": PASSWORD})))
        .await?;
    assert_eq!(st, 204, "{body}");
    assert_eq!(bob.get(&tf("bob")).await?["backup_codes_left"], 10);

    // Wrong answers end the sign-in.
    let api = by_ip()?;
    let (_, c) = password_step(&api, "bob", run).await?;
    let token = &c["second_factor"]["token"];
    for _ in 0..5 {
        let (st, body) = second_step(&api, token, json!({"code": "000000"})).await?;
        assert_eq!(
            (st, body["error"].as_str()),
            (401, Some("unauthenticated")),
            "{body}"
        );
    }
    let right = json!({"code": totp(&bob_secret, now_step() + 1)});
    let (st, body) = second_step(&api, token, right).await?;
    assert_eq!(
        (st, body["error"].as_str()),
        (401, Some("sign_in_expired")),
        "{body}"
    );
    // An administrator removes the other; the backup codes go with it.
    let path = format!("{}/factors/{}", tf("bob"), bob_factors[1]);
    let (st, body) = admin.try_send(Method::DELETE, &path, None).await?;
    assert_eq!(st, 204, "{body}");
    let status = bob.get(&tf("bob")).await?;
    assert_eq!(
        (
            status["factors"].clone(),
            status["backup_codes_left"].clone()
        ),
        (json!([]), json!(0))
    );

    // --- The tenant requires a second factor: Carol has none yet.
    let policy_path = format!("/tenants/{tenant}/two-factor-policy");
    let p = admin
        .send(Method::PUT, &policy_path, Some(json!({"required": true})))
        .await?;
    assert_eq!(p.1["required"], true);
    let carol = by_ip()?;
    assert_eq!(password_step(&carol, "carol", run).await?.0, 204);
    assert_eq!(carol.get("/auth/me").await?["two_factor_needed"], true);
    let (st, body) = carol.try_send(Method::GET, "/vms", None).await?;
    assert_eq!(
        (st, body["error"].as_str()),
        (403, Some("two_factor_required")),
        "{body}"
    );
    assert_eq!(carol.status_of(&tf("alice")).await?, 403, "only her own");
    assert_eq!(carol.get(&tf("carol")).await?["required"], true);
    let e = carol
        .post(
            &format!("{}/totp/start", tf("carol")),
            json!({"password": PASSWORD}),
        )
        .await?;
    let carol_secret = e["secret"].as_str().unwrap().to_owned();
    carol
        .post(
            &format!("{}/totp", tf("carol")),
            json!({"token": e["token"], "code": totp(&carol_secret, now_step()), "name": "Phone"}),
        )
        .await?;
    assert_eq!(
        carol.status_of("/vms").await?,
        200,
        "with a factor, all is open again"
    );
    assert_eq!(carol.get("/auth/me").await?["two_factor_needed"], false);

    // A YubiKey's OTPs, against the stand-in validation service.
    if let Ok(addr) = std::env::var("MST_E2E_YUBICO") {
        let seen = Arc::new(Mutex::new(HashSet::new()));
        tokio::spawn(validation_service(addr, seen));
        let public_id = "cccjgjgkhcbb";
        let otp = || {
            let modhex = b"cbdefghijklnrtuv";
            let tail: String = rand_bytes(32)
                .iter()
                .map(|b| modhex[(b & 15) as usize] as char)
                .collect();
            format!("{public_id}{tail}")
        };
        let added = carol
            .post(
                &format!("{}/yubico-otp", tf("carol")),
                json!({"password": PASSWORD, "otp": otp(), "name": "YubiKey"}),
            )
            .await?;
        assert_eq!(added["factor"]["kind"], "yubico_otp", "{added}");
        let (_, c) = password_step(&by_ip()?, "carol", run).await?;
        let methods = c["second_factor"]["methods"].as_array().unwrap();
        assert!(methods.contains(&json!("yubico_otp")), "{c}");
        let one = otp();
        assert_eq!(sign_in_with_code(&by_ip()?, "carol", run, &one).await?, 204);
        assert_eq!(
            sign_in_with_code(&by_ip()?, "carol", run, &one).await?,
            401,
            "a replayed OTP"
        );
        let other = format!("vvvvvvvvvvvv{}", &one[12..]);
        assert_eq!(
            sign_in_with_code(&by_ip()?, "carol", run, &other).await?,
            401,
            "another key"
        );
    } else {
        eprintln!("MST_E2E_YUBICO not set; skipping Yubico OTP");
    }

    // Changing one's password takes the current one.
    let me_path = format!("/users/{}", ids["carol"]);
    let (st, _) = carol
        .try_send(
            Method::PATCH,
            &me_path,
            Some(json!({"password": "e2e-password-2"})),
        )
        .await?;
    assert_eq!(st, 400);
    let (st, _) = carol
        .try_send(
            Method::PATCH,
            &me_path,
            Some(json!({"password": PASSWORD, "current_password": PASSWORD})),
        )
        .await?;
    assert_eq!(st, 200);

    // --- An administrator resets Alice, who lost her phone and key: she is
    // signed out, and as the tenant requires one, must add a factor again.
    let users = admin.get(&format!("/users?tenant_id={tenant}")).await?;
    let flags: BTreeMap<String, bool> = users
        .as_array()
        .unwrap()
        .iter()
        .map(|u| {
            (
                u["username"].as_str().unwrap().to_owned(),
                u["two_factor"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        flags,
        BTreeMap::from([
            ("alice".into(), true),
            ("bob".into(), false),
            ("carol".into(), true)
        ])
    );

    // Resetting a password or second factors keeps the person's API tokens,
    // unless the administrator asks (someone else may have got in).
    let token = carol
        .post("/tokens", json!({"name": "script", "expires_in_days": 1}))
        .await?["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let script = Api::with_token(&admin.base, &token)?;
    let carol_path = format!("/users/{}", ids["carol"]);
    admin
        .send(
            Method::PATCH,
            &carol_path,
            Some(json!({"password": PASSWORD})),
        )
        .await?;
    assert_eq!(
        script.status_of("/auth/me").await?,
        200,
        "kept by a plain reset"
    );
    let (st, body) = admin
        .try_send(
            Method::PATCH,
            &carol_path,
            Some(json!({"password": PASSWORD, "revoke_tokens": true})),
        )
        .await?;
    assert_eq!(st, 200, "{body}");
    assert_eq!(
        script.status_of("/auth/me").await?,
        401,
        "revoked on request"
    );
    let (st, body) = admin.try_send(Method::DELETE, &tf("alice"), None).await?;
    assert_eq!(st, 204, "{body}");
    assert_eq!(alice.status_of("/auth/me").await?, 401, "signed out");
    let api = by_ip()?;
    assert_eq!(password_step(&api, "alice", run).await?.0, 204);
    assert_eq!(api.get("/auth/me").await?["two_factor_needed"], true);
    let p = admin
        .send(Method::PUT, &policy_path, Some(json!({"required": false})))
        .await?;
    assert_eq!(p.1["required"], false);
    assert_eq!(api.status_of("/vms").await?, 200);
    Ok(())
}

/// A stand-in Yubico validation service (protocol 2.0): signed answers, OK
/// for an OTP it has not seen, REPLAYED_OTP for one it has.
async fn validation_service(addr: String, seen: Arc<Mutex<HashSet<String>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let key = hmac::Key::new(
        hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY,
        &STANDARD.decode(YUBICO_KEY).unwrap(),
    );
    let sign = |params: &BTreeMap<String, String>| {
        let text: Vec<String> = params
            .iter()
            .filter(|(k, _)| *k != "h")
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        STANDARD.encode(hmac::sign(&key, text.join("&").as_bytes()))
    };
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("binding MST_E2E_YUBICO");
    loop {
        let Ok((mut conn, _)) = listener.accept().await else {
            continue;
        };
        let mut buf = vec![0u8; 8192];
        let n = conn.read(&mut buf).await.unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..n]).to_string();
        let path = req.split_whitespace().nth(1).unwrap_or("/");
        let url = reqwest::Url::parse(&format!("http://x{path}")).unwrap();
        let params: BTreeMap<String, String> = url
            .query_pairs()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        let otp = params.get("otp").cloned().unwrap_or_default();
        let status = if params.get("h") != Some(&sign(&params)) {
            "BAD_SIGNATURE"
        } else if seen.lock().unwrap().insert(otp.clone()) {
            "OK"
        } else {
            "REPLAYED_OTP"
        };
        let mut answer = BTreeMap::from([
            ("otp".to_owned(), otp),
            (
                "nonce".to_owned(),
                params.get("nonce").cloned().unwrap_or_default(),
            ),
            ("t".to_owned(), chrono::Utc::now().to_rfc3339()),
            ("status".to_owned(), status.to_owned()),
            ("sl".to_owned(), "100".to_owned()),
        ]);
        answer.insert("h".into(), sign(&answer));
        let body: String = answer.iter().map(|(k, v)| format!("{k}={v}\r\n")).collect();
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = conn.write_all(resp.as_bytes()).await;
    }
}
