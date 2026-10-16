//! Pools as the controller describes them, prepared on first use.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use tokio::sync::Mutex;

use crate::rbd::{RbdConfig, RbdPool};
use crate::{Error, FilePool, Result, run};

/// A pool description from the controller.
#[derive(Clone, Debug)]
pub struct PoolSpec {
    pub id: String,
    /// `local`, `nfs` or `rbd`.
    pub kind: String,
    pub config: serde_json::Value,
}

#[derive(Deserialize)]
struct NfsConfig {
    server: String,
    export: String,
    #[serde(default = "default_nfs_options")]
    options: String,
}

fn default_nfs_options() -> String {
    "vers=4.2,hard,noatime".into()
}

/// A prepared pool.
#[derive(Clone)]
pub enum Pool {
    File(FilePool),
    Rbd(RbdPool),
}

impl Pool {
    pub async fn ensure_image(
        &self,
        id: &str,
        url: &str,
        format: &str,
        policy: crate::FetchPolicy,
    ) -> Result<()> {
        match self {
            Pool::File(p) => p.ensure_image(id, url, format, policy).await,
            Pool::Rbd(p) => p.ensure_image(id, url, format, policy).await,
        }
    }

    pub async fn ensure_volume(&self, id: &str, size: u64, image: Option<&str>) -> Result<()> {
        match self {
            Pool::File(p) => p.ensure_volume(id, size, image).await,
            Pool::Rbd(p) => p.ensure_volume(id, size, image).await,
        }
    }

    pub async fn delete_volume(&self, id: &str) -> Result<()> {
        match self {
            Pool::File(p) => p.delete_volume(id).await,
            Pool::Rbd(p) => p.delete_volume(id).await,
        }
    }
}

/// The agent's pools, prepared (mounted, ...) on first use and cached.
pub struct Pools {
    data_dir: PathBuf,
    ready: Mutex<HashMap<String, Pool>>,
}

impl Pools {
    /// `data_dir` is the local pool's directory; shared pools are mounted
    /// below `data_dir/pools/`.
    pub fn new(data_dir: impl Into<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            data_dir: data_dir.into(),
            ready: Mutex::default(),
        })
    }

    pub async fn get(&self, spec: &PoolSpec) -> Result<Pool> {
        let mut ready = self.ready.lock().await;
        if let Some(p) = ready.get(&spec.id)
            && self.still_ready(spec).await
        {
            return Ok(p.clone());
        }
        let pool = match spec.kind.as_str() {
            "local" => {
                let p = FilePool::new(&self.data_dir);
                p.init().await?;
                Pool::File(p)
            }
            "nfs" => {
                let cfg: NfsConfig = serde_json::from_value(spec.config.clone())
                    .map_err(|e| Error::Pool(format!("NFS config: {e}")))?;
                let mountpoint = self.mountpoint(spec)?;
                mount_nfs(&cfg, &mountpoint).await?;
                let p = FilePool::new(&mountpoint);
                p.init().await?;
                probe_writable(&mountpoint).await?;
                Pool::File(p)
            }
            "rbd" => {
                let cfg: RbdConfig = serde_json::from_value(spec.config.clone())
                    .map_err(|e| Error::Pool(format!("RBD config: {e}")))?;
                Pool::Rbd(RbdPool::new(cfg, &self.mountpoint(spec)?).await?)
            }
            other => return Err(Error::Pool(format!("pool kind {other} is not supported"))),
        };
        ready.insert(spec.id.clone(), pool.clone());
        Ok(pool)
    }

    fn mountpoint(&self, spec: &PoolSpec) -> Result<PathBuf> {
        let id = &spec.id;
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
            return Err(Error::Pool(format!("bad pool id {id:?}")));
        }
        Ok(self.data_dir.join("pools").join(id))
    }

    /// Shared pools can disappear underneath us (unmounted by hand).
    async fn still_ready(&self, spec: &PoolSpec) -> bool {
        match spec.kind.as_str() {
            "nfs" => self.mountpoint(spec).is_ok_and(|m| is_mounted(&m)),
            _ => true,
        }
    }
}

fn is_mounted(path: &Path) -> bool {
    let Ok(mounts) = std::fs::read_to_string("/proc/self/mounts") else {
        return false;
    };
    let want = path.to_string_lossy();
    // Paths in /proc/self/mounts escape spaces as \040; ours have none.
    mounts
        .lines()
        .any(|l| l.split_whitespace().nth(1) == Some(&*want))
}

async fn mount_nfs(cfg: &NfsConfig, mountpoint: &Path) -> Result<()> {
    if is_mounted(mountpoint) {
        return Ok(());
    }
    tokio::fs::create_dir_all(mountpoint).await?;
    let source = format!("{}:{}", cfg.server, cfg.export);
    let target = mountpoint.to_string_lossy();
    tracing::info!(%source, target = %target, "mounting NFS pool");
    run(
        "mount",
        &["-t", "nfs", "-o", &cfg.options, &source, &target],
    )
    .await?;
    Ok(())
}

/// Fails early, with a clear error, if the pool is mounted read-only or the
/// export squashes root.
async fn probe_writable(root: &Path) -> Result<()> {
    let probe = root.join(".hypermon-probe");
    tokio::fs::write(&probe, b"ok")
        .await
        .map_err(|e| Error::Pool(format!("{} is not writable: {e}", root.display())))?;
    let _ = tokio::fs::remove_file(&probe).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_pools_are_prepared_and_cached() {
        let dir = std::env::temp_dir().join(format!("hm-pools-{}", std::process::id()));
        let pools = Pools::new(&dir);
        let spec = PoolSpec {
            id: "00000000-0000-0000-0000-000000000001".into(),
            kind: "local".into(),
            config: serde_json::json!({}),
        };
        let Pool::File(p) = pools.get(&spec).await.unwrap() else {
            panic!("local pools are file pools")
        };
        assert!(p.volume_path("v1").unwrap().starts_with(&dir));
        assert!(dir.join("images").is_dir() && dir.join("volumes").is_dir());

        let bad = PoolSpec {
            id: "../x".into(),
            kind: "nfs".into(),
            config: serde_json::json!({"server": "s", "export": "/e"}),
        };
        assert!(pools.get(&bad).await.is_err());
        let unknown = PoolSpec {
            id: "abc".into(),
            kind: "zfs".into(),
            config: serde_json::json!({}),
        };
        assert!(matches!(pools.get(&unknown).await, Err(Error::Pool(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
