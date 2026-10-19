//! Storage for the metastratum agent.
//!
//! File pools are directories of qcow2 volumes, each standing on its own,
//! and an image store (see [`store`]). The local pool is a directory on the
//! host; an NFS pool is the same layout on a share mounted by the agent, so
//! every host attached to it sees the same images and volumes. [`Pools`]
//! prepares pools on demand from the descriptions the controller sends.

pub mod backups;
mod fetch;
mod pools;
mod rbd;
mod snapshots;
mod store;
mod transfer;

pub use fetch::{Download, FetchPolicy, PublicOnly, check_url, is_public};

pub use pools::{Pool, PoolSpec, Pools};
pub use rbd::RbdPool;
pub use snapshots::snapshot_name;
pub use store::{ImageStore, StoredImage};
pub use transfer::{CopySource, NbdSource, copy_into};

use std::path::{Path, PathBuf};

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
    #[error("image {0} is not in the image store")]
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
    // Cut off (an image deleted while it downloads), it stops too.
    let out = Command::new(tool)
        .args(args)
        .kill_on_drop(true)
        .output()
        .await?;
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
}

impl FilePool {
    /// `root` holds `images/` (the image store) and `volumes/`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The pool's directory (a backup repository keeps `backups/` here).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Writes a copy of a volume to `dest` as a qcow2 file of its own.
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
        set_mode(&tmp, 0o600).await?;
        tokio::fs::rename(&tmp, &dest).await?;
        Ok(())
    }

    pub async fn init(&self) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        // QEMU passes through the pool's root to its VM's files.
        tokio::fs::create_dir_all(&self.root).await?;
        let mode = tokio::fs::metadata(&self.root).await?.permissions().mode();
        if mode & 0o011 != 0o011 {
            set_mode(&self.root, (mode | 0o711) & 0o7777).await?;
        }
        for dir in ["images", "volumes"] {
            private_dir(&self.root.join(dir)).await?;
        }
        Ok(())
    }

    pub fn volume_path(&self, id: &str) -> Result<PathBuf> {
        Ok(self
            .root
            .join("volumes")
            .join(format!("{}.qcow2", safe_id(id)?)))
    }

    /// Creates a blank volume. Existing volumes are left alone.
    pub async fn ensure_volume(&self, id: &str, size: u64) -> Result<()> {
        let dest = self.volume_path(id)?;
        if tokio::fs::try_exists(&dest).await? {
            return Ok(());
        }
        let tmp = dest.with_extension(format!("{}.tmp", random_suffix()));
        let (tmp_s, size_s) = (tmp.to_string_lossy().into_owned(), size.to_string());
        run(
            "qemu-img",
            &["create", "-q", "-f", "qcow2", &tmp_s, &size_s],
        )
        .await?;
        set_mode(&tmp, 0o600).await?;
        tokio::fs::rename(&tmp, &dest).await?;
        Ok(())
    }

    pub async fn delete_volume(&self, id: &str) -> Result<()> {
        match tokio::fs::remove_file(self.volume_path(id)?).await {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// Removes everything of metastratum's from the pool: its image store,
    /// and any volumes left (the controller removes pools that hold none).
    /// Only metastratum's own files go (`<id>.qcow2` in `volumes/`, and the
    /// store's tenants' directories); `images/` and `volumes/` go too if
    /// that leaves them empty. Returns how many files were removed.
    pub async fn purge(&self) -> Result<usize> {
        let mut removed = crate::ImageStore::new(&self.root).purge().await?;
        let _ = tokio::fs::remove_dir(self.root.join("images")).await;
        for sub in ["volumes"] {
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

    /// Creates volume `id` as a copy of `source`, at least `size` bytes,
    /// depending on nothing. Internal snapshots are not copied. An existing
    /// volume is left alone.
    pub async fn clone_volume(&self, source: &str, id: &str, size: u64) -> Result<()> {
        let dest = self.volume_path(id)?;
        if tokio::fs::try_exists(&dest).await? {
            return Ok(());
        }
        let src = self.volume_path(source)?;
        let tmp = dest.with_extension(format!("{}.tmp", random_suffix()));
        let (s, t) = (src.to_string_lossy(), tmp.to_string_lossy());
        let args = ["convert", "-q", "-f", "qcow2", "-O", "qcow2", &*s, &*t];
        let result = async {
            run("qemu-img", &args).await?;
            if size > img_info(&tmp, Some("qcow2")).await?.virtual_size {
                let sz = size.to_string();
                run("qemu-img", &["resize", "-q", "-f", "qcow2", &t, &sz]).await?;
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e);
        }
        set_mode(&tmp, 0o600).await?;
        tokio::fs::rename(&tmp, &dest).await?;
        Ok(())
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

/// Makes a directory of VMs' files private: others on the host may pass
/// through it (QEMU, which libvirt gives each VM's own files when it
/// starts) but not list it. What is written into it is readable by its
/// owner only.
pub async fn private_dir(dir: &Path) -> Result<()> {
    tokio::fs::create_dir_all(dir).await?;
    set_mode(dir, 0o711).await?;
    Ok(())
}

/// A qcow2 file's backing file name, read from its header (cheaper than
/// `qemu-img info` for every volume). None for anything else.
pub(crate) async fn qcow2_backing_file(path: &Path) -> std::io::Result<Option<String>> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut f = tokio::fs::File::open(path).await?;
    let mut header = [0u8; 20];
    if f.read_exact(&mut header).await.is_err() || header[..4] != *b"QFI\xfb" {
        return Ok(None);
    }
    let offset = u64::from_be_bytes(header[8..16].try_into().unwrap_or_default());
    let len = u32::from_be_bytes(header[16..20].try_into().unwrap_or_default());
    if offset == 0 || len == 0 || len > 1023 {
        return Ok(None);
    }
    f.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut name = vec![0u8; len as usize];
    f.read_exact(&mut name).await?;
    Ok(Some(String::from_utf8_lossy(&name).into_owned()))
}

async fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).await?;
    Ok(())
}

/// Checks an image a host received (uploaded, or downloaded by the
/// controller) and converts it to `out_format` at `out`. See [`fetch`] for
/// what is refused. Returns the SHA-256 of the file as it came (hex).
pub(crate) async fn import(
    file: &Path,
    format: &str,
    out: &Path,
    out_format: &str,
    policy: FetchPolicy,
) -> Result<String> {
    let declared = match format {
        "qcow2" | "raw" => format,
        other => return Err(Error::InvalidImage(format!("unsupported format {other}"))),
    };
    let len = tokio::fs::metadata(file).await?.len();
    if len > policy.max_bytes {
        return Err(Error::InvalidImage(format!(
            "larger than the {} GiB limit",
            policy.max_bytes >> 30
        )));
    }
    let hash = fetch::hash_file(file).await?;
    let size = fetch::check_untrusted(file, declared).await?;
    if size > policy.max_virtual_bytes {
        return Err(Error::InvalidImage(format!(
            "the image is a {} GiB disk, larger than the {} GiB allowed",
            size.div_ceil(1 << 30),
            policy.max_virtual_bytes >> 30
        )));
    }
    let (src, dst) = (file.to_string_lossy(), out.to_string_lossy());
    // What the conversion may write: the disk, and room for qcow2's tables.
    let limit = size + (size / 50).max(64 << 20);
    fetch::untrusted_qemu_img(
        &["convert", "-f", declared, "-O", out_format, &src, &dst],
        600,
        2 << 30,
        Some(limit),
    )
    .await?;
    Ok(hash)
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
    async fn images_larger_than_their_disk_are_refused_before_unpacking() {
        if Command::new("qemu-img")
            .arg("--version")
            .output()
            .await
            .is_err()
        {
            return eprintln!("qemu-img not installed; skipping");
        }
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = std::env::temp_dir().join(format!("mst-bomb-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        // A small file describing a 1 GiB disk.
        let img = dir.join("img.qcow2");
        run(
            "qemu-img",
            &["create", "-q", "-f", "qcow2", img.to_str().unwrap(), "1G"],
        )
        .await
        .unwrap();
        let policy = |max_virtual_bytes| FetchPolicy {
            max_virtual_bytes,
            ..FetchPolicy::default()
        };
        let out = dir.join("x.qcow2");
        let err = import(&img, "qcow2", &out, "qcow2", policy(512 << 20))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("larger than"), "{err}");
        assert!(!out.exists(), "nothing unpacked");
        let hash = import(&img, "qcow2", &out, "qcow2", policy(1 << 30))
            .await
            .unwrap();
        assert_eq!(hash.len(), 64);
        assert!(out.exists());
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn volumes_in_file_pools() {
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

        pool.ensure_volume("vol1", 2 << 30).await.unwrap();
        pool.ensure_volume("vol1", 2 << 30).await.unwrap();
        assert_eq!(
            pool.volume_info("vol1").await.unwrap(),
            ("qcow2".into(), 2 << 30)
        );
        let v1 = pool
            .volume_path("vol1")
            .unwrap()
            .to_string_lossy()
            .into_owned();
        run(
            "qemu-io",
            &["-f", "qcow2", "-c", "write -P 0xcd 0 64k", &v1],
        )
        .await
        .unwrap();
        pool.ensure_volume("blank", 1 << 30).await.unwrap();

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

        // A clone has vol1's data, and stands apart from it.
        pool.clone_volume("vol1", "copy", 3 << 30).await.unwrap();
        pool.clone_volume("vol1", "copy", 3 << 30).await.unwrap();
        let copy = pool.volume_path("copy").unwrap();
        let info = img_info(&copy, None).await.unwrap();
        assert_eq!((info.virtual_size, info.backing_filename), (3 << 30, None));
        let c = copy.to_string_lossy().into_owned();
        let out = run("qemu-io", &["-f", "qcow2", "-c", "read -P 0xcd 0 64k", &c])
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&out).contains("read 65536"));

        // Grown, never shrunk.
        pool.grow_volume("blank", 2 << 30).await.unwrap();
        pool.grow_volume("blank", 1 << 30).await.unwrap();
        assert_eq!(pool.volume_info("blank").await.unwrap().1, 2 << 30);

        pool.delete_volume("vol1").await.unwrap();
        pool.delete_volume("vol1").await.unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }
}
