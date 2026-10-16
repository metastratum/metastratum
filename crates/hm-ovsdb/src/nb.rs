//! A northbound database connection. What to write is decided by
//! `hm-netplan`; this only reads rows and runs transactions.

use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::client::{Client, Error, Result};

const DB: &str = "OVN_Northbound";

/// Geneve adds 58 bytes (IPv4 underlay with options); this keeps guests
/// within a standard 1500-byte underlay.
pub const DEFAULT_MTU: u32 = 1442;

/// A northbound connection that reconnects lazily after failures.
pub struct Northbound {
    addr: String,
    client: Mutex<Option<Arc<Client>>>,
}

impl Northbound {
    pub fn new(addr: impl Into<String>) -> Self {
        Self {
            addr: addr.into(),
            client: Mutex::new(None),
        }
    }

    async fn client(&self) -> Result<Arc<Client>> {
        let mut slot = self.client.lock().await;
        if let Some(c) = slot.as_ref().filter(|c| !c.is_closed()) {
            return Ok(c.clone());
        }
        let c = Arc::new(Client::connect(&self.addr).await?);
        *slot = Some(c.clone());
        Ok(c)
    }

    async fn tx(&self, ops: Vec<Value>) -> Result<Vec<Value>> {
        let c = self.client().await?;
        match c.transact(DB, ops.clone()).await {
            // The connection dropped or went silent (server restarted, NB
            // leader change). Retry once on a fresh connection; every
            // transaction here is safe to repeat.
            Err(Error::Closed | Error::Timeout(_)) => {
                *self.client.lock().await = None;
                self.client().await?.transact(DB, ops).await
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
