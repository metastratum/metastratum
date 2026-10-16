//! Helpers for end-to-end tests against a live controller and lab.
//!
//! Tests run only when `HM_E2E_URL` is set, e.g.
//! `HM_E2E_URL=http://127.0.0.1:18080 HM_E2E_PASSWORD=... cargo test -p hm-e2e -- --nocapture`.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

#[derive(Clone)]
pub struct Api {
    pub base: String,
    http: reqwest::Client,
}

/// Returns a logged-in client, or `None` when e2e tests are not configured.
pub async fn connect() -> Result<Option<Api>> {
    let Ok(base) = std::env::var("HM_E2E_URL") else {
        eprintln!("HM_E2E_URL not set; skipping");
        return Ok(None);
    };
    // reqwest uses the process-wide rustls provider.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let http = reqwest::Client::builder().cookie_store(true).build()?;
    let api = Api {
        base: base.trim_end_matches('/').to_owned(),
        http,
    };
    let user = std::env::var("HM_E2E_USER").unwrap_or_else(|_| "admin".into());
    let password = std::env::var("HM_E2E_PASSWORD").context("HM_E2E_PASSWORD is required")?;
    api.send(
        reqwest::Method::POST,
        "/auth/login",
        Some(json!({"username": user, "password": password})),
    )
    .await
    .context("login")?;
    Ok(Some(api))
}

