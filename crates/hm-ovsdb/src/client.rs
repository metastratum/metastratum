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
    #[error("invalid address {0:?}; expected tcp:HOST:PORT or unix:PATH")]
    Address(String),
    #[error("connection closed")]
    Closed,
    #[error("rpc error: {0}")]
    Rpc(Value),
    #[error("transaction failed: {0}")]
    Transaction(String),
    #[error("unexpected response: {0}")]
    Protocol(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;

pub struct Client {
    next_id: AtomicU64,
    pending: Pending,
    tx: mpsc::UnboundedSender<Value>,
}

impl Client {
    /// Connects to `tcp:HOST:PORT` or `unix:PATH`.
    pub async fn connect(addr: &str) -> Result<Self> {
        if let Some(hostport) = addr.strip_prefix("tcp:") {
            let s = tokio::net::TcpStream::connect(hostport).await?;
            s.set_nodelay(true)?;
            let (r, w) = s.into_split();
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
        let pending: Pending = Arc::default();
        let (tx, mut rx) = mpsc::unbounded_channel::<Value>();

        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                let bytes = serde_json::to_vec(&msg).expect("JSON values always serialise");
                if writer.write_all(&bytes).await.is_err() {
                    break;
                }
            }
        });

        let reply = tx.clone();
        let p = pending.clone();
        tokio::spawn(async move {
            if let Err(e) = read_loop(reader, &p, &reply).await {
                tracing::debug!(error = %e, "ovsdb connection ended");
            }
            // Fail everything still waiting so callers can reconnect.
            for (_, waiter) in p.lock().unwrap().drain() {
                let _ = waiter.send(Err(Error::Closed));
            }
        });

        Self {
            next_id: AtomicU64::new(1),
            pending,
            tx,
        }
    }

    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (done, wait) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, done);
        if self
            .tx
            .send(json!({"method": method, "params": params, "id": id}))
            .is_err()
        {
            self.pending.lock().unwrap().remove(&id);
            return Err(Error::Closed);
        }
        wait.await.map_err(|_| Error::Closed)?
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
            let Some(waiter) = pending.lock().unwrap().remove(&id) else {
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

    #[test]
    fn encodings() {
        assert_eq!(enc::as_uuid(&enc::uuid("abc")), Some("abc"));
        assert_eq!(
            enc::has_external_id("k", "v"),
            json!(["external_ids", "includes", ["map", [["k", "v"]]]])
        );
    }
}
