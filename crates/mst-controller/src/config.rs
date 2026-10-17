//! The controller's configuration: one TOML file, by default
//! `/etc/metastratum/controller.toml`. Only the database and the OVN
//! northbound database must be given; everything else has a default, and
//! unknown settings are refused (a typo must not pass silently). Relative
//! paths are relative to the file, so a configuration passed in as a systemd
//! credential finds the master key beside it.
//!
//! `deploy/config/controller.toml` lists every setting.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

use crate::ratelimit::Cidr;

pub const DEFAULT_PATH: &str = "/etc/metastratum/controller.toml";
/// Where packages put the web UI.
const PACKAGED_WEB_DIR: &str = "/usr/share/metastratum/web";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Postgres connection URL.
    pub database_url: String,
    /// The master key sealing secrets in the database.
    #[serde(default = "default_master_key_file")]
    pub master_key_file: PathBuf,
    /// The built web UI, served at `/`.
    #[serde(default)]
    pub web_dir: Option<PathBuf>,
    /// Address for Prometheus metrics (`/metrics`); none by default.
    #[serde(default)]
    pub metrics_listen: Option<SocketAddr>,
    #[serde(default)]
    pub api: Api,
    #[serde(default)]
    pub agents: Agents,
    pub ovn: Ovn,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Api {
    /// Address for the HTTP API and web UI.
    #[serde(default = "default_api_listen")]
    pub listen: SocketAddr,
    /// Load balancers in front of the API, whose X-Forwarded-* headers are
    /// believed.
    #[serde(default)]
    pub trusted_proxies: Vec<Cidr>,
    /// Identity provider issuers sub-tenants may use on internal addresses.
    #[serde(default)]
    pub trusted_oidc_issuers: Vec<String>,
    /// Redis to share rate limits between replicas.
    #[serde(default)]
    pub redis_url: Option<String>,
}

