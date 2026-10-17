//! Storage for the metastratum agent.
//!
//! File pools are directories of qcow2 volumes, each a copy-on-write overlay
//! of an image cached in the same pool. The local pool is a directory on the
//! host; an NFS pool is the same layout on a share mounted by the agent, so
//! every host attached to it sees the same images and volumes. [`Pools`]
//! prepares pools on demand from the descriptions the controller sends.

mod fetch;
mod pools;
mod rbd;
mod snapshots;

pub use fetch::{FetchPolicy, PublicOnly, check_url, is_public};

pub use pools::{Pool, PoolSpec, Pools};
pub use rbd::RbdPool;
pub use snapshots::snapshot_name;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use tokio::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("download failed: {0}")]
    Download(String),
    #[error("{tool} failed: {stderr}")]
    Tool { tool: &'static str, stderr: String },
    #[error("invalid image: {0}")]
    InvalidImage(String),
    #[error("image {0} is not cached on this host")]
    MissingImage(String),
    #[error("requested size {requested} is smaller than the image ({image} bytes)")]
    TooSmall { requested: u64, image: u64 },
    #[error("invalid pool: {0}")]
    Pool(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// IDs come from the controller; make sure they cannot escape the pool.
fn safe_id(id: &str) -> Result<&str> {
    if !id.is_empty()
        && id.len() <= 64
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        Ok(id)
    } else {
        Err(Error::InvalidImage(format!("bad identifier {id:?}")))
    }
}

#[derive(Deserialize)]
pub(crate) struct ImgInfo {
    pub(crate) format: String,
    #[serde(rename = "virtual-size")]
    pub(crate) virtual_size: u64,
    /// Checked by the tests (volumes' backing images).
    #[cfg_attr(not(test), allow(dead_code))]
    #[serde(rename = "backing-filename")]
    pub(crate) backing_filename: Option<String>,
}

pub(crate) async fn run(tool: &'static str, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new(tool).args(args).output().await?;
    if !out.status.success() {
        return Err(Error::Tool {
            tool,
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        });
    }
    Ok(out.stdout)
}

/// Converts `src` to `dest` (written under a temporary name, then renamed).
pub(crate) async fn convert(
    src: &Path,
    src_format: &str,
    dest: &Path,
    dest_format: &str,
) -> Result<()> {
    let tmp = dest.with_extension(format!("{}.part", random_suffix()));
    let (s, t) = (src.to_string_lossy(), tmp.to_string_lossy());
    let result = run(
        "qemu-img",
        &["convert", "-q", "-f", src_format, "-O", dest_format, &s, &t],
    )
    .await;
    if let Err(e) = result {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    tokio::fs::rename(&tmp, dest).await?;
    Ok(())
}

pub(crate) async fn img_info(path: &Path, format: Option<&str>) -> Result<ImgInfo> {
    let p = path.to_string_lossy();
    let mut args = vec!["info", "--output=json"];
    if let Some(f) = format {
        args.extend(["-f", f]);
    }
    args.push(&p);
    let out = run("qemu-img", &args).await?;
    serde_json::from_slice(&out).map_err(|e| Error::InvalidImage(e.to_string()))
}

#[derive(Clone)]
pub struct FilePool {
    root: PathBuf,
    /// Serialises work per image so concurrent VM creations download once.
    image_locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

impl FilePool {
    /// `root` holds `images/` (cached base images) and `volumes/`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            image_locks: Arc::default(),
        }
    }

    /// The pool's directory (a backup repository keeps `backups/` here).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Writes a copy of a volume to `dest` as a qcow2 file of its own, with
    /// its image's data in it.
    pub async fn export_volume(&self, id: &str, dest: &Path) -> Result<()> {
        convert(&self.volume_path(id)?, "qcow2", dest, "qcow2").await
    }

    /// Creates a volume from a qcow2 file (a backup's disk), of at least
    /// `size` bytes. An existing volume is left alone.
    pub async fn import_volume(&self, id: &str, size: u64, src: &Path) -> Result<()> {
        let dest = self.volume_path(id)?;
        if tokio::fs::try_exists(&dest).await? {
            return Ok(());
        }
        let tmp = dest.with_extension(format!("{}.tmp", random_suffix()));
        convert(src, "qcow2", &tmp, "qcow2").await?;
        if size > img_info(&tmp, Some("qcow2")).await?.virtual_size {
            let (t, sz) = (tmp.to_string_lossy().into_owned(), size.to_string());
            run("qemu-img", &["resize", "-q", "-f", "qcow2", &t, &sz]).await?;
        }
        tokio::fs::rename(&tmp, &dest).await?;
        Ok(())
    }

    pub async fn init(&self) -> Result<()> {
        for dir in ["images", "volumes"] {
            tokio::fs::create_dir_all(self.root.join(dir)).await?;
        }
        Ok(())
    }

    pub fn image_path(&self, id: &str) -> Result<PathBuf> {
        Ok(self
            .root
            .join("images")
            .join(format!("{}.qcow2", safe_id(id)?)))
    }

    pub fn volume_path(&self, id: &str) -> Result<PathBuf> {
        Ok(self
            .root
            .join("volumes")
            .join(format!("{}.qcow2", safe_id(id)?)))
    }

    fn image_lock(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.image_locks
            .lock()
            .unwrap()
            .entry(id.to_owned())
            .or_default()
            .clone()
    }

    /// Downloads an image into the cache unless already present, converted
    /// to qcow2. See [`fetch`] for what downloads may not do.
    pub async fn ensure_image(
        &self,
        id: &str,
        url: &str,
        format: &str,
        policy: FetchPolicy,
    ) -> Result<()> {
        let dest = self.image_path(id)?;
        let lock = self.image_lock(id);
        let _guard = lock.lock().await;
        if tokio::fs::try_exists(&dest).await? {
            return Ok(());
        }
        // Hosts sharing a pool may fetch the same image at once: each works
        // on its own temporary files and the first to finish wins.
        let unique = format!("{}.{}", std::process::id(), random_suffix());
        let part = dest.with_extension(format!("{unique}.part"));
        let converted = dest.with_extension(format!("{unique}.tmp"));
        let result = async {
            fetch(url, format, &part, &converted, "qcow2", policy).await?;
            set_mode(&converted, 0o644).await?;
            // Another host finished first: keep its copy, which running VMs
            // may already use as a backing file.
            if !tokio::fs::try_exists(&dest).await? {
                tokio::fs::rename(&converted, &dest).await?;
            }
            Ok(())
        }
        .await;
        let _ = tokio::fs::remove_file(&part).await;
        let _ = tokio::fs::remove_file(&converted).await;
        if result.is_ok() {
            tracing::info!(image = id, path = %dest.display(), "image cached");
        }
        result
    }

    /// Creates a volume, as an overlay of `image` if given. Existing volumes
    /// are left alone.
    pub async fn ensure_volume(&self, id: &str, size: u64, image: Option<&str>) -> Result<()> {
        let dest = self.volume_path(id)?;
        if tokio::fs::try_exists(&dest).await? {
            return Ok(());
        }
        let tmp = dest.with_extension(format!("{}.tmp", random_suffix()));
        let (tmp_s, size_s) = (tmp.to_string_lossy().into_owned(), size.to_string());
        match image {
            Some(img) => {
                let base = self.image_path(img)?;
                if !tokio::fs::try_exists(&base).await? {
                    return Err(Error::MissingImage(img.into()));
                }
                let info = img_info(&base, Some("qcow2")).await?;
                if size < info.virtual_size {
                    return Err(Error::TooSmall {
                        requested: size,
                        image: info.virtual_size,
                    });
                }
                let base_s = base.to_string_lossy();
                run(
                    "qemu-img",
                    &[
                        "create", "-q", "-f", "qcow2", "-F", "qcow2", "-b", &base_s, &tmp_s,
                        &size_s,
                    ],
                )
                .await?;
            }
            None => {
                run(
                    "qemu-img",
                    &["create", "-q", "-f", "qcow2", &tmp_s, &size_s],
                )
                .await?;
            }
        }
        tokio::fs::rename(&tmp, &dest).await?;
        Ok(())
    }

    pub async fn delete_volume(&self, id: &str) -> Result<()> {
        match tokio::fs::remove_file(self.volume_path(id)?).await {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// Removes a cached image. Volumes built on it would break: the
    /// controller only deletes images no volume uses.
    pub async fn delete_image(&self, id: &str) -> Result<()> {
        let _guard = self.image_lock(id).lock_owned().await;
        match tokio::fs::remove_file(self.image_path(id)?).await {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// IDs of the cached images, with when each was cached.
    pub async fn cached_images(&self) -> Result<Vec<(String, std::time::SystemTime)>> {
        let mut out = vec![];
        let mut dir = match tokio::fs::read_dir(self.root.join("images")).await {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e.into()),
        };
        while let Some(e) = dir.next_entry().await? {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(id) = name.strip_suffix(".qcow2")
                && safe_id(id).is_ok()
            {
                out.push((id.to_owned(), e.metadata().await?.modified()?));
            }
        }
        Ok(out)
    }

    /// Removes everything of metastratum's from the pool: cached images, and
    /// any volumes left (the controller removes pools that hold none). Only
    /// metastratum's own files go (`<id>.qcow2` in `images/` and `volumes/`);
    /// the directories go too if that leaves them empty. Returns how many
    /// files were removed.
    pub async fn purge(&self) -> Result<usize> {
        let mut removed = 0;
        for sub in ["images", "volumes"] {
            let dir_path = self.root.join(sub);
            let mut dir = match tokio::fs::read_dir(&dir_path).await {
                Ok(d) => d,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            while let Some(e) = dir.next_entry().await? {
                let name = e.file_name().to_string_lossy().into_owned();
                if name
                    .strip_suffix(".qcow2")
                    .is_some_and(|id| safe_id(id).is_ok())
                    && e.file_type().await?.is_file()
                {
                    tokio::fs::remove_file(e.path()).await?;
                    removed += 1;
                }
            }
            // Only if empty: anything else there is not ours.
            let _ = tokio::fs::remove_dir(&dir_path).await;
        }
        Ok(removed)
    }

    /// Grows a volume no VM is using to `size` bytes; one that large already
    /// is left alone. qcow2 cannot resize images with internal snapshots.
    pub async fn grow_volume(&self, id: &str, size: u64) -> Result<()> {
        let path = self.volume_path(id)?;
        if img_info(&path, Some("qcow2")).await?.virtual_size >= size {
            return Ok(());
        }
        let (p, sz) = (path.to_string_lossy(), size.to_string());
        run("qemu-img", &["resize", "-q", "-f", "qcow2", &p, &sz]).await?;
        Ok(())
    }

    /// Format and virtual size of a volume, for checks and tests.
    pub async fn volume_info(&self, id: &str) -> Result<(String, u64)> {
        let info = img_info(&self.volume_path(id)?, None).await?;
        Ok((info.format, info.virtual_size))
    }
}

pub(crate) fn random_suffix() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    );
    format!("{:016x}", h.finish())
}

async fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).await?;
    Ok(())
}

/// Downloads `url` to `part`, checks it, and converts it to `out_format` at
/// `out`. See [`fetch`] for what is refused.
pub(crate) async fn fetch(
    url: &str,
    format: &str,
    part: &Path,
    out: &Path,
    out_format: &str,
    policy: FetchPolicy,
) -> Result<()> {
    let declared = match format {
        "qcow2" | "raw" => format,
        other => return Err(Error::InvalidImage(format!("unsupported format {other}"))),
    };
    fetch::download(url, part, policy).await?;
    fetch::check_untrusted(part, declared).await?;
    let (src, dst) = (part.to_string_lossy(), out.to_string_lossy());
    fetch::untrusted_qemu_img(
        &["convert", "-f", declared, "-O", out_format, &src, &dst],
        600,
        2 << 30,
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_cannot_escape_the_pool() {
        let pool = FilePool::new("/var/lib/metastratum");
        assert!(
            pool.volume_path("0199f0a2-7b3c-7d00-8000-000000000001")
                .is_ok()
        );
        for bad in ["", "../etc/passwd", "a/b", "a.b", "a b"] {
            assert!(pool.volume_path(bad).is_err(), "{bad:?}");
        }
    }

    /// Exercises qemu-img when it is installed: overlays, size checks,
    /// idempotency and rejection of images with backing files.
    #[tokio::test]
    async fn volumes_from_local_images() {
        if Command::new("qemu-img")
            .arg("--version")
            .output()
            .await
            .is_err()
        {
            return eprintln!("qemu-img not installed; skipping");
        }
        let root = std::env::temp_dir().join(format!("mst-storage-{}", std::process::id()));
        let pool = FilePool::new(&root);
        pool.init().await.unwrap();

        // Seed the cache directly (downloads need a server).
        let img = pool.image_path("img1").unwrap();
        run(
            "qemu-img",
            &["create", "-q", "-f", "qcow2", img.to_str().unwrap(), "1G"],
        )
        .await
        .unwrap();

        pool.ensure_volume("vol1", 2 << 30, Some("img1"))
            .await
            .unwrap();
        pool.ensure_volume("vol1", 2 << 30, Some("img1"))
            .await
            .unwrap();
        assert_eq!(
            pool.volume_info("vol1").await.unwrap(),
            ("qcow2".into(), 2 << 30)
        );
        let info = img_info(&pool.volume_path("vol1").unwrap(), None)
            .await
            .unwrap();
        assert_eq!(info.backing_filename.as_deref(), img.to_str());

        assert!(matches!(
            pool.ensure_volume("vol2", 1 << 20, Some("img1")).await,
            Err(Error::TooSmall { .. })
        ));
        assert!(matches!(
            pool.ensure_volume("vol3", 1 << 30, Some("nope")).await,
            Err(Error::MissingImage(_))
        ));
        pool.ensure_volume("blank", 1 << 30, None).await.unwrap();

        // A backup of vol1 stands on its own, and restores larger.
        let backup = root.join("vol1-backup.qcow2");
        pool.export_volume("vol1", &backup).await.unwrap();
        let info = img_info(&backup, Some("qcow2")).await.unwrap();
        assert_eq!((info.virtual_size, info.backing_filename), (2 << 30, None));
        pool.import_volume("restored", 3 << 30, &backup)
            .await
            .unwrap();
        pool.import_volume("restored", 3 << 30, &backup)
            .await
            .unwrap();
        assert_eq!(
            pool.volume_info("restored").await.unwrap(),
            ("qcow2".into(), 3 << 30)
        );

        // A snapshot puts the volume back as it was.
        let blank = pool.volume_path("blank").unwrap();
        let io = |cmd: &str| {
            let (cmd, path) = (cmd.to_owned(), blank.to_string_lossy().into_owned());
            async move {
                run("qemu-io", &["-f", "qcow2", "-c", &cmd, &path])
                    .await
                    .map(|out| String::from_utf8_lossy(&out).into_owned())
            }
        };
        let snap = "0199f0a2-7b3c-7d00-8000-0000000000a1";
        pool.create_snapshot("blank", snap).await.unwrap();
        pool.create_snapshot("blank", snap).await.unwrap();
        assert_eq!(
            pool.snapshots("blank").await.unwrap(),
            [format!("mst-{snap}")]
        );
        io("write -P 0xab 0 64k").await.unwrap();
        assert!(
            io("read -P 0xab 0 64k")
                .await
                .unwrap()
                .contains("read 65536")
        );
        pool.revert_snapshot("blank", snap).await.unwrap();
        assert!(io("read -P 0 0 64k").await.unwrap().contains("read 65536"));
        pool.delete_snapshot("blank", snap).await.unwrap();
        pool.delete_snapshot("blank", snap).await.unwrap();
        assert!(pool.snapshots("blank").await.unwrap().is_empty());
        assert!(pool.create_snapshot("blank", "../x").await.is_err());

        // Grown, never shrunk.
        pool.grow_volume("blank", 2 << 30).await.unwrap();
        pool.grow_volume("blank", 1 << 30).await.unwrap();
        assert_eq!(pool.volume_info("blank").await.unwrap().1, 2 << 30);

        pool.delete_volume("vol1").await.unwrap();
        pool.delete_volume("vol1").await.unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }
}
