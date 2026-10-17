//! Yubico OTP: the one-time passwords a YubiKey types when touched, checked
//! by a validation server (YubiCloud, or one of your own) over its
//! protocol, version 2.0. Requests and responses are signed with the API
//! key that comes with the client ID.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ring::hmac;

const MODHEX: &str = "cbdefghijklnrtuv";

/// Whether text looks like a Yubico OTP: a public ID and 32 characters of
/// ciphertext, in modhex.
pub fn looks_like(otp: &str) -> bool {
    (34..=48).contains(&otp.len()) && otp.chars().all(|c| MODHEX.contains(c))
}

/// The key's public ID: what comes before the ciphertext. It is the same in
/// every OTP the key types.
pub fn public_id(otp: &str) -> &str {
    &otp[..otp.len().saturating_sub(32)]
}

pub struct Validator {
    client_id: String,
    key: hmac::Key,
    servers: Vec<String>,
    http: reqwest::Client,
}

impl Validator {
    pub fn new(cfg: &crate::config::Yubico) -> anyhow::Result<Self> {
        let key = STANDARD
            .decode(cfg.secret_key.trim())
            .context("yubico.secret_key is not base64")?;
        Ok(Self {
            client_id: cfg.client_id.clone(),
            key: hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, &key),
            servers: cfg.servers.clone(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()?,
        })
    }

    /// Asks the validation servers, in turn until one answers, whether the
    /// OTP is genuine and unused. Errors when none could say.
    pub async fn verify(&self, otp: &str) -> anyhow::Result<bool> {
        let nonce = mst_pki::random_hex(16);
        let mut params = BTreeMap::from([
            ("id", self.client_id.as_str()),
            ("nonce", nonce.as_str()),
            ("otp", otp),
        ]);
        let h = sign(&self.key, &params);
        params.insert("h", &h);
        let mut last = None;
        for server in &self.servers {
            match self.ask(server, &params, otp, &nonce).await {
                Ok(answer) => return Ok(answer),
                Err(e) => {
                    tracing::warn!(server, error = %format!("{e:#}"), "Yubico OTP validation");
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("no validation servers")))
    }

    async fn ask(
        &self,
        server: &str,
        params: &BTreeMap<&str, &str>,
        otp: &str,
        nonce: &str,
    ) -> anyhow::Result<bool> {
        let mut url = reqwest::Url::parse(server)?;
        url.query_pairs_mut().extend_pairs(params);
        let body = self
            .http
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let fields = parse(&body);
        let status = fields.get("status").map(String::as_str).unwrap_or("");
        match status {
            "OK" => {
                let h = fields.get("h").context("the answer is not signed")?;
                let rest: BTreeMap<&str, &str> = fields
                    .iter()
                    .filter(|(k, _)| *k != "h")
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect();
                if !super::same(sign(&self.key, &rest).as_bytes(), h.as_bytes()) {
                    bail!("the answer's signature does not match (is yubico.secret_key right?)");
                }
                if fields.get("otp").map(String::as_str) != Some(otp)
                    || fields.get("nonce").map(String::as_str) != Some(nonce)
                {
                    bail!("the answer is for another request");
                }
                Ok(true)
            }
            // The OTP is not genuine, or was used before.
            "BAD_OTP" | "REPLAYED_OTP" => Ok(false),
            "" => bail!("unexpected answer"),
            other => bail!("the server says {other}"),
        }
    }
}

/// HMAC-SHA1 over the parameters in key order, `k=v` joined with `&`.
pub fn sign(key: &hmac::Key, params: &BTreeMap<&str, &str>) -> String {
    let text: Vec<String> = params.iter().map(|(k, v)| format!("{k}={v}")).collect();
    STANDARD.encode(hmac::sign(key, text.join("&").as_bytes()))
}

/// The `key=value` lines of an answer.
pub fn parse(body: &str) -> BTreeMap<String, String> {
    body.lines()
        .filter_map(|l| l.trim().split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_otps() {
        let otp = "cccjgjgkhcbbirdrfdnlnghhfgrtnnlgedjlftrbdeut";
        assert!(looks_like(otp));
        assert_eq!(public_id(otp), "cccjgjgkhcbb");
        assert!(!looks_like("123456"));
        assert!(!looks_like("abcd-efgh-ijkl"));
    }

    #[test]
    fn signs_like_the_protocol() {
        let key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, b"secret");
        let params = BTreeMap::from([("otp", "x"), ("id", "1"), ("nonce", "n")]);
        let text = "id=1&nonce=n&otp=x";
        assert_eq!(
            sign(&key, &params),
            STANDARD.encode(hmac::sign(&key, text.as_bytes()))
        );
        let body = "h=abc=\r\nt=2026-10-17T10:00:00Z0123\r\nstatus=OK\r\n\r\n";
        let fields = parse(body);
        assert_eq!(fields["h"], "abc=");
        assert_eq!(fields["status"], "OK");
    }
}
