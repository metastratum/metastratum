//! Ceph RBD pools, driven with the `rbd` tool (package `ceph-common`).
//!
//! Volumes are RBD images (`vol-<id>`); images kept in the pool are in its
//! image store (`store_*`, see [`RbdPool::store_name`]), and volumes made
//! from them are copies. QEMU reaches the volumes directly over the network,
//! authenticating with a libvirt secret.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::Deserialize;

use crate::{Error, Result, import, random_suffix};

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
        // Downloads and key files go there: private, whatever the umask.
        crate::set_mode(dir, 0o700).await?;
        crate::set_mode(&dir.join("tmp"), 0o700).await?;
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

    async fn meta_get(&self, spec: &str, key: &str) -> Option<String> {
        let out = self.rbd(&["image-meta", "get", spec, key]).await.ok()?;
        Some(String::from_utf8_lossy(&out).trim().to_owned()).filter(|v| !v.is_empty())
    }

    pub(crate) async fn meta_set(&self, spec: &str, key: &str, value: &str) -> Result<()> {
        self.rbd(&["image-meta", "set", spec, key, value]).await?;
        Ok(())
    }

    /// The SHA-256 of an RBD image's contents (hex).
    async fn digest_of(&self, spec: &str) -> Result<String> {
        let keyring = self.keyring.to_string_lossy().into_owned();
        let mons = self.monitors.join(",");
        let mut child = tokio::process::Command::new("rbd")
            .args(["--id", &self.user, "--keyring", &keyring, "-m", &mons])
            .args(["--no-mon-config", "export", "--no-progress", spec, "-"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let stdout = child.stdout.take().expect("piped");
        let digest = crate::store::hash_reader(stdout).await;
        let out = child.wait_with_output().await?;
        if !out.status.success() {
            return Err(Error::Tool {
                tool: "rbd",
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
            });
        }
        digest
    }

    /// What the store holds of an image: its sizes, and what is kept with
    /// it (worked out again, should that be missing).
    async fn stored(&self, name: &str) -> Result<crate::StoredImage> {
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
        let digest = match self.meta_get(&spec, "mst.digest").await {
            Some(d) => d,
            None => {
                let d = self.digest_of(&spec).await?;
                self.meta_set(&spec, "mst.digest", &d).await?;
                d
            }
        };
        Ok(crate::StoredImage {
            size_bytes: used.unwrap_or(virtual_bytes),
            virtual_bytes,
            sha256: self.meta_get(&spec, "mst.sha256").await.unwrap_or_default(),
            digest,
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

    /// Adds an image to the store from a file this host received (see
    /// [`crate::ImageStore::add`]):
    /// checked, converted and its digest worked out on this host first,
    /// then copied in, unless another host stored it meanwhile (that one
    /// stays).
    pub async fn add_stored(
        &self,
        tenant_path: &str,
        id: &str,
        file: &Path,
        format: &str,
        policy: crate::FetchPolicy,
    ) -> Result<crate::StoredImage> {
        let name = Self::store_name(tenant_path, id)?;
        let lock = self.store_lock(id);
        let _held = lock.lock().await;
        // Stored by another host meanwhile: that one stays.
        if self.exists(&self.spec(&name)).await? {
            return self.stored(&name).await;
        }
        let unique = format!("{}.{}", std::process::id(), random_suffix());
        let converted = self.work_dir.join(format!("{id}.{unique}.qcow2"));
        let _left = crate::store::Leftovers(vec![converted.clone()]);
        let sha256 = import(file, format, &converted, "qcow2", policy).await?;
        let got = crate::store::digest_file(&converted, &self.work_dir).await?;
        let src = crate::CopySource::File(&converted);
        let meta = [
            ("mst.sha256", sha256.as_str()),
            ("mst.digest", got.as_str()),
        ];
        crate::transfer::copy_to_rbd_name(self, &name, 0, &src, &meta).await?;
        let stored = self.stored(&name).await?;
        tracing::info!(image = id, pool = %self.pool, "image added to the store");
        Ok(stored)
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

    /// The store's RBD images: (name, image ID, whether a copy cut off).
    async fn store_images(&self) -> Result<Vec<(String, String, bool)>> {
        let list = self.rbd(&["ls", &self.pool]).await?;
        Ok(String::from_utf8_lossy(&list)
            .lines()
            .filter_map(|n| {
                let name = n.trim();
                let (id, part) = store_id(name)?;
                Some((name.to_owned(), id, part))
            })
            .collect())
    }

    /// Removes images not in `keep` stored before `before`, and copies cut
    /// off before `leftovers_before` (and their files here). Returns the IDs
    /// of the images removed.
    pub async fn prune_stored(
        &self,
        keep: &std::collections::HashSet<&str>,
        before: std::time::SystemTime,
        leftovers_before: std::time::SystemTime,
    ) -> Result<Vec<String>> {
        let secs = |t: std::time::SystemTime| {
            t.duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        };
        let mut removed = vec![];
        for (name, id, part) in self.store_images().await? {
            let spec = self.spec(&name);
            // Images without their time are not ours to judge.
            let Some(created) = self
                .meta_get(&spec, "mst.created")
                .await
                .and_then(|c| c.parse::<u64>().ok())
            else {
                continue;
            };
            let gone = match part {
                true => created < secs(leftovers_before),
                false => created < secs(before) && !keep.contains(id.as_str()),
            };
            if !gone {
                continue;
            }
            let lock = self.store_lock(&id);
            let _held = lock.lock().await;
            match self.rbd(&["rm", "--no-progress", &spec]).await {
                Err(Error::Tool { stderr, .. }) if stderr.contains("No such file") => {}
                r => {
                    r?;
                }
            }
            if !part {
                removed.push(id);
            }
        }
        // Downloads into the store cut off here.
        if let Ok(mut dir) = tokio::fs::read_dir(&self.work_dir).await {
            while let Ok(Some(e)) = dir.next_entry().await {
                let name = e.file_name().to_string_lossy().into_owned();
                let ours = [".part", ".qcow2", ".raw"]
                    .iter()
                    .any(|s| name.ends_with(s));
                let old = e
                    .metadata()
                    .await
                    .and_then(|m| m.modified())
                    .is_ok_and(|m| m < leftovers_before);
                if ours && old {
                    let _ = tokio::fs::remove_file(e.path()).await;
                }
            }
        }
        Ok(removed)
    }

    /// Removes metastratum's images from the Ceph pool (the store's; volumes
    /// are gone before a pool is removed, and anything else is not ours).
    /// Returns how many were removed.
    pub async fn purge(&self) -> Result<usize> {
        let mut removed = 0;
        for (name, _, _) in self.store_images().await? {
            self.rbd(&["rm", "--no-progress", &self.spec(&name)])
                .await?;
            removed += 1;
        }
        Ok(removed)
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

    /// Creates a blank volume. Existing volumes are left alone.
    pub async fn ensure_volume(&self, id: &str, size: u64) -> Result<()> {
        let vol = self.spec(&Self::volume_name(id)?);
        if self.exists(&vol).await? {
            return Ok(());
        }
        let mib = size.div_ceil(1 << 20).to_string();
        self.rbd(&["create", "--size", &mib, &vol]).await?;
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

/// The image ID in a store name (`store_<tenant>_..._<id>`, see
/// [`RbdPool::store_name`]), and whether it is a copy under way
/// (`<name>-part-<suffix>`); None for any other name.
fn store_id(name: &str) -> Option<(String, bool)> {
    let rest = name.strip_prefix("store_")?;
    let (base, part) = match rest.split_once("-part-") {
        Some((b, sfx)) if !sfx.is_empty() && sfx.bytes().all(|c| c.is_ascii_hexdigit()) => {
            (b, true)
        }
        Some(_) => return None,
        None => (rest, false),
    };
    let (labels, id) = base.rsplit_once('_')?;
    let hex = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    };
    let uuid = id.len() == 36
        && id.split('-').map(str::len).eq([8, 4, 4, 4, 12])
        && id.split('-').all(hex);
    (uuid && labels.split('_').all(hex)).then(|| (id.to_owned(), part))
}

#[cfg(test)]
mod store_name_tests {
    use super::*;

    #[test]
    fn store_names_round_trip_and_nothing_else_matches() {
        let id = "0199f0a2-7b3c-7d00-8000-0000000000a1";
        let name = RbdPool::store_name("01a1/02b2", id).unwrap();
        assert_eq!(name, format!("store_01a1_02b2_{id}"));
        assert_eq!(store_id(&name), Some((id.to_owned(), false)));
        assert_eq!(
            store_id(&format!("{name}-part-0123456789abcdef")),
            Some((id.to_owned(), true))
        );
        for other in [
            "store_x",
            "vol-1",
            "store_01a1_notanid",
            "store__",
            "store_01a1_ID",
        ] {
            assert_eq!(store_id(other), None, "{other}");
        }
        assert!(RbdPool::store_name("01a1/../x", id).is_err());
    }
}