impl Default for Api {
    fn default() -> Self {
        Self {
            listen: default_api_listen(),
            trusted_proxies: vec![],
            trusted_oidc_issuers: vec![],
            redis_url: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Agents {
    /// Address for the agent endpoint (gRPC over mutual TLS).
    #[serde(default = "default_agent_listen")]
    pub listen: SocketAddr,
    /// Where other replicas reach this one's agent endpoint. Worked out by
    /// default (see [`Config::peer_address`]).
    #[serde(default)]
    pub peer_address: Option<String>,
}

impl Default for Agents {
    fn default() -> Self {
        Self {
            listen: default_agent_listen(),
            peer_address: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ovn {
    /// Northbound database servers (`ssl:HOST:PORT`, `tcp:...`, `unix:...`);
    /// with several, the cluster's leader is used.
    pub northbound: Vec<String>,
    /// Southbound database servers for hosts' ovn-controller, `ssl:` only.
    #[serde(default)]
    pub southbound: Vec<String>,
}

fn default_master_key_file() -> PathBuf {
    "master.key".into()
}

fn default_api_listen() -> SocketAddr {
    ([127, 0, 0, 1], 8080).into()
}

fn default_agent_listen() -> SocketAddr {
    ([0, 0, 0, 0], 8443).into()
}

impl Config {
    /// Reads and checks the file at `path`.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| {
            format!(
                "reading {} (see deploy/config/controller.toml for its settings)",
                path.display()
            )
        })?;
        Self::parse(&text, path.parent().unwrap_or(Path::new(".")))
            .with_context(|| format!("in {}", path.display()))
    }

    /// Parses a configuration, resolving relative paths against `dir`.
    fn parse(text: &str, dir: &Path) -> anyhow::Result<Self> {
        let mut c: Config = toml::from_str(text)?;
        c.master_key_file = dir.join(&c.master_key_file);
        c.web_dir = match c.web_dir.take() {
            Some(d) => Some(dir.join(d)),
            None => Some(PathBuf::from(PACKAGED_WEB_DIR)).filter(|d| d.is_dir()),
        };
        if c.ovn.northbound.is_empty() {
            anyhow::bail!("ovn.northbound needs at least one server");
        }
        if let Some(r) = c.ovn.southbound.iter().find(|r| !r.starts_with("ssl:")) {
            anyhow::bail!(
                "ovn.southbound takes ssl: servers only (OVN's RBAC works over TLS), not {r}"
            );
        }
        Ok(c)
    }

    /// The northbound servers as the OVSDB client takes them.
    pub fn northbound(&self) -> String {
        self.ovn.northbound.join(",")
    }

    /// The southbound servers handed to agents, comma-separated; none means
    /// hosts keep the remote they were set up with.
    pub fn southbound(&self) -> Option<String> {
        Some(self.ovn.southbound.join(",")).filter(|s| !s.is_empty())
    }

    /// Where other replicas reach this one's agent endpoint: as configured;
    /// else the listen address when it names one; else the address this
    /// machine reaches the database from (replicas share it), or failing
    /// that its host name.
    pub fn peer_address(&self, hostname: &str) -> String {
        if let Some(a) = &self.agents.peer_address {
            return a.clone();
        }
        let listen = self.agents.listen;
        if !listen.ip().is_unspecified() {
            return listen.to_string();
        }
        match self.address_towards_database() {
            Some(ip) => SocketAddr::new(ip, listen.port()).to_string(),
            None => format!("{hostname}:{}", listen.port()),
        }
    }

    /// The local address the route to the database's host leaves from; none
    /// for a database on this machine.
    fn address_towards_database(&self) -> Option<IpAddr> {
        let opts: mst_db::sqlx::postgres::PgConnectOptions = self.database_url.parse().ok()?;
        let host = opts.get_host();
        if host.starts_with('/') {
            return None;
        }
        let socket = std::net::UdpSocket::bind(("::", 0))
            .or_else(|_| std::net::UdpSocket::bind(("0.0.0.0", 0)))
            .ok()?;
        // Connecting a UDP socket sends nothing; it only picks the route.
        socket.connect((host, opts.get_port())).ok()?;
        let ip = match socket.local_addr().ok()?.ip() {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
            v4 => v4,
        };
        Some(ip).filter(|ip| !ip.is_loopback())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
database_url = "postgres://mst:secret@db.example.net/metastratum"
[ovn]
northbound = ["ssl:10.0.0.31:6641"]
"#;

    #[test]
    fn minimal_file_takes_the_defaults() {
        let c = Config::parse(MINIMAL, Path::new("/etc/metastratum")).unwrap();
        assert_eq!(c.master_key_file, Path::new("/etc/metastratum/master.key"));
        assert_eq!(c.api.listen.to_string(), "127.0.0.1:8080");
        assert_eq!(c.agents.listen.to_string(), "0.0.0.0:8443");
        assert_eq!(c.northbound(), "ssl:10.0.0.31:6641");
        assert_eq!(c.southbound(), None);
        assert!(c.metrics_listen.is_none() && c.api.redis_url.is_none());
    }

    #[test]
    fn mistakes_are_refused() {
        let dir = Path::new("/etc/metastratum");
        // A typo is an error, not a silently ignored setting.
        let typo = format!("{MINIMAL}\n[api]\nlistn = \"0.0.0.0:8080\"\n");
        let err = format!("{:#}", Config::parse(&typo, dir).unwrap_err());
        assert!(err.contains("listn"), "{err}");
        let no_ovn = r#"database_url = "postgres://db/mst""#;
        assert!(Config::parse(no_ovn, dir).is_err());
        let plain_sb = format!("{MINIMAL}southbound = [\"tcp:10.0.0.31:6642\"]\n");
        assert!(Config::parse(&plain_sb, dir).is_err());
        let bad_proxy = format!("{MINIMAL}\n[api]\ntrusted_proxies = [\"10.0.0.0/33\"]\n");
        assert!(Config::parse(&bad_proxy, dir).is_err());
    }

    #[test]
    fn peer_address_is_worked_out() {
        let dir = Path::new("/etc/metastratum");
        let at = |extra: &str, db: &str| {
            let text =
                format!("database_url = \"{db}\"\n{extra}\n[ovn]\nnorthbound = [\"tcp:x:1\"]\n");
            Config::parse(&text, dir).unwrap().peer_address("ctl1")
        };
        assert_eq!(
            at(
                "[agents]\npeer_address = \"10.0.0.21:8443\"",
                "postgres://db/m"
            ),
            "10.0.0.21:8443"
        );
        assert_eq!(
            at("[agents]\nlisten = \"10.0.0.21:9443\"", "postgres://db/m"),
            "10.0.0.21:9443"
        );
        // A database on this machine says nothing about the network.
        assert_eq!(at("", "postgres://localhost/m"), "ctl1:8443");
        // A remote one: the address the route to it leaves from.
        let towards = at("", "postgres://192.0.2.10/m");
        assert!(
            towards.ends_with(":8443") && !towards.starts_with("ctl1"),
            "{towards}"
        );
    }

    #[test]
    fn the_dev_file_is_valid() {
        let dev = include_str!("../../../deploy/dev/controller.toml");
        let c = Config::parse(dev, Path::new("deploy/dev")).unwrap();
        assert_eq!(
            c.web_dir.as_deref(),
            Some(Path::new("deploy/dev/../../web/dist"))
        );
    }

    #[test]
    fn the_example_file_is_valid() {
        let example = include_str!("../../../deploy/config/controller.toml");
        let c = Config::parse(example, Path::new("/etc/metastratum")).unwrap();
        assert!(!c.ovn.northbound.is_empty());
    }
}
