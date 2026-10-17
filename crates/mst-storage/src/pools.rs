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

    /// Writes a copy of a volume to `dest` as a qcow2 file of its own.
    pub async fn export_volume(&self, id: &str, dest: &Path) -> Result<()> {
        match self {
            Pool::File(p) => p.export_volume(id, dest).await,
            Pool::Rbd(p) => p.export_volume(id, dest).await,
        }
    }

    /// Creates a volume from a qcow2 file, of at least `size` bytes.
    pub async fn import_volume(&self, id: &str, size: u64, src: &Path) -> Result<()> {
        match self {
            Pool::File(p) => p.import_volume(id, size, src).await,
            Pool::Rbd(p) => p.import_volume(id, size, src).await,
        }
    }

    /// The file a volume is in, for pools of files: what `qemu-nbd` serves
    /// for another host to copy.
    pub fn volume_file(&self, id: &str) -> Result<Option<PathBuf>> {
        match self {
            Pool::File(p) => p.volume_path(id).map(Some),
            Pool::Rbd(_) => Ok(None),
        }
    }

    /// Creates a volume as a copy of another; see each pool's
    /// `clone_volume`.
    pub async fn clone_volume(
        &self,
        source: &str,
        id: &str,
        size: u64,
        image: Option<&str>,
    ) -> Result<()> {
        match self {
            Pool::File(p) => p.clone_volume(source, id, size, image).await,
            Pool::Rbd(p) => p.clone_volume(source, id, size).await,
        }
    }

    /// Grows a volume no VM is using; see each pool's `grow_volume`.
    pub async fn grow_volume(&self, id: &str, size: u64) -> Result<()> {
        match self {
            Pool::File(p) => p.grow_volume(id, size).await,
            Pool::Rbd(p) => p.grow_volume(id, size).await,
        }
    }

    pub async fn delete_image(&self, id: &str) -> Result<()> {
        match self {
            Pool::File(p) => p.delete_image(id).await,
            Pool::Rbd(p) => p.delete_image(id).await,
        }
    }

    /// Removes metastratum's data from the pool's storage. Returns how many
    /// images and volumes went.
    pub async fn purge(&self) -> Result<usize> {
        match self {
            Pool::File(p) => p.purge().await,
            Pool::Rbd(p) => p.purge().await,
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

    /// Lets go of a shared pool on this host: unmounts it and removes its
    /// directory (an NFS mountpoint, or an RBD pool's keyring). Leaves the
    /// storage itself alone.
    pub async fn forget(&self, id: &str) -> Result<()> {
        self.ready.lock().await.remove(id);
        let dir = self.mountpoint(&PoolSpec {
            id: id.to_owned(),
            kind: String::new(),
            config: serde_json::Value::Null,
        })?;
        if is_mounted(&dir) {
            let target = dir.to_string_lossy();
            tracing::info!(pool = id, "unmounting pool");
            run("umount", &[&target]).await?;
        }
        // Never remove anything across a mount we could not undo.
        if is_mounted(&dir) {
            return Err(Error::Pool(format!("{} is still mounted", dir.display())));
        }
        match tokio::fs::remove_dir_all(&dir).await {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// IDs of the shared pools this host has set up (has a directory for).
    pub async fn attached(&self) -> Result<Vec<String>> {
        let mut out = vec![];
        let mut dir = match tokio::fs::read_dir(self.data_dir.join("pools")).await {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e.into()),
        };
        while let Some(e) = dir.next_entry().await? {
            let id = e.file_name().to_string_lossy().into_owned();
            if !id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
                out.push(id);
            }
        }
        Ok(out)
    }

    /// The local pool's cache and files, for cleaning up after images.
    pub fn local(&self) -> FilePool {
        FilePool::new(&self.data_dir)
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
    let probe = root.join(".metastratum-probe");
    tokio::fs::write(&probe, b"ok")
        .await
        .map_err(|e| Error::Pool(format!("{} is not writable: {e}", root.display())))?;
    let _ = tokio::fs::remove_file(&probe).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cleaning up removes metastratum's files and nothing else.
    #[tokio::test]
    async fn pools_are_purged_and_forgotten_but_foreign_files_stay() {
        let dir = std::env::temp_dir().join(format!("mst-purge-{}", std::process::id()));
        let pools = Pools::new(&dir);
        let id = "01a14668-2b2e-7154-83c1-27608f10c018";
        // An attached pool's directory, as for an RBD keyring.
        let pool_dir = dir.join("pools").join(id);
        std::fs::create_dir_all(pool_dir.join("tmp")).unwrap();
        std::fs::write(pool_dir.join("keyring"), "k").unwrap();
        assert_eq!(pools.attached().await.unwrap(), [id]);

        let files = FilePool::new(dir.join("shared"));
        files.init().await.unwrap();
        let image = dir.join("shared/images/0199f0a2-7b3c-7d00-8000-000000000001.qcow2");
        std::fs::write(&image, "x").unwrap();
        std::fs::write(
            dir.join("shared/volumes/0199f0a2-7b3c-7d00-8000-000000000002.qcow2"),
            "x",
        )
        .unwrap();
        std::fs::write(dir.join("shared/images/notes.txt"), "someone else's").unwrap();
        let cached = files.cached_images().await.unwrap();
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].0, "0199f0a2-7b3c-7d00-8000-000000000001");

        files
            .delete_image("0199f0a2-7b3c-7d00-8000-000000000001")
            .await
            .unwrap();
        assert!(!image.exists());
        files
            .delete_image("0199f0a2-7b3c-7d00-8000-000000000001")
            .await
            .unwrap();
        assert_eq!(files.purge().await.unwrap(), 1);
        assert!(
            dir.join("shared/images/notes.txt").exists(),
            "not ours: kept"
        );
        assert!(!dir.join("shared/volumes").exists(), "emptied: removed");

        pools.forget(id).await.unwrap();
        assert!(!pool_dir.exists());
        assert!(pools.attached().await.unwrap().is_empty());
        assert!(pools.forget("../etc").await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn local_pools_are_prepared_and_cached() {
        let dir = std::env::temp_dir().join(format!("mst-pools-{}", std::process::id()));
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
