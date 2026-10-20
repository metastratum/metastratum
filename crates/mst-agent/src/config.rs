//! The agent's configuration: one TOML file, by default
//! `/etc/metastratum/agent.toml`. Only the controllers must be given (and,
//! until the host is enrolled, a join token); unknown settings are refused.
//! Relative paths are relative to the file.
//!
//! `deploy/config/agent.toml` lists every setting.

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

pub const DEFAULT_PATH: &str = "/etc/metastratum/agent.toml";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The controllers' agent endpoints; the agent moves to the next when
    /// one is unreachable.
    pub controllers: Vec<String>,
    /// Single-use join token, needed until the host is enrolled.
    #[serde(default)]
    pub join_token: Option<String>,
    /// Address the controllers and other hosts reach this host on; by
    /// default, the one the route to the first controller leaves from.
    #[serde(default)]
    pub address: Option<IpAddr>,
    /// The host's identity: key and certificates.
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
    /// VM data: images, volumes, cloud-init seeds.
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    /// Largest image the host takes (its file, and the disk it describes), in
    /// GiB.
    #[serde(default = "default_image_max_gib")]
    pub image_max_gib: u64,
    #[serde(default = "default_libvirt_uri")]
    pub libvirt_uri: String,
    /// libvirt's socket; by default virtqemud's, or libvirtd's on
    /// monolithic installs.
    #[serde(default)]
    pub libvirt_socket: Option<PathBuf>,
}

fn default_state_dir() -> PathBuf {
    "/var/lib/metastratum-agent".into()
}

fn default_data_dir() -> PathBuf {
    "/var/lib/metastratum".into()
}

fn default_image_max_gib() -> u64 {
    64
}

fn default_libvirt_uri() -> String {
    "qemu:///system".into()
}

impl Config {
    /// Reads and checks the file at `path`.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| {
            format!(
                "reading {} (see deploy/config/agent.toml for its settings)",
                path.display()
            )
        })?;
        Self::parse(&text, path.parent().unwrap_or(Path::new(".")))
            .with_context(|| format!("in {}", path.display()))
    }

    fn parse(text: &str, dir: &Path) -> anyhow::Result<Self> {
        let mut c: Config = toml::from_str(text)?;
        if c.controllers.is_empty() {
            anyhow::bail!("controllers needs at least one agent endpoint");
        }
        c.state_dir = dir.join(&c.state_dir);
        c.data_dir = dir.join(&c.data_dir);
        c.libvirt_socket = c.libvirt_socket.map(|s| dir.join(s));
        Ok(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_file_takes_the_defaults() {
        let c = Config::parse(
            r#"controllers = ["https://ctl1.example.net:7821"]"#,
            Path::new("/etc/metastratum"),
        )
        .unwrap();
        assert_eq!(c.state_dir, Path::new("/var/lib/metastratum-agent"));
        assert_eq!(c.data_dir, Path::new("/var/lib/metastratum"));
        assert_eq!(c.image_max_gib, 64);
        assert!(c.join_token.is_none() && c.address.is_none());
    }

    #[test]
    fn mistakes_are_refused() {
        let dir = Path::new("/etc/metastratum");
        assert!(Config::parse("controllers = []", dir).is_err());
        let typo = "controllers = [\"https://c:7821\"]\njoin_tokn = \"x\"";
        assert!(format!("{:#}", Config::parse(typo, dir).unwrap_err()).contains("join_tokn"));
    }

    #[test]
    fn the_example_file_is_valid() {
        let example = include_str!("../../../deploy/config/agent.toml");
        Config::parse(example, Path::new("/etc/metastratum")).unwrap();
    }
}
