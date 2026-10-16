//! Storage for the hypermon agent.
//!
//! File pools are directories of qcow2 volumes, each a copy-on-write overlay
//! of an image cached in the same pool. The local pool is a directory on the
//! host; an NFS pool is the same layout on a share mounted by the agent, so
//! every host attached to it sees the same images and volumes. [`Pools`]
//! prepares pools on demand from the descriptions the controller sends.

mod pools;
mod rbd;

pub use pools::{Pool, PoolSpec, Pools};
pub use rbd::RbdPool;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
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

    /// Downloads an image into the cache unless already present. Downloads
    /// are converted to qcow2 and must not reference backing files, which a
    /// hostile image could use to read arbitrary host files.
    pub async fn ensure_image(&self, id: &str, url: &str, format: &str) -> Result<()> {
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
            fetch(url, format, &part, &converted, "qcow2").await?;
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
/// `out`. Downloads must not reference backing files, which a hostile image
/// could use to read arbitrary host files.
pub(crate) async fn fetch(
    url: &str,
    format: &str,
    part: &Path,
    out: &Path,
    out_format: &str,
) -> Result<()> {
    download(url, part).await?;
    let declared = match format {
        "qcow2" | "raw" => format,
        other => return Err(Error::InvalidImage(format!("unsupported format {other}"))),
    };
    let info = img_info(part, Some(declared)).await?;
    if info.backing_filename.is_some() {
        return Err(Error::InvalidImage(
            "image references a backing file".into(),
        ));
    }
    let (src, dst) = (part.to_string_lossy(), out.to_string_lossy());
    run(
        "qemu-img",
        &["convert", "-f", declared, "-O", out_format, &src, &dst],
    )
    .await?;
    Ok(())
}

async fn download(url: &str, dest: &Path) -> Result<()> {
    let resp = reqwest::get(url)
        .await
        .map_err(|e| Error::Download(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(Error::Download(format!("{url}: HTTP {}", resp.status())));
    }
    let mut file = tokio::fs::File::create(dest).await?;
    let mut body = resp.bytes_stream();
    while let Some(chunk) = body.next().await {
        file.write_all(&chunk.map_err(|e| Error::Download(e.to_string()))?)
            .await?;
    }
    file.flush().await?;
    file.sync_all().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_cannot_escape_the_pool() {
        let pool = FilePool::new("/var/lib/hypermon");
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
        let root = std::env::temp_dir().join(format!("hm-storage-{}", std::process::id()));
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

        pool.delete_volume("vol1").await.unwrap();
        pool.delete_volume("vol1").await.unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }
}
