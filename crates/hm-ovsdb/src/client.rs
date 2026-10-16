//! OVSDB JSON-RPC 1.0 client (RFC 7047).
//!
//! Messages are bare JSON values written back to back with no framing, so the
//! reader parses incrementally. The server periodically sends `echo` requests
//! as an inactivity probe; they are answered here, or the server would drop
//! the connection.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid address {0:?}; expected tcp:HOST:PORT, ssl:HOST:PORT or unix:PATH")]
    Address(String),
    #[error("tls: {0}")]
    Tls(String),
    #[error("{0} is not the cluster leader")]
    NotLeader(String),
    #[error("connection closed")]
    Closed,
    #[error("no reply within {0:?}")]
    Timeout(std::time::Duration),
    #[error("rpc error: {0}")]
    Rpc(Value),
    #[error("transaction failed: {0}")]
    Transaction(String),
    #[error("unexpected response: {0}")]
    Protocol(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Requests awaiting replies. `None` once the connection is closed: closing
/// takes the map in the same step that marks the connection dead, so a
/// request can never be registered on a connection nobody reads any more.
type Pending = Arc<Mutex<Option<HashMap<u64, oneshot::Sender<Result<Value>>>>>>;

/// Replies normally arrive in milliseconds; this bounds the wait when the
/// server vanished without the connection noticing.
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Marks the connection closed and fails everything still waiting.
fn close(pending: &Pending) {
    if let Some(waiters) = pending.lock().unwrap().take() {
        for (_, waiter) in waiters {
            let _ = waiter.send(Err(Error::Closed));
        }
    }
}

pub struct Client {
    next_id: AtomicU64,
    pending: Pending,
    tx: mpsc::UnboundedSender<Value>,
}

impl Client {
    /// Connects to `tcp:HOST:PORT`, `ssl:HOST:PORT` (needs `tls`, see
    /// [`crate::tls::client_config`]) or `unix:PATH`.
    pub async fn connect(addr: &str, tls: Option<&Arc<rustls::ClientConfig>>) -> Result<Self> {
        let tcp = |hostport: &str| {
            let hostport = hostport.to_owned();
            async move {
                let s = tokio::time::timeout(
                    CONNECT_TIMEOUT,
                    tokio::net::TcpStream::connect(&hostport),
                )
                .await
                .map_err(|_| Error::Timeout(CONNECT_TIMEOUT))??;
                s.set_nodelay(true)?;
                Ok::<_, Error>(s)
            }
        };
        if let Some(hostport) = addr.strip_prefix("tcp:") {
            let (r, w) = tcp(hostport).await?.into_split();
            Ok(Self::start(r, w))
        } else if let Some(hostport) = addr.strip_prefix("ssl:") {
            let tls = tls.ok_or_else(|| {
                Error::Tls(format!(
                    "{addr} needs a CA certificate, certificate and key"
                ))
            })?;
            let host = hostport
                .rsplit_once(':')
                .map(|(h, _)| h.trim_matches(['[', ']']))
                .ok_or_else(|| Error::Address(addr.into()))?;
            let name = rustls::pki_types::ServerName::try_from(host.to_owned())
                .map_err(|_| Error::Address(addr.into()))?;
            let stream = tcp(hostport).await?;
            let stream = tokio::time::timeout(
                CONNECT_TIMEOUT,
                tokio_rustls::TlsConnector::from(tls.clone()).connect(name, stream),
            )
            .await
            .map_err(|_| Error::Timeout(CONNECT_TIMEOUT))?
            .map_err(|e| Error::Tls(format!("{addr}: {e}")))?;
            let (r, w) = tokio::io::split(stream);
            Ok(Self::start(r, w))
        } else if let Some(path) = addr.strip_prefix("unix:") {
            let (r, w) = tokio::net::UnixStream::connect(path).await?.into_split();
            Ok(Self::start(r, w))
        } else {
            Err(Error::Address(addr.into()))
        }
    }

    fn start<R, W>(reader: R, mut writer: W) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
        let (tx, mut rx) = mpsc::unbounded_channel::<Value>();

        let p = pending.clone();
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                let bytes = serde_json::to_vec(&msg).expect("JSON values always serialise");
                if writer.write_all(&bytes).await.is_err() {
                    break;
                }
            }
            close(&p);
        });

        let reply = tx.clone();
        let p = pending.clone();
        tokio::spawn(async move {
            if let Err(e) = read_loop(reader, &p, &reply).await {
                tracing::debug!(error = %e, "ovsdb connection ended");
            }
            close(&p);
        });

        Self {
            next_id: AtomicU64::new(1),
            pending,
            tx,
        }
    }

    pub fn is_closed(&self) -> bool {
        self.pending.lock().unwrap().is_none()
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (done, wait) = oneshot::channel();
        match self.pending.lock().unwrap().as_mut() {
            Some(waiters) => waiters.insert(id, done),
            None => return Err(Error::Closed),
        };
        if self
            .tx
            .send(json!({"method": method, "params": params, "id": id}))
            .is_err()
        {
            close(&self.pending);
            return Err(Error::Closed);
        }
        match tokio::time::timeout(CALL_TIMEOUT, wait).await {
            Ok(reply) => reply.map_err(|_| Error::Closed)?,
            Err(_) => {
                if let Some(waiters) = self.pending.lock().unwrap().as_mut() {
                    waiters.remove(&id);
                }
                Err(Error::Timeout(CALL_TIMEOUT))
            }
        }
    }

    /// Runs a transaction. Returns one result per operation, or an error
    /// naming the first operation that failed.
    pub async fn transact(&self, db: &str, ops: Vec<Value>) -> Result<Vec<Value>> {
        let n = ops.len();
        let mut params = vec![json!(db)];
        params.extend(ops);
        let results = match self.call("transact", Value::Array(params)).await? {
            Value::Array(a) => a,
            other => return Err(Error::Protocol(other.to_string())),
        };
        // On failure the server appends a commit-level error after the
        // per-operation results; check everything.
        for (i, r) in results.iter().enumerate() {
            if let Some(err) = r.get("error").filter(|e| !e.is_null()) {
                let details = r.get("details").and_then(Value::as_str).unwrap_or("");
                let which = if i < n {
                    format!("operation {i}")
                } else {
                    "commit".into()
                };
                return Err(Error::Transaction(
                    format!("{which}: {err} {details}").trim().into(),
                ));
            }
        }
        Ok(results.into_iter().take(n).collect())
    }
}

