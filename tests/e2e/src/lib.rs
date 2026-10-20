//! Helpers for end-to-end tests against a live controller and lab.
//!
//! Tests run only when `MST_E2E_URL` is set, e.g.
//! `MST_E2E_URL=http://127.0.0.1:18080 MST_E2E_PASSWORD=... cargo test -p mst-e2e -- --nocapture`.

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
    let Ok(base) = std::env::var("MST_E2E_URL") else {
        eprintln!("MST_E2E_URL not set; skipping");
        return Ok(None);
    };
    // reqwest uses the process-wide rustls provider.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let http = reqwest::Client::builder().cookie_store(true).build()?;
    let api = Api {
        base: base.trim_end_matches('/').to_owned(),
        http,
    };
    let user = std::env::var("MST_E2E_USER").unwrap_or_else(|_| "admin".into());
    let password = std::env::var("MST_E2E_PASSWORD").context("MST_E2E_PASSWORD is required")?;
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

    /// A client without a session that reaches the controller at `base` under
    /// another host name (`domain`), as a browser at that name would.
    pub fn anonymous_at(base: &str, domain: &str) -> Result<Api> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let url = reqwest::Url::parse(base)?;
        let addr = url
            .socket_addrs(|| None)?
            .into_iter()
            .next()
            .context("controller address")?;
        let http = reqwest::Client::builder()
            .cookie_store(true)
            .resolve(domain, addr)
            .build()?;
        Ok(Api {
            base: format!("{}://{domain}:{}", url.scheme(), addr.port()),
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

    /// Waits for an image being added to be done: ready, or failed. Its
    /// file comes to a host (downloaded by the controller, or uploaded),
    /// which then stores it.
    pub async fn wait_image(&self, id: &str, timeout: Duration) -> Result<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            let images = self.get("/images").await?;
            let img = images
                .as_array()
                .into_iter()
                .flatten()
                .find(|i| i["id"] == id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no image {id}"))?;
            if matches!(img["state"].as_str(), Some("ready" | "failed")) {
                return Ok(img);
            }
            if Instant::now() > deadline {
                bail!("image {id} still {} after {timeout:?}", img["state"]);
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    /// Waits for an image being added to be ready (failing if adding it
    /// failed).
    pub async fn wait_image_ready(&self, id: &str, timeout: Duration) -> Result<Value> {
        let img = self.wait_image(id, timeout).await?;
        if img["state"] != "ready" {
            bail!("adding image {id} failed: {img}");
        }
        Ok(img)
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
        let url = std::env::var("MST_E2E_IMAGE_URL").unwrap_or_else(|_| {
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

    /// Sends raw bytes (`PUT`, as an upload's parts go), returning the
    /// status and the JSON answer.
    pub async fn put_bytes(&self, path: &str, body: Vec<u8>) -> Result<(u16, Value)> {
        let resp = self
            .http
            .put(format!("{}/api/v1{path}", self.base))
            .header("content-type", "application/octet-stream")
            .body(body)
            .send()
            .await?;
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
            // A first login starts the user's session, slowly on a busy host.
            self.send(format!("{user}\r").as_bytes()).await?;
            self.expect("assword:", Duration::from_secs(60)).await?;
            self.send(format!("{password}\r").as_bytes()).await?;
            self.expect("$ ", Duration::from_secs(90)).await?;
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

    /// Sends a raw TPM 2.0 command to a Linux guest's TPM (no tools needed
    /// in the guest) and returns its response.
    async fn tpm(&mut self, user: &str, password: &str, command: &[u8]) -> Result<Vec<u8>> {
        let octal: String = command.iter().map(|b| format!("\\{b:03o}")).collect();
        // The marker is split by quotes in the echoed command line, so only
        // the output has it whole.
        let cmd = format!(
            "echo \"T\"\"PM=$(sudo sh -c 'exec 3<>/dev/tpmrm0; printf \"{octal}\" >&3; dd bs=4096 count=1 <&3 2>/dev/null' | od -An -tx1 | tr -d ' \\n')\""
        );
        let (out, code) = self.run(user, password, &cmd).await?;
        anyhow::ensure!(code == 0, "TPM command failed ({code}): {out}");
        let hex = out
            .rsplit_once("TPM=")
            .map(|(_, rest)| {
                rest.split(|c: char| !c.is_ascii_hexdigit())
                    .next()
                    .unwrap_or("")
            })
            .unwrap_or("");
        let bytes: Vec<u8> = (0..hex.len() / 2)
            .filter_map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok())
            .collect();
        anyhow::ensure!(bytes.len() >= 10, "no TPM response in {out:?}");
        let rc = u32::from_be_bytes(bytes[6..10].try_into().unwrap());
        anyhow::ensure!(rc == 0, "TPM response code {rc:#x}");
        Ok(bytes)
    }

    /// Whether the guest's TPM has a password on its owner hierarchy: kept
    /// in the TPM's own lasting state, so it tells that state was kept.
    pub async fn tpm_owner_auth_set(&mut self, user: &str, password: &str) -> Result<bool> {
        // TPM2_GetCapability(TPM_CAP_TPM_PROPERTIES, TPM_PT_PERMANENT, 1).
        let cmd = [
            0x80, 0x01, 0, 0, 0, 0x16, 0, 0, 0x01, 0x7a, 0, 0, 0, 0x06, 0, 0, 0x02, 0, 0, 0, 0, 1,
        ];
        let r = self.tpm(user, password, &cmd).await?;
        // TPMA_PERMANENT, last; its bit 0 is ownerAuthSet.
        anyhow::ensure!(r.len() >= 4, "short TPM response");
        Ok(r[r.len() - 1] & 1 == 1)
    }

    /// Gives the guest's TPM owner hierarchy a password.
    pub async fn tpm_set_owner_auth(&mut self, user: &str, password: &str) -> Result<()> {
        // TPM2_HierarchyChangeAuth(TPM_RH_OWNER) with the empty password
        // session, to "test".
        let cmd = [
            0x80, 0x02, 0, 0, 0, 0x21, 0, 0, 0x01, 0x29, 0x40, 0, 0, 0x01, 0, 0, 0, 0x09, 0x40, 0,
            0, 0x09, 0, 0, 0, 0, 0, 0, 0x04, b't', b'e', b's', b't',
        ];
        self.tpm(user, password, &cmd).await.map(drop)
    }

    /// Signs in on a Windows guest's serial console (its Special
    /// Administration Console) with a command channel, runs `cmd` there, and
    /// reads until `until` appears in what it prints (the console redraws
    /// itself, so prompts say little). Fails if the password is refused.
    pub async fn sac_run(
        &mut self,
        user: &str,
        password: &str,
        cmd: &str,
        until: &str,
    ) -> Result<String> {
        self.send(b"\r").await?;
        self.expect("SAC>", Duration::from_secs(600)).await?;
        self.send(b"cmd\r").await?;
        // The channel comes a moment later, announced by name (CmdNNNN);
        // Windows announces channels of its own (setup's logs) meanwhile.
        self.expect("Channel: Cmd", Duration::from_secs(120))
            .await?;
        let digits = self
            .expect_with(Duration::from_secs(30), |s| {
                let n: String = s.chars().take_while(char::is_ascii_digit).collect();
                (!n.is_empty() && s.len() > n.len()).then_some(n)
            })
            .await?;
        let channel = format!("Cmd{digits}");
        self.send(format!("ch -sn {channel}\r").as_bytes()).await?;
        self.expect("any other key", Duration::from_secs(30))
            .await?;
        self.send(b"\r").await?;
        self.expect("Username:", Duration::from_secs(60)).await?;
        self.send(format!("{user}\r").as_bytes()).await?;
        self.expect("Domain", Duration::from_secs(30)).await?;
        self.send(b"\r").await?;
        self.expect("Password:", Duration::from_secs(30)).await?;
        self.send(format!("{password}\r").as_bytes()).await?;
        let signed_in = self
            .expect_any(
                &["System32>", "Unable to authenticate"],
                Duration::from_secs(120),
            )
            .await?;
        if signed_in.contains("Unable to authenticate") {
            bail!("the console refused the password");
        }
        // What is typed while the new session still draws its screen is
        // lost: typed again until it shows.
        for _ in 0..5 {
            self.send(format!("{cmd}\r").as_bytes()).await?;
            if let Ok(out) = self.expect(until, Duration::from_secs(10)).await {
                return Ok(out);
            }
        }
        self.send(format!("{cmd}\r").as_bytes()).await?;
        self.expect(until, Duration::from_secs(30)).await
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

/// With `MST_E2E_DEBUG` set, prints console traffic to stderr.
fn debug(dir: &str, data: &[u8]) {
    if std::env::var_os("MST_E2E_DEBUG").is_some() {
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

/// Hosts from `/hosts` that are in service (not decommissioned).
pub fn in_service(hosts: &Value) -> Vec<Value> {
    hosts
        .as_array()
        .into_iter()
        .flatten()
        .filter(|h| h["state"] != "decommissioned")
        .cloned()
        .collect()
}

/// How a test's body ended: returned, or panicked (a failed assertion).
pub struct Outcome(std::thread::Result<Result<()>>);

/// Runs a test's body, catching a panic so what it made can still be
/// removed afterwards (see [`Outcome::finish`]).
pub async fn attempt(body: impl std::future::Future<Output = Result<()>>) -> Outcome {
    use futures::FutureExt;
    Outcome(std::panic::AssertUnwindSafe(body).catch_unwind().await)
}

impl Outcome {
    /// The test's result, after cleaning up: a panic is raised again, an
    /// error returned; a body that passed fails if cleaning up did not, so
    /// leftovers do not go unnoticed.
    pub fn finish(self, cleanup: Result<()>) -> Result<()> {
        match (self.0, cleanup) {
            (Err(panic), cleanup) => {
                if let Err(e) = cleanup {
                    eprintln!("cleaning up: {e:#}");
                }
                std::panic::resume_unwind(panic)
            }
            (Ok(Err(e)), cleanup) => {
                if let Err(c) = cleanup {
                    eprintln!("cleaning up: {c:#}");
                }
                Err(e)
            }
            (Ok(Ok(())), cleanup) => cleanup.context("cleaning up"),
        }
    }
}

/// A name for this run of `suite`, which what the run makes is named after:
/// unique among suites run at the same time, so that cleaning up after one
/// never removes another's.
pub fn run_name(suite: &str) -> String {
    // Milliseconds: a suite's tests may follow each other within a second.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("e2e-{suite}-{now:x}")
}

/// Runs a test's body, then removes what its run made ([`clean`]), however
/// the body ended.
pub async fn cleaned_up(
    api: &Api,
    run: &str,
    body: impl std::future::Future<Output = Result<()>>,
) -> Result<()> {
    let outcome = attempt(body).await;
    outcome.finish(clean(api, run).await)
}

/// Removes what a run made: VMs, disks and images named after it, and its tenants
/// (those whose slug path starts with it) with what they hold (VMs, disks,
/// images), deepest first. Says what it could not remove.
pub async fn clean(api: &Api, run: &str) -> Result<()> {
    let ours = |path: &str| path.split('/').next().is_some_and(|s| s.starts_with(run));
    let tenants: Vec<Value> = api
        .get("/tenants")
        .await?
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|t| t["slug_path"].as_str().is_some_and(ours))
        .collect();
    let tenant_ids: Vec<&str> = tenants.iter().filter_map(|t| t["id"].as_str()).collect();
    let mut failed = vec![];

    let vms = api.get("/vms?descendants=true").await?;
    let vms: Vec<&Value> = vms
        .as_array()
        .into_iter()
        .flatten()
        .filter(|v| {
            v["name"].as_str().is_some_and(|n| n.starts_with(run))
                || v["tenant_id"]
                    .as_str()
                    .is_some_and(|t| tenant_ids.contains(&t))
        })
        .collect();
    let deletions = vms.iter().map(|v| async move {
        let id = v["id"].as_str().unwrap_or_default();
        delete_vm(api, id)
            .await
            .with_context(|| format!("VM {}", v["name"]))
    });
    for r in futures::future::join_all(deletions).await {
        if let Err(e) = r {
            failed.push(format!("{e:#}"));
        }
    }

    // Disks of their own, attached to nothing once the VMs are gone.
    let disks = api.get("/disks").await?;
    for d in disks.as_array().into_iter().flatten() {
        let ours = d["name"].as_str().is_some_and(|n| n.starts_with(run))
            || d["tenant_id"]
                .as_str()
                .is_some_and(|t| tenant_ids.contains(&t));
        if !ours {
            continue;
        }
        let path = format!("/disks/{}", d["id"].as_str().unwrap_or_default());
        match api.try_send(reqwest::Method::DELETE, &path, None).await {
            Ok((st, _)) if st < 300 || st == 404 => {}
            Ok((_, b)) => failed.push(format!("disk {}: {b}", d["name"])),
            Err(e) => failed.push(format!("disk {}: {e:#}", d["name"])),
        }
    }

    for t in &tenant_ids {
        let images = api.get(&format!("/images?tenant_id={t}")).await?;
        for i in images.as_array().into_iter().flatten() {
            if i["tenant_id"].as_str() == Some(t) {
                let path = format!("/images/{}", i["id"].as_str().unwrap_or_default());
                if let Ok((st, b)) = api.try_send(reqwest::Method::DELETE, &path, None).await
                    && st >= 300
                    && st != 404
                {
                    failed.push(format!("image {}: {b}", i["name"]));
                }
            }
        }
    }

    // Images named after the run, in the caller's tenant.
    let images = api.get("/images").await?;
    for i in images.as_array().into_iter().flatten() {
        if !i["name"].as_str().is_some_and(|n| n.starts_with(run)) {
            continue;
        }
        let path = format!("/images/{}", i["id"].as_str().unwrap_or_default());
        match api.try_send(reqwest::Method::DELETE, &path, None).await {
            Ok((st, _)) if st < 300 || st == 404 => {}
            Ok((_, b)) => failed.push(format!("image {}: {b}", i["name"])),
            Err(e) => failed.push(format!("image {}: {e:#}", i["name"])),
        }
    }

    // Sub-tenants before their parents.
    let mut by_depth: Vec<&Value> = tenants.iter().collect();
    by_depth.sort_by_key(|t| {
        std::cmp::Reverse(t["slug_path"].as_str().map_or(0, |p| p.split('/').count()))
    });
    for t in by_depth {
        let path = format!("/tenants/{}", t["id"].as_str().unwrap_or_default());
        match api.try_send(reqwest::Method::DELETE, &path, None).await {
            Ok((st, _)) if st < 300 || st == 404 => {}
            Ok((_, b)) => failed.push(format!("tenant {}: {b}", t["slug_path"])),
            Err(e) => failed.push(format!("tenant {}: {e:#}", t["slug_path"])),
        }
    }

    if failed.is_empty() {
        Ok(())
    } else {
        bail!("left behind: {}", failed.join("; "))
    }
}

/// Deletes a VM and waits for it to go. A VM busy with something else (a
/// migration, a backup) is tried again for a while.
pub async fn delete_vm(api: &Api, id: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        let (st, body) = api
            .try_send(reqwest::Method::DELETE, &format!("/vms/{id}"), None)
            .await?;
        match st {
            202 => {
                if let Some(task) = body["task_id"].as_str() {
                    api.wait_task(task, Duration::from_secs(300)).await?;
                }
                return Ok(());
            }
            404 => return Ok(()),
            409 if Instant::now() < deadline => tokio::time::sleep(Duration::from_secs(5)).await,
            _ => bail!("deleting it: {st} {body}"),
        }
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
