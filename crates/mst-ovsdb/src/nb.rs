//! A northbound database connection. What to write is decided by
//! `mst-netplan`; this only reads rows and runs transactions.

use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::client::{Client, Error, Result};

/// Geneve adds 58 bytes (IPv4 underlay with options); this keeps guests
/// within a standard 1500-byte underlay.
pub const DEFAULT_MTU: u32 = 1442;

/// A northbound connection that reconnects lazily after failures.
///
/// It may be given several remotes (comma-separated, as OVN's tools take
/// them): the servers of a clustered database. Like `ovn-nbctl`, it only
/// uses the cluster's leader, so what it reads is never behind what it
/// wrote; when the leader changes it moves on to the next remote.
pub struct Northbound {
    /// The database's name.
    db: &'static str,
    remotes: Vec<String>,
    tls: Option<Arc<rustls::ClientConfig>>,
    client: Mutex<(usize, Option<Arc<Client>>)>,
}

impl Northbound {
    /// `remotes` as `tcp:HOST:PORT`, `ssl:HOST:PORT` or `unix:PATH`,
    /// comma-separated; `ssl:` needs `tls`.
    pub fn new(remotes: &str, tls: Option<Arc<rustls::ClientConfig>>) -> Self {
        Self::of("OVN_Northbound", remotes, tls)
    }

    fn of(db: &'static str, remotes: &str, tls: Option<Arc<rustls::ClientConfig>>) -> Self {
        Self {
            db,
            remotes: remotes
                .split(',')
                .map(|r| r.trim().to_owned())
                .filter(|r| !r.is_empty())
                .collect(),
            tls,
            client: Mutex::new((0, None)),
        }
    }

    async fn client(&self) -> Result<Arc<Client>> {
        let mut slot = self.client.lock().await;
        if let Some(c) = slot.1.as_ref().filter(|c| !c.is_closed()) {
            return Ok(c.clone());
        }
        let mut last = Error::Address(String::new());
        // Starting with the remote that worked last.
        for i in 0..self.remotes.len() {
            let at = (slot.0 + i) % self.remotes.len();
            let remote = &self.remotes[at];
            match self.open(remote).await {
                Ok(c) => {
                    let c = Arc::new(c);
                    *slot = (at, Some(c.clone()));
                    return Ok(c);
                }
                Err(e) => {
                    tracing::debug!(%remote, error = %e, "OVN northbound remote unusable");
                    last = e;
                }
            }
        }
        Err(last)
    }

    /// The remote currently connected to, if any.
    pub async fn remote(&self) -> Option<String> {
        let slot = self.client.lock().await;
        slot.1
            .as_ref()
            .filter(|c| !c.is_closed())
            .map(|_| self.remotes[slot.0].clone())
    }

    /// Connects to `remote` if it is a standalone database or the leader
    /// of a clustered one.
    async fn open(&self, remote: &str) -> Result<Client> {
        let c = Client::connect(remote, self.tls.as_ref()).await?;
        let rows = c
            .transact(
                "_Server",
                vec![json!({"op": "select", "table": "Database",
                    "where": [["name", "==", self.db]], "columns": ["model", "leader", "connected"]})],
            )
            .await;
        let row = match &rows {
            Ok(r) => r[0]["rows"].get(0).cloned().unwrap_or(Value::Null),
            // Servers from before clustering have no _Server database.
            Err(Error::Transaction(_) | Error::Rpc(_)) => Value::Null,
            Err(_) => return Err(rows.unwrap_err()),
        };
        if row["model"] == "clustered" && !(row["leader"] == true && row["connected"] == true) {
            return Err(Error::NotLeader(remote.to_owned()));
        }
        Ok(c)
    }

    async fn tx(&self, ops: Vec<Value>) -> Result<Vec<Value>> {
        let c = self.client().await?;
        match c.transact(self.db, ops.clone()).await {
            // The connection dropped or went silent (server restarted, NB
            // leader change). Retry once on a fresh connection; every
            // transaction here is safe to repeat.
            Err(Error::Closed | Error::Timeout(_)) => {
                self.client.lock().await.1 = None;
                self.client().await?.transact(self.db, ops).await
            }
            r => r,
        }
    }

    /// Runs one transaction (ops as in RFC 7047), retried once on a fresh
    /// connection if the old one died.
    pub async fn transact(&self, ops: Vec<Value>) -> Result<Vec<Value>> {
        self.tx(ops).await
    }

    /// Every row of a table, with `_uuid` and the given columns.
    pub async fn select_all(
        &self,
        table: &str,
        columns: &[&str],
    ) -> Result<Vec<serde_json::Map<String, Value>>> {
        let mut cols: Vec<&str> = vec!["_uuid"];
        cols.extend_from_slice(columns);
        let res = self
            .tx(vec![
                json!({"op": "select", "table": table, "where": [], "columns": cols}),
            ])
            .await?;
        Ok(res[0]["rows"]
            .as_array()
            .map(|rows| rows.iter().filter_map(|r| r.as_object().cloned()).collect())
            .unwrap_or_default())
    }

    /// True once ovn-controller on the VM's host has bound the port, i.e. the
    /// VM's interface is plugged in and the network path is programmed.
    pub async fn port_up(&self, name: &str) -> Result<bool> {
        let res = self
            .tx(vec![json!({"op": "select", "table": "Logical_Switch_Port",
                "where": [["name", "==", name]], "columns": ["up"]})])
            .await?;
        Ok(res[0]["rows"][0]["up"] == json!(true)
            || res[0]["rows"][0]["up"] == json!(["set", [true]]))
    }
}