async fn read_loop<R: AsyncRead + Unpin>(
    mut reader: R,
    pending: &Pending,
    reply: &mpsc::UnboundedSender<Value>,
) -> Result<()> {
    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            return Err(Error::Closed);
        }
        buf.extend_from_slice(&chunk[..n]);

        let mut consumed = 0;
        let mut stream = serde_json::Deserializer::from_slice(&buf).into_iter::<Value>();
        loop {
            match stream.next() {
                Some(Ok(msg)) => {
                    consumed = stream.byte_offset();
                    dispatch(msg, pending, reply);
                }
                Some(Err(e)) if e.is_eof() => break,
                Some(Err(e)) => return Err(Error::Protocol(e.to_string())),
                None => break,
            }
        }
        buf.drain(..consumed);
    }
}

fn dispatch(msg: Value, pending: &Pending, reply: &mpsc::UnboundedSender<Value>) {
    match msg.get("method").and_then(Value::as_str) {
        Some("echo") => {
            let _ = reply.send(json!({"id": msg["id"], "result": msg["params"], "error": null}));
        }
        // Monitor updates are not used yet.
        Some(_) => {}
        None => {
            let Some(id) = msg.get("id").and_then(Value::as_u64) else {
                return;
            };
            let Some(waiter) = pending.lock().unwrap().as_mut().and_then(|w| w.remove(&id)) else {
                return;
            };
            let result = match msg.get("error").filter(|e| !e.is_null()) {
                Some(err) => Err(Error::Rpc(err.clone())),
                None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = waiter.send(result);
        }
    }
}

/// Helpers for OVSDB's JSON encoding of values and conditions.
pub mod enc {
    use serde_json::{Value, json};

    pub fn uuid(u: &str) -> Value {
        json!(["uuid", u])
    }

    pub fn named(name: &str) -> Value {
        json!(["named-uuid", name])
    }

    pub fn set<I: IntoIterator<Item = Value>>(items: I) -> Value {
        json!(["set", items.into_iter().collect::<Vec<_>>()])
    }

    pub fn map<'a, I: IntoIterator<Item = (&'a str, String)>>(pairs: I) -> Value {
        json!([
            "map",
            pairs
                .into_iter()
                .map(|(k, v)| json!([k, v]))
                .collect::<Vec<_>>()
        ])
    }

    /// Condition matching rows whose `external_ids` contains `key=value`.
    pub fn has_external_id(key: &str, value: &str) -> Value {
        json!(["external_ids", "includes", map([(key, value.to_owned())])])
    }

    /// Extracts the UUID string from an `["uuid", "..."]` value.
    pub fn as_uuid(v: &Value) -> Option<&str> {
        match v.as_array()?.as_slice() {
            [tag, id] if tag == "uuid" => id.as_str(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake server that answers one transact, probes with echo, and sends a
    /// response split across writes, exercising incremental parsing.
    #[tokio::test]
    async fn rpc_over_unframed_stream_with_echo() {
        let (client_io, server_io) = tokio::io::duplex(4096);
        let (cr, cw) = tokio::io::split(client_io);
        let client = Client::start(cr, cw);
        let (mut sr, mut sw) = tokio::io::split(server_io);

        let server = tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            let n = sr.read(&mut buf).await.unwrap();
            let req: Value = serde_json::from_slice(&buf[..n]).unwrap();
            assert_eq!(req["method"], "transact");

            // Echo probe first; the client must answer it.
            sw.write_all(br#"{"method":"echo","params":["x"],"id":"echo"}"#)
                .await
                .unwrap();
            let n = sr.read(&mut buf).await.unwrap();
            let echo: Value = serde_json::from_slice(&buf[..n]).unwrap();
            assert_eq!(echo["id"], "echo");
            assert_eq!(echo["result"], json!(["x"]));

            let resp = serde_json::to_vec(
                &json!({"id": req["id"], "result": [{"rows": []}], "error": null}),
            )
            .unwrap();
            let (a, b) = resp.split_at(resp.len() / 2);
            sw.write_all(a).await.unwrap();
            sw.flush().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            sw.write_all(b).await.unwrap();
        });

        let res = client
            .transact(
                "OVN_Northbound",
                vec![json!({"op": "select", "table": "x", "where": []})],
            )
            .await
            .unwrap();
        assert_eq!(res, vec![json!({"rows": []})]);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn transaction_errors_are_reported() {
        let (client_io, server_io) = tokio::io::duplex(4096);
        let (cr, cw) = tokio::io::split(client_io);
        let client = Client::start(cr, cw);
        let (mut sr, mut sw) = tokio::io::split(server_io);
        tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            let n = sr.read(&mut buf).await.unwrap();
            let req: Value = serde_json::from_slice(&buf[..n]).unwrap();
            let resp = json!({"id": req["id"], "error": null, "result": [
                {"uuid": ["uuid", "1"]},
                {"error": "constraint violation", "details": "duplicate name"}
            ]});
            sw.write_all(&serde_json::to_vec(&resp).unwrap())
                .await
                .unwrap();
        });
        let err = client
            .transact("db", vec![json!({}), json!({})])
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("operation 1: \"constraint violation\" duplicate name"),
            "{err}"
        );
    }

    /// A request made after the server went away must fail, not wait forever.
    #[tokio::test]
    async fn requests_on_a_dead_connection_fail_promptly() {
        let (client_io, server_io) = tokio::io::duplex(4096);
        let (cr, cw) = tokio::io::split(client_io);
        let client = Client::start(cr, cw);
        drop(server_io);
        // Let the reader notice.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(client.is_closed());
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client.call("echo", json!([])),
        )
        .await;
        assert!(matches!(res, Ok(Err(Error::Closed))), "{res:?}");
    }

    #[test]
    fn encodings() {
        assert_eq!(enc::as_uuid(&enc::uuid("abc")), Some("abc"));
        assert_eq!(
            enc::has_external_id("k", "v"),
            json!(["external_ids", "includes", ["map", [["k", "v"]]]])
        );
    }
}
