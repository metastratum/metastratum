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
mod transfer;

pub use fetch::{FetchPolicy, PublicOnly, check_url, is_public};

pub use pools::{Pool, PoolSpec, Pools};
pub use rbd::RbdPool;
pub use snapshots::snapshot_name;
pub use transfer::NbdSource;

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
        set_mode(&tmp, 0o600).await?;
        tokio::fs::rename(&tmp, &dest).await?;
        Ok(())
    }

    /// Creates a volume as a copy of one another host serves, of at least
    /// `size` bytes. An existing volume is left alone.
    pub async fn import_nbd(&self, id: &str, size: u64, src: &transfer::NbdSource) -> Result<()> {
        let dest = self.volume_path(id)?;
        if tokio::fs::try_exists(&dest).await? {
            return Ok(());
        }
        let tmp = transfer::temporary(&dest);
        let result = async {
            transfer::copy_from_nbd(src, &tmp, "qcow2").await?;
            if size > img_info(&tmp, Some("qcow2")).await?.virtual_size {
                let (t, sz) = (tmp.to_string_lossy().into_owned(), size.to_string());
                run("qemu-img", &["resize", "-q", "-f", "qcow2", &t, &sz]).await?;
            }
            set_mode(&tmp, 0o600).await?;
            tokio::fs::rename(&tmp, &dest).await?;
            Ok(())
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&tmp).await;
        }
        result
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
            set_mode(&converted, 0o600).await?;
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
        // While a volume is put on an image, the image stays (see
        // `delete_image`).
        let lock = image.map(|i| self.image_lock(i));
        let _held = match &lock {
            Some(l) => Some(l.lock().await),
            None => None,
        };
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

    /// Removes a cached image, unless a volume here is built on it (it
    /// would break): the controller drops images no volume uses, but a VM
    /// may be getting one at that very moment. Volumes are put on images
    /// under the same lock.
    pub async fn delete_image(&self, id: &str) -> Result<()> {
        let _guard = self.image_lock(id).lock_owned().await;
        let path = self.image_path(id)?;
        if let Some(user) = self.volume_backed_by(&path).await? {
            return Err(Error::Pool(format!("volume {user} is built on image {id}")));
        }
        match tokio::fs::remove_file(path).await {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// A volume (or one being made) whose backing file is `image`, if any.
    async fn volume_backed_by(&self, image: &Path) -> Result<Option<String>> {
        let name = image.file_name().map(|n| n.to_string_lossy().into_owned());
        let mut dir = match tokio::fs::read_dir(self.root.join("volumes")).await {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        while let Some(e) = dir.next_entry().await? {
            let backing = qcow2_backing_file(&e.path()).await.unwrap_or_default();
            let file = backing.as_deref().and_then(|b| Path::new(b).file_name());
            if file.map(|f| f.to_string_lossy().into_owned()) == name {
                return Ok(Some(e.file_name().to_string_lossy().into_owned()));
            }
        }
        Ok(None)
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

    /// Creates volume `id` as a copy of `source`, at least `size` bytes,
    /// keeping only what differs from `image` when given (the copy then
    /// shares it, as volumes made from it do). Internal snapshots are not
    /// copied. An existing volume is left alone.
    pub async fn clone_volume(
        &self,
        source: &str,
        id: &str,
        size: u64,
        image: Option<&str>,
    ) -> Result<()> {
        let dest = self.volume_path(id)?;
        if tokio::fs::try_exists(&dest).await? {
            return Ok(());
        }
        let src = self.volume_path(source)?;
        let tmp = dest.with_extension(format!("{}.tmp", random_suffix()));
        let (s, t) = (src.to_string_lossy(), tmp.to_string_lossy());
        let base = image.map(|i| self.image_path(i)).transpose()?;
        let base_s = base.as_ref().map(|b| b.to_string_lossy());
        let lock = image.map(|i| self.image_lock(i));
        let _held = match &lock {
            Some(l) => Some(l.lock().await),
            None => None,
        };
        let mut args = vec!["convert", "-q", "-f", "qcow2", "-O", "qcow2"];
        if let Some(b) = &base_s {
            args.extend(["-B", b, "-F", "qcow2"]);
        }
        args.extend([&*s, &*t]);
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
/// starts) but not list it, and its files are readable by their owner only.
/// Files made before are tightened too.
pub async fn private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::create_dir_all(dir).await?;
    set_mode(dir, 0o711).await?;
    let mut entries = tokio::fs::read_dir(dir).await?;
    let gone = |e: &std::io::Error| e.kind() == std::io::ErrorKind::NotFound;
    while let Some(e) = entries.next_entry().await? {
        // Files come and go meanwhile (others' are written beside these,
        // through temporary names): one gone needs no tightening.
        let meta = match e.metadata().await {
            Err(err) if gone(&err) => continue,
            m => m?,
        };
        if meta.is_file() && meta.permissions().mode() & 0o077 != 0 {
            match tokio::fs::set_permissions(e.path(), std::fs::Permissions::from_mode(0o600)).await
            {
                Err(err) if gone(&err) => {}
                r => r?,
            }
        }
    }
    Ok(())
}

/// A qcow2 file's backing file name, read from its header (cheaper than
/// `qemu-img info` for every volume). None for anything else.
async fn qcow2_backing_file(path: &Path) -> std::io::Result<Option<String>> {
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
    let size = fetch::check_untrusted(part, declared).await?;
    if size > policy.max_virtual_bytes {
        return Err(Error::InvalidImage(format!(
            "the image is a {} GiB disk, larger than the {} GiB disk it is for",
            size.div_ceil(1 << 30),
            policy.max_virtual_bytes >> 30
        )));
    }
    let (src, dst) = (part.to_string_lossy(), out.to_string_lossy());
    // What the conversion may write: the disk, and room for qcow2's tables.
    let limit = size + (size / 50).max(64 << 20);
    fetch::untrusted_qemu_img(
        &["convert", "-f", declared, "-O", out_format, &src, &dst],
        600,
        2 << 30,
        Some(limit),
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
    async fn private_dirs_tolerate_files_coming_and_going() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("mst-private-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let old = dir.join("old.iso");
        tokio::fs::write(&old, "x").await.unwrap();
        tokio::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o644))
            .await
            .unwrap();
        // Seeds written meanwhile, through temporary names, as VMs are
        // defined side by side.
        let churn = {
            let dir = dir.clone();
            tokio::spawn(async move {
                for i in 0..2000 {
                    let tmp = dir.join(format!("{i}.tmp"));
                    tokio::fs::write(&tmp, "x").await.unwrap();
                    tokio::fs::rename(&tmp, dir.join(format!("{i}.iso")))
                        .await
                        .unwrap();
                    tokio::fs::remove_file(dir.join(format!("{i}.iso")))
                        .await
                        .unwrap();
                }
            })
        };
        while !churn.is_finished() {
            private_dir(&dir).await.unwrap();
        }
        churn.await.unwrap();
        let mode = tokio::fs::metadata(&old)
            .await
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

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
        let body = tokio::fs::read(&img).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/img.qcow2", listener.local_addr().unwrap());
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            while let Ok((mut c, _)) = listener.accept().await {
                let mut req = [0u8; 4096];
                let _ = c.read(&mut req).await;
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = c.write_all(head.as_bytes()).await;
                let _ = c.write_all(&body).await;
            }
        });
        let policy = |max_virtual_bytes| FetchPolicy {
            allow_private: true,
            max_virtual_bytes,
            ..FetchPolicy::default()
        };
        let (part, out) = (dir.join("x.part"), dir.join("x.qcow2"));
        let err = fetch(&url, "qcow2", &part, &out, "qcow2", policy(512 << 20))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("larger than"), "{err}");
        assert!(!out.exists(), "nothing unpacked");
        fetch(&url, "qcow2", &part, &out, "qcow2", policy(1 << 30))
            .await
            .unwrap();
        assert!(out.exists());
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

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
        // A cached image stays while a volume is built on it.
        assert_eq!(
            qcow2_backing_file(&pool.volume_path("vol1").unwrap())
                .await
                .unwrap()
                .as_deref(),
            img.to_str()
        );
        assert!(pool.delete_image("img1").await.is_err());
        assert!(img.exists());

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

        // A clone has vol1's data, still on img1, and stands apart from it.
        pool.clone_volume("vol1", "copy", 3 << 30, Some("img1"))
            .await
            .unwrap();
        pool.clone_volume("vol1", "copy", 3 << 30, Some("img1"))
            .await
            .unwrap();
        let info = img_info(&pool.volume_path("copy").unwrap(), None)
            .await
            .unwrap();
        assert_eq!(
            (info.virtual_size, info.backing_filename.as_deref()),
            (3 << 30, img.to_str())
        );
        pool.clone_volume("blank", "blank-copy", 0, None)
            .await
            .unwrap();
        assert_eq!(
            img_info(&pool.volume_path("blank-copy").unwrap(), None)
                .await
                .unwrap()
                .backing_filename,
            None
        );

        // Grown, never shrunk.
        pool.grow_volume("blank", 2 << 30).await.unwrap();
        pool.grow_volume("blank", 1 << 30).await.unwrap();
        assert_eq!(pool.volume_info("blank").await.unwrap().1, 2 << 30);

        pool.delete_volume("vol1").await.unwrap();
        pool.delete_volume("vol1").await.unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }
}