impl Api {
    /// Signs in as a specific user, optionally of a sub-tenant (`acme/dev`).
    pub async fn login(
        base: &str,
        username: &str,
        password: &str,
        tenant: Option<&str>,
    ) -> Result<Api> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder().cookie_store(true).build()?;
        let api = Api {
            base: base.trim_end_matches('/').to_owned(),
            http,
        };
        api.send(
            reqwest::Method::POST,
            "/auth/login",
            Some(json!({"username": username, "password": password, "tenant": tenant})),
        )
        .await
        .with_context(|| format!("login as {username}"))?;
        Ok(api)
    }

    /// A client authenticating with a bearer token instead of a session.
    pub fn with_token(base: &str, token: &str) -> Result<Api> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {token}").parse()?,
        );
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .build()?;
        Ok(Api {
            base: base.trim_end_matches('/').to_owned(),
            http,
        })
    }

    /// Like `send`, but returns error statuses instead of failing.
    pub async fn try_send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(u16, Value)> {
        let mut req = self
            .http
            .request(method, format!("{}/api/v1{path}", self.base));
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await?;
        let status = resp.status().as_u16();
        let text = resp.text().await?;
        Ok((
            status,
            if text.is_empty() {
                Value::Null
            } else {
                serde_json::from_str(&text)?
            },
        ))
    }

    pub async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(u16, Value)> {
        let mut req = self
            .http
            .request(method, format!("{}/api/v1{path}", self.base));
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await?;
        let status = resp.status().as_u16();
        let text = resp.text().await?;
        let value = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text)?
        };
        if status >= 400 {
            bail!("{path}: HTTP {status}: {value}");
        }
        Ok((status, value))
    }

    /// Opens the live event stream (`/events`).
    pub async fn events(&self) -> Result<Events> {
        let resp = self
            .http
            .get(format!("{}/api/v1/events", self.base))
            .send()
            .await?
            .error_for_status()?;
        Ok(Events {
            body: Box::pin(resp.bytes_stream()),
            buf: String::new(),
        })
    }

    pub async fn get(&self, path: &str) -> Result<Value> {
        Ok(self.send(reqwest::Method::GET, path, None).await?.1)
    }

    pub async fn post(&self, path: &str, body: Value) -> Result<Value> {
        Ok(self.send(reqwest::Method::POST, path, Some(body)).await?.1)
    }

    /// Status code of a GET, without failing on errors.
    pub async fn status_of(&self, path: &str) -> Result<u16> {
        Ok(self
            .http
            .get(format!("{}/api/v1{path}", self.base))
            .send()
            .await?
            .status()
            .as_u16())
    }

    /// Waits for a task to finish; fails if it fails.
    pub async fn wait_task(&self, id: &str, timeout: Duration) -> Result<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            let t = self.get(&format!("/tasks/{id}")).await?;
            match t["state"].as_str() {
                Some("succeeded") => return Ok(t),
                Some("failed") => bail!("task {} failed: {}", t["kind"], t["error"]),
                _ if Instant::now() > deadline => {
                    bail!("task {id} still {} after {timeout:?}", t["state"])
                }
                _ => tokio::time::sleep(Duration::from_millis(500)).await,
            }
        }
    }

    /// Waits for a task to end, however it ends, and returns it.
    pub async fn wait_task_end(&self, id: &str, timeout: Duration) -> Result<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            let t = self.get(&format!("/tasks/{id}")).await?;
            if matches!(
                t["state"].as_str(),
                Some("succeeded" | "failed" | "cancelled")
            ) {
                return Ok(t);
            }
            if Instant::now() > deadline {
                bail!("task {id} still {} after {timeout:?}", t["state"]);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Polls a VM until `pred` holds.
    pub async fn wait_vm(
        &self,
        id: &str,
        timeout: Duration,
        pred: impl Fn(&Value) -> bool,
    ) -> Result<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            let vm = self.get(&format!("/vms/{id}")).await?;
            if pred(&vm) {
                return Ok(vm);
            }
            if Instant::now() > deadline {
                bail!("VM {id} did not reach the expected state: {vm}");
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Finds or registers the test image, shared so sub-tenants can use it.
    pub async fn image(&self) -> Result<String> {
        let name = "e2e-debian-13-shared";
        let images = self.get("/images").await?;
        if let Some(i) = images
            .as_array()
            .into_iter()
            .flatten()
            .find(|i| i["name"] == name)
        {
            return Ok(i["id"].as_str().unwrap().to_owned());
        }
        let url = std::env::var("HM_E2E_IMAGE_URL").unwrap_or_else(|_| {
            "https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2".into()
        });
        let img = self
            .post(
                "/images",
                json!({"name": name, "source_url": url, "format": "qcow2", "shared": true}),
            )
            .await?;
        Ok(img["id"].as_str().unwrap().to_owned())
    }

    /// Opens a console WebSocket via a fresh ticket.
    pub async fn console(&self, vm: &str, kind: &str) -> Result<Console> {
        let t = self
            .post(&format!("/vms/{vm}/console"), json!({"kind": kind}))
            .await?;
        Console::open(&self.ws_url(t["path"].as_str().unwrap())).await
    }

    pub fn ws_url(&self, path: &str) -> String {
        format!("{}{path}", self.base.replacen("http", "ws", 1))
    }
}

pub struct Console {
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    buf: Vec<u8>,
}

impl Console {
    pub async fn open(url: &str) -> Result<Self> {
        let (ws, _) = tokio_tungstenite::connect_async(url).await?;
        Ok(Self {
            ws,
            buf: Vec::new(),
        })
    }

    pub async fn send(&mut self, data: &[u8]) -> Result<()> {
        debug(">>", data);
        self.ws.send(Message::Binary(data.to_vec().into())).await?;
        Ok(())
    }

    /// Reads until `needle` appears and returns everything up to and
    /// including it. Anything after the needle stays buffered for the next
    /// read: output can arrive in arbitrary chunks.
    pub async fn expect(&mut self, needle: &str, timeout: Duration) -> Result<String> {
        self.expect_any(&[needle], timeout).await
    }

    /// Logs in on a serial console (if needed) and runs a command, returning
    /// its output and exit status.
    pub async fn run(&mut self, user: &str, password: &str, cmd: &str) -> Result<(String, i32)> {
        self.send(b"\r").await?;
        let screen = self
            .expect_any(&["login:", "$ "], Duration::from_secs(180))
            .await?;
        if screen.contains("login:") {
            self.send(format!("{user}\r").as_bytes()).await?;
            self.expect("assword:", Duration::from_secs(20)).await?;
            self.send(format!("{password}\r").as_bytes()).await?;
            self.expect("$ ", Duration::from_secs(30)).await?;
        }
        // Quoting splits the marker in the echoed command line, so only the
        // command's own output contains `__rc=<code>__`.
        self.send(format!("{cmd}; echo \"__rc\"\"=$?__\"\r").as_bytes())
            .await?;
        let out = self.expect("__rc=", Duration::from_secs(60)).await?;
        let code = self
            .expect_with(Duration::from_secs(10), |s| {
                let end = s.find("__")?;
                s[..end].trim().parse::<i32>().ok()
            })
            .await?;
        Ok((out, code))
    }

    /// Reads until `parse` succeeds on what has been read since the call.
    async fn expect_with<T>(
        &mut self,
        timeout: Duration,
        parse: impl Fn(&str) -> Option<T>,
    ) -> Result<T> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(v) = parse(&String::from_utf8_lossy(&self.buf)) {
                // Callers only read the remainder of the current line.
                self.buf.clear();
                return Ok(v);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, self.ws.next()).await {
                Ok(Some(Ok(Message::Binary(b)))) => {
                    debug("<<", &b);
                    self.buf.extend_from_slice(&b)
                }
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(e))) => bail!("console error: {e}"),
                Ok(None) => bail!("console closed"),
                Err(_) => bail!("timed out; got: {}", String::from_utf8_lossy(&self.buf)),
            }
        }
    }

    /// Reads until any of `needles` appears; consumes up to the first match.
    async fn expect_any(&mut self, needles: &[&str], timeout: Duration) -> Result<String> {
        let deadline = Instant::now() + timeout;
        let longest = needles.iter().map(|n| n.len()).max().unwrap_or(0);
        let mut from = 0;
        loop {
            let found = needles
                .iter()
                .filter_map(|n| find(&self.buf, n.as_bytes(), from))
                .min();
            if let Some(end) = found {
                let rest = self.buf.split_off(end);
                let head = std::mem::replace(&mut self.buf, rest);
                return Ok(String::from_utf8_lossy(&head).into_owned());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, self.ws.next()).await {
                Ok(Some(Ok(Message::Binary(b)))) => {
                    debug("<<", &b);
                    // Only the new bytes (and a needle's length before them)
                    // can complete a match. Rescanning everything per chunk is
                    // quadratic: slow enough that console backpressure
                    // throttles the guest while it boots.
                    from = self.buf.len().saturating_sub(longest);
                    self.buf.extend_from_slice(&b)
                }
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(e))) => bail!("console error: {e}"),
                Ok(None) => bail!("console closed while waiting for {needles:?}"),
                Err(_) => bail!(
                    "timed out waiting for {needles:?}; got: {}",
                    String::from_utf8_lossy(&self.buf)
                ),
            }
        }
    }
}

