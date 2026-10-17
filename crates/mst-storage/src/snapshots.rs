//! Snapshots of volumes, kept in the volumes themselves: qcow2 internal
//! snapshots in file pools, RBD snapshots in Ceph. Named `mst-<id>`.
//!
//! These work on volumes no VM is using; a running VM's file volumes are
//! snapshotted through QEMU instead (the agent does that), under the same
//! names.

use serde::Deserialize;

use crate::{Error, FilePool, Pool, RbdPool, Result, run, safe_id};

/// The name a snapshot has in a volume.
pub fn snapshot_name(id: &str) -> Result<String> {
    Ok(format!("mst-{}", safe_id(id)?))
}

#[derive(Deserialize)]
struct Named {
    name: String,
}

impl FilePool {
    /// The names of a volume's snapshots. Readable while a VM uses it.
    pub async fn snapshots(&self, id: &str) -> Result<Vec<String>> {
        #[derive(Deserialize)]
        struct Info {
            #[serde(default)]
            snapshots: Vec<Named>,
        }
        let path = self.volume_path(id)?;
        let p = path.to_string_lossy();
        let out = run(
            "qemu-img",
            &["info", "--output=json", "-U", "-f", "qcow2", &p],
        )
        .await?;
        let info: Info =
            serde_json::from_slice(&out).map_err(|e| Error::InvalidImage(e.to_string()))?;
        Ok(info.snapshots.into_iter().map(|s| s.name).collect())
    }

    async fn snapshot_cmd(&self, op: &str, id: &str, snap: &str) -> Result<()> {
        let path = self.volume_path(id)?;
        let (p, name) = (path.to_string_lossy(), snapshot_name(snap)?);
        // No `-f`: older qemu-img (8.x) does not take it here. The volumes
        // are qcow2 files metastratum made, which is what it finds.
        run("qemu-img", &["snapshot", "-q", op, &name, &p]).await?;
        Ok(())
    }

    pub async fn create_snapshot(&self, id: &str, snap: &str) -> Result<()> {
        if self.snapshots(id).await?.contains(&snapshot_name(snap)?) {
            return Ok(());
        }
        self.snapshot_cmd("-c", id, snap).await
    }

    pub async fn revert_snapshot(&self, id: &str, snap: &str) -> Result<()> {
        self.snapshot_cmd("-a", id, snap).await
    }

    pub async fn delete_snapshot(&self, id: &str, snap: &str) -> Result<()> {
        if !self.snapshots(id).await?.contains(&snapshot_name(snap)?) {
            return Ok(());
        }
        self.snapshot_cmd("-d", id, snap).await
    }
}

impl RbdPool {
    /// The names of a volume's snapshots.
    pub async fn snapshots(&self, id: &str) -> Result<Vec<String>> {
        let vol = self.spec(&Self::volume_name(id)?);
        let out = self.rbd(&["snap", "ls", "--format", "json", &vol]).await?;
        let list: Vec<Named> =
            serde_json::from_slice(&out).map_err(|e| Error::Pool(e.to_string()))?;
        Ok(list.into_iter().map(|s| s.name).collect())
    }

    fn snap_spec(&self, id: &str, snap: &str) -> Result<String> {
        Ok(format!(
            "{}@{}",
            self.spec(&Self::volume_name(id)?),
            snapshot_name(snap)?
        ))
    }

    /// Takes a snapshot; safe while a VM uses the volume (librbd flushes
    /// its cache for it), though only crash-consistent unless the guest's
    /// file systems are frozen.
    pub async fn create_snapshot(&self, id: &str, snap: &str) -> Result<()> {
        if self.snapshots(id).await?.contains(&snapshot_name(snap)?) {
            return Ok(());
        }
        self.rbd(&[
            "snap",
            "create",
            "--no-progress",
            &self.snap_spec(id, snap)?,
        ])
        .await?;
        Ok(())
    }

    pub async fn revert_snapshot(&self, id: &str, snap: &str) -> Result<()> {
        self.rbd(&[
            "snap",
            "rollback",
            "--no-progress",
            &self.snap_spec(id, snap)?,
        ])
        .await?;
        Ok(())
    }

    pub async fn delete_snapshot(&self, id: &str, snap: &str) -> Result<()> {
        if !self.snapshots(id).await?.contains(&snapshot_name(snap)?) {
            return Ok(());
        }
        self.rbd(&["snap", "rm", "--no-progress", &self.snap_spec(id, snap)?])
            .await?;
        Ok(())
    }
}

impl Pool {
    pub async fn snapshots(&self, id: &str) -> Result<Vec<String>> {
        match self {
            Pool::File(p) => p.snapshots(id).await,
            Pool::Rbd(p) => p.snapshots(id).await,
        }
    }

    pub async fn create_snapshot(&self, id: &str, snap: &str) -> Result<()> {
        match self {
            Pool::File(p) => p.create_snapshot(id, snap).await,
            Pool::Rbd(p) => p.create_snapshot(id, snap).await,
        }
    }

    pub async fn revert_snapshot(&self, id: &str, snap: &str) -> Result<()> {
        match self {
            Pool::File(p) => p.revert_snapshot(id, snap).await,
            Pool::Rbd(p) => p.revert_snapshot(id, snap).await,
        }
    }

    pub async fn delete_snapshot(&self, id: &str, snap: &str) -> Result<()> {
        match self {
            Pool::File(p) => p.delete_snapshot(id, snap).await,
            Pool::Rbd(p) => p.delete_snapshot(id, snap).await,
        }
    }
}
