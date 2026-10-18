//! Ceph RBD pools, driven with the `rbd` tool (package `ceph-common`).
//!
//! Images kept in the pool are in its image store (`store_*`, see
//! [`RbdPool::store_name`]); volumes made from them are copies. Volumes made
//! before the store are copy-on-write clones (`vol-<id>`) of an image
//! imported as `img-<id>` with a protected `base` snapshot. QEMU reaches the
//! volumes directly over the network, authenticating with a libvirt secret.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::Deserialize;

use crate::{Error, Result, fetch, random_suffix};

#[derive(Deserialize)]
pub(crate) struct RbdConfig {
    pub monitors: Vec<String>,
    pub pool: String,
    pub user: String,
    pub key: String,
}

#[derive(Clone)]
pub struct RbdPool {
    pub monitors: Vec<String>,
    pub pool: String,
    pub user: String,
    pub key: String,
    keyring: PathBuf,
    work_dir: PathBuf,
    image_locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

pub(crate) fn safe(name: &str) -> Result<&str> {
    if !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        Ok(name)
    } else {
        Err(Error::Pool(format!("bad RBD name {name:?}")))
    }
}

impl RbdPool {
    /// Writes the client keyring into `dir` (mode 0600) so the key never
    /// appears on a command line.
    pub(crate) async fn new(cfg: RbdConfig, dir: &Path) -> Result<Self> {
        safe(&cfg.pool)?;
        safe(&cfg.user)?;
        tokio::fs::create_dir_all(dir.join("tmp")).await?;
        let keyring = dir.join("keyring");
        let contents = format!("[client.{}]\n\tkey = {}\n", cfg.user, cfg.key.trim());
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&keyring)?;
            std::io::Write::write_all(&mut f, contents.as_bytes())?;
        }
        Ok(Self {
            monitors: cfg.monitors,
            pool: cfg.pool,
            user: cfg.user,
            key: cfg.key,
            keyring,
            work_dir: dir.join("tmp"),
            image_locks: Arc::default(),
        })
    }

    pub fn image_name(id: &str) -> Result<String> {
        Ok(format!("img-{}", safe(id)?))
    }

    pub fn volume_name(id: &str) -> Result<String> {
        Ok(format!("vol-{}", safe(id)?))
    }

    /// `pool/vol-<id>`, as QEMU and libvirt name it.
    pub fn volume_path(&self, id: &str) -> Result<String> {
        Ok(format!("{}/{}", self.pool, Self::volume_name(id)?))
    }

    /// Monitors as `(host, port)`, defaulting the port to 6789.
    pub fn monitor_addrs(&self) -> Vec<(String, u16)> {
        self.monitors
            .iter()
            .map(|m| match m.rsplit_once(':') {
                Some((h, p)) => (h.to_owned(), p.parse().unwrap_or(6789)),
                None => (m.clone(), 6789),
            })
            .collect()
    }

    pub(crate) async fn rbd(&self, args: &[&str]) -> Result<Vec<u8>> {
        let keyring = self.keyring.to_string_lossy();
        let mons = self.monitors.join(",");
        let mut all = vec![
            "--id",
            &self.user,
            "--keyring",
            &keyring,
            "-m",
            &mons,
            "--no-mon-config",
        ];
        all.extend_from_slice(args);
        crate::run("rbd", &all).await
    }

    pub(crate) async fn exists(&self, spec: &str) -> Result<bool> {
        match self.rbd(&["info", spec]).await {
            Ok(_) => Ok(true),
            Err(Error::Tool { stderr, .. }) if stderr.contains("No such file") => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Where copies keep their temporary files (Ceph keys for QEMU).
    pub(crate) fn work_dir(&self) -> &Path {
        &self.work_dir
    }

    pub(crate) fn spec(&self, name: &str) -> String {
        format!("{}/{}", self.pool, name)
    }

    /// Imports an image (once) as a protected snapshot volumes clone from.
    pub async fn ensure_image(
        &self,
        id: &str,
        url: &str,
        format: &str,
        policy: crate::FetchPolicy,
    ) -> Result<()> {
        let name = Self::image_name(id)?;
        let lock = self
            .image_locks
            .lock()
            .unwrap()
            .entry(id.to_owned())
            .or_default()
            .clone();
        let _guard = lock.lock().await;
        if self.exists(&format!("{}@base", self.spec(&name))).await? {
            return Ok(());
        }
        let unique = format!("{}.{}", std::process::id(), random_suffix());
        let part = self.work_dir.join(format!("{id}.{unique}.part"));
        let raw = self.work_dir.join(format!("{id}.{unique}.raw"));
        // Import under a temporary name and rename once complete, so other
        // hosts never see a half-imported image.
        let tmp_name = format!("{name}.tmp{}", random_suffix());
        let result = async {
            fetch(url, format, &part, &raw, "raw", policy, None).await?;
            let _ = tokio::fs::remove_file(&part).await;
            let raw_s = raw.to_string_lossy();
            self.rbd(&[
                "import",
                "--image-format",
                "2",
                &raw_s,
                &self.spec(&tmp_name),
            ])
            .await?;
            self.rbd(&["snap", "create", &format!("{}@base", self.spec(&tmp_name))])
                .await?;
            self.rbd(&["snap", "protect", &format!("{}@base", self.spec(&tmp_name))])
                .await?;
            match self
                .rbd(&["rename", &self.spec(&tmp_name), &self.spec(&name)])
                .await
            {
                Ok(_) => Ok(()),
                // Another host won the race: use its image, drop ours.
                Err(_) if self.exists(&self.spec(&name)).await? => {
                    self.remove_image(&tmp_name).await;
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
        .await;
        let _ = tokio::fs::remove_file(&part).await;
        let _ = tokio::fs::remove_file(&raw).await;
        if result.is_err() {
            self.remove_image(&tmp_name).await;
        } else {
            tracing::info!(image = id, pool = %self.pool, "image imported into RBD");
        }
        result
    }

    /// Removes an imported image (and its snapshot). Fails while volumes
    /// are cloned from it.
    pub async fn delete_image(&self, id: &str) -> Result<()> {
        let name = Self::image_name(id)?;
        if !self.exists(&self.spec(&name)).await? {
            return Ok(());
        }
        let snap = format!("{}@base", self.spec(&name));
        let children = self.rbd(&["children", &snap]).await.unwrap_or_default();
        if !String::from_utf8_lossy(&children).trim().is_empty() {
            return Err(Error::Pool(format!(
                "{name} still has volumes cloned from it"
            )));
        }
        let _ = self.rbd(&["snap", "unprotect", &snap]).await;
        self.rbd(&["snap", "purge", &self.spec(&name)]).await?;
        self.rbd(&["rm", &self.spec(&name)]).await?;
        Ok(())
    }

    /// The store's name for an image (see [`crate::ImageStore`]):
    /// `store_<tenant>_<sub-tenant>_..._<image id>`, a raw RBD image.
    pub fn store_name(tenant_path: &str, id: &str) -> Result<String> {
        let labels = crate::store::tenant_labels(tenant_path)?;
        Ok(format!("store_{}_{}", labels.join("_"), safe(id)?))
    }

    fn store_lock(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.image_locks
            .lock()
            .unwrap()
            .entry(format!("store:{id}"))
            .or_default()
            .clone()
    }

    async fn stored(&self, name: &str, sha256: String) -> Result<crate::StoredImage> {
        #[derive(Deserialize)]
        struct Du {
            images: Vec<DuImage>,
        }
        #[derive(Deserialize)]
        struct DuImage {
            used_size: u64,
        }
        let spec = self.spec(name);
        let virtual_bytes = self.size_of(&spec).await?;
        // What it takes in Ceph (thin): its size where that cannot be told.
        let used = self
            .rbd(&["du", "--format", "json", &spec])
            .await
            .ok()
            .and_then(|out| serde_json::from_slice::<Du>(&out).ok())
            .and_then(|du| du.images.first().map(|i| i.used_size));
        Ok(crate::StoredImage {
            size_bytes: used.unwrap_or(virtual_bytes),
            virtual_bytes,
            sha256,
        })
    }

    /// The size of an RBD image, if it exists.
    pub(crate) async fn image_size(&self, name: &str) -> Result<Option<u64>> {
        let spec = self.spec(safe(name)?);
        if !self.exists(&spec).await? {
            return Ok(None);
        }
        Ok(Some(self.size_of(&spec).await?))
    }

    /// Whether the store holds the image.
    pub async fn has_stored(&self, tenant_path: &str, id: &str) -> Result<bool> {
        self.exists(&self.spec(&Self::store_name(tenant_path, id)?))
            .await
    }

    /// Downloads an image into the store (see [`crate::ImageStore::fetch`]):
    /// checked and converted on this host first, then copied in.
    pub async fn fetch_stored(
        &self,
        tenant_path: &str,
        id: &str,
        url: &str,
        format: &str,
        policy: crate::FetchPolicy,
        sha256: Option<&str>,
    ) -> Result<crate::StoredImage> {
        let name = Self::store_name(tenant_path, id)?;
        let lock = self.store_lock(id);
        let _held = lock.lock().await;
        if self.exists(&self.spec(&name)).await? {
            return self.stored(&name, String::new()).await;
        }
        let unique = format!("{}.{}", std::process::id(), random_suffix());
        let part = self.work_dir.join(format!("{id}.{unique}.part"));
        let converted = self.work_dir.join(format!("{id}.{unique}.qcow2"));
        let result = async {
            let hash = fetch(url, format, &part, &converted, "qcow2", policy, sha256).await?;
            let _ = tokio::fs::remove_file(&part).await;
            let src = crate::CopySource::File(&converted);
            crate::transfer::copy_to_rbd_name(self, &name, 0, &src).await?;
            self.stored(&name, hash).await
        }
        .await;
        let _ = tokio::fs::remove_file(&part).await;
        let _ = tokio::fs::remove_file(&converted).await;
        if result.is_ok() {
            tracing::info!(image = id, pool = %self.pool, "image downloaded into the store");
        }
        result
    }

    /// Removes an image from the store.
    pub async fn remove_stored(&self, tenant_path: &str, id: &str) -> Result<()> {
        let spec = self.spec(&Self::store_name(tenant_path, id)?);
        let lock = self.store_lock(id);
        let _held = lock.lock().await;
        match self.rbd(&["rm", "--no-progress", &spec]).await {
            Err(Error::Tool { stderr, .. }) if stderr.contains("No such file") => Ok(()),
            r => r.map(|_| ()),
        }
    }

    /// Removes metastratum's images from the Ceph pool (`img-*`, and the
    /// store's `store_*`; volumes are gone before a pool is removed, and
    /// anything else is not ours).
    /// Returns how many were removed.
    pub async fn purge(&self) -> Result<usize> {
        let list = self.rbd(&["ls", &self.pool]).await?;
        let mut removed = 0;
        for name in String::from_utf8_lossy(&list).lines() {
            let name = name.trim();
            if let Some(id) = name.strip_prefix("img-") {
                self.delete_image(id).await?;
                removed += 1;
            } else if name.starts_with("store_") {
                self.rbd(&["rm", "--no-progress", &self.spec(safe(name)?)])
                    .await?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    async fn remove_image(&self, name: &str) {
        let snap = format!("{}@base", self.spec(name));
        let _ = self.rbd(&["snap", "unprotect", &snap]).await;
        let _ = self.rbd(&["snap", "rm", &snap]).await;
        let _ = self.rbd(&["rm", &self.spec(name)]).await;
    }

    async fn size_of(&self, spec: &str) -> Result<u64> {
        #[derive(Deserialize)]
        struct Info {
            size: u64,
        }
        let out = self.rbd(&["info", "--format", "json", spec]).await?;
        Ok(serde_json::from_slice::<Info>(&out)
            .map_err(|e| Error::Pool(e.to_string()))?
            .size)
    }

    /// Creates a volume, as a clone of an image if given. Existing volumes
    /// are left alone.
    pub async fn ensure_volume(&self, id: &str, size: u64, image: Option<&str>) -> Result<()> {
        let vol = self.spec(&Self::volume_name(id)?);
        if self.exists(&vol).await? {
            return Ok(());
        }
        let mib = size.div_ceil(1 << 20).to_string();
        match image {
            Some(img) => {
                let base = self.spec(&Self::image_name(img)?);
                if !self.exists(&format!("{base}@base")).await? {
                    return Err(Error::MissingImage(img.into()));
                }
                let image_size = self.size_of(&base).await?;
                if size < image_size {
                    return Err(Error::TooSmall {
                        requested: size,
                        image: image_size,
                    });
                }
                self.rbd(&["clone", &format!("{base}@base"), &vol]).await?;
                self.rbd(&["resize", "--size", &mib, &vol]).await?;
            }
            None => {
                self.rbd(&["create", "--size", &mib, &vol]).await?;
            }
        }
        Ok(())
    }

    pub async fn delete_volume(&self, id: &str) -> Result<()> {
        let vol = self.spec(&Self::volume_name(id)?);
        // Its snapshots go with it (`rbd rm` refuses an image that has any).
        match self.rbd(&["snap", "purge", "--no-progress", &vol]).await {
            Err(Error::Tool { stderr, .. }) if stderr.contains("No such file") => return Ok(()),
            r => r.map(|_| ())?,
        }
        match self.rbd(&["rm", "--no-progress", &vol]).await {
            Err(Error::Tool { stderr, .. }) if stderr.contains("No such file") => Ok(()),
            r => r.map(|_| ()),
        }
    }

    /// Writes a copy of a volume to `dest` as a qcow2 file, by way of a raw
    /// export beside it.
    pub async fn export_volume(&self, id: &str, dest: &Path) -> Result<()> {
        let vol = self.spec(&Self::volume_name(id)?);
        let raw = dest.with_extension(format!("{}.raw", random_suffix()));
        let raw_s = raw.to_string_lossy().into_owned();
        let result = async {
            self.rbd(&["export", "--no-progress", &vol, &raw_s]).await?;
            crate::convert(&raw, "raw", dest, "qcow2").await
        }
        .await;
        let _ = tokio::fs::remove_file(&raw).await;
        result
    }

    /// Creates a volume from a qcow2 file (a backup's disk), of at least
    /// `size` bytes, by way of a raw file beside it. An existing volume is
    /// left alone.
    pub async fn import_volume(&self, id: &str, size: u64, src: &Path) -> Result<()> {
        let vol = self.spec(&Self::volume_name(id)?);
        if self.exists(&vol).await? {
            return Ok(());
        }
        let raw = src.with_extension(format!("{}.raw", random_suffix()));
        let raw_s = raw.to_string_lossy().into_owned();
        let result = async {
            crate::convert(src, "qcow2", &raw, "raw").await?;
            self.rbd(&["import", "--no-progress", &raw_s, &vol]).await?;
            if size > self.size_of(&vol).await? {
                let mib = size.div_ceil(1 << 20).to_string();
                self.rbd(&["resize", "--size", &mib, &vol]).await?;
            }
            Ok(())
        }
        .await;
        let _ = tokio::fs::remove_file(&raw).await;
        result
    }

    /// Creates volume `id` as a full copy of `source` (its snapshots are not
    /// copied), at least `size` bytes; under a temporary name until done.
    /// An existing volume is left alone.
    pub async fn clone_volume(&self, source: &str, id: &str, size: u64) -> Result<()> {
        let vol = self.spec(&Self::volume_name(id)?);
        if self.exists(&vol).await? {
            return Ok(());
        }
        let src = self.spec(&Self::volume_name(source)?);
        let tmp = format!("{vol}-{}", random_suffix());
        let result = async {
            self.rbd(&["cp", "--no-progress", &src, &tmp]).await?;
            if size > self.size_of(&tmp).await? {
                let mib = size.div_ceil(1 << 20).to_string();
                self.rbd(&["resize", "--no-progress", "--size", &mib, &tmp])
                    .await?;
            }
            self.rbd(&["rename", &tmp, &vol]).await?;
            Ok(())
        }
        .await;
        if result.is_err() {
            let _ = self.rbd(&["rm", "--no-progress", &tmp]).await;
        }
        result
    }

    /// Grows a volume to `size` bytes; one that large already is left
    /// alone. Safe while a VM uses it, though the VM only notices through
    /// libvirt's block resize.
    pub async fn grow_volume(&self, id: &str, size: u64) -> Result<()> {
        let vol = self.spec(&Self::volume_name(id)?);
        if self.size_of(&vol).await? >= size {
            return Ok(());
        }
        let mib = size.div_ceil(1 << 20).to_string();
        self.rbd(&["resize", "--no-progress", "--size", &mib, &vol])
            .await?;
        Ok(())
    }

    /// Size and existence of a volume, for checks and tests.
    pub async fn volume_size(&self, id: &str) -> Result<Option<u64>> {
        let vol = self.spec(&Self::volume_name(id)?);
        if !self.exists(&vol).await? {
            return Ok(None);
        }
        Ok(Some(self.size_of(&vol).await?))
    }
}