/// With `HM_E2E_DEBUG` set, prints console traffic to stderr.
fn debug(dir: &str, data: &[u8]) {
    if std::env::var_os("HM_E2E_DEBUG").is_some() {
        eprintln!("{dir} {:?}", String::from_utf8_lossy(data));
    }
}

/// End offset of the first occurrence of `needle` in `hay` starting at or
/// after `from`.
fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    let from = from.min(hay.len());
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| from + i + needle.len())
}

/// A Server-Sent Events stream.
pub struct Events {
    body: std::pin::Pin<Box<dyn futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    buf: String,
}

impl Events {
    /// The next event as (name, data), or `None` if none comes in time.
    pub async fn next(&mut self, within: Duration) -> Result<Option<(String, Value)>> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            if let Some(end) = self.buf.find("\n\n") {
                let block: String = self.buf.drain(..end + 2).collect();
                let mut name = String::from("message");
                let mut data = String::new();
                for line in block.lines() {
                    if let Some(v) = line.strip_prefix("event:") {
                        name = v.trim().to_owned();
                    } else if let Some(v) = line.strip_prefix("data:") {
                        data.push_str(v.trim());
                    }
                }
                // Comments (keep-alives) have neither.
                if data.is_empty() {
                    continue;
                }
                return Ok(Some((name, serde_json::from_str(&data)?)));
            }
            match tokio::time::timeout_at(deadline, self.body.next()).await {
                Err(_) => return Ok(None),
                Ok(None) => bail!("event stream ended"),
                Ok(Some(chunk)) => self.buf.push_str(&String::from_utf8_lossy(&chunk?)),
            }
        }
    }

    /// Waits for an event about `id`, skipping others.
    pub async fn wait_for(
        &mut self,
        kind: &str,
        id: &str,
        within: Duration,
    ) -> Result<Option<Value>> {
        let deadline = Instant::now() + within;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.next(left).await? {
                Some((k, data)) if k == kind && data["id"] == id => return Ok(Some(data)),
                Some(_) => {}
                None => break,
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::find;

    #[test]
    fn find_returns_the_end_of_the_first_match() {
        assert_eq!(find(b"ab__rc=0__", b"__rc=", 0), Some(7));
        assert_eq!(find(b"ab__rc=0__", b"__rc=", 2), Some(7));
        assert_eq!(find(b"ab__rc=0__", b"__rc=", 3), None);
        assert_eq!(find(b"abc", b"c", 9), None);
    }
}
