//! The image store: images downloaded once, when they are added, and copied
//! from host to host from then on. Each is a qcow2 file of its own (no
//! backing file) in a pool of files (the local pool, or a shared one an
//! image is kept in), under its tenant's lineage:
//! `images/<tenant>/<sub-tenant>/.../<image id>.qcow2`, tenants named by ID.
//! Ceph pools keep them as RBD images (see [`crate::RbdPool::store_name`]).
//! VMs' disks are copies of them, so an image here is only ever read, to
//! make a disk or another host's copy.
//!
//! Volumes made before the store have a cached image as their backing file,
//! directly in `images/` (see [`crate::FilePool::ensure_image`]); those files
//! are left to that cache.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::SystemTime;

use crate::transfer::CopySource;
use crate::{Error, FetchPolicy, Result, img_info, random_suffix, safe_id, set_mode};

/// One fetch or copy at a time per image, on this host.
static LOCKS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(Mutex::default);

fn lock(id: &str) -> Arc<tokio::sync::Mutex<()>> {
    LOCKS
        .lock()
        .unwrap()
        .entry(id.to_owned())
        .or_default()
        .clone()
}

/// An image in the store.
#[derive(Clone, Debug, serde::Serialize)]
pub struct StoredImage {
    /// The file's size.
    pub size_bytes: u64,
    /// The size of the disk it holds.
    pub virtual_bytes: u64,
    /// SHA-256 of the file downloaded from the image's URL (hex); empty when
    /// not downloaded just now.
    pub sha256: String,
}

/// A tenant's lineage (see [`ImageStore`]), checked.
pub(crate) fn tenant_labels(tenant_path: &str) -> Result<Vec<&str>> {
    let labels: Vec<&str> = tenant_path.split('/').collect();
    let ok = |l: &&str| {
        (1..=64).contains(&l.len())
            && l.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    if labels.len() > 64 || !labels.iter().all(ok) {
        return Err(Error::InvalidImage(format!(
            "bad tenant path {tenant_path:?}"
        )));
    }
    Ok(labels)
}

#[derive(Clone, Debug)]
pub struct ImageStore {
    root: PathBuf,
}

impl ImageStore {
    /// The store of the pool of files at `root` (the local pool's is the
    /// agent's data directory).
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().join("images"),
        }
    }

    /// The directory of a tenant's images; `tenant_path` is its lineage,
    /// IDs (simple hex form) separated by '/'.
    fn tenant_dir(&self, tenant_path: &str) -> Result<PathBuf> {
        Ok(tenant_labels(tenant_path)?
            .iter()
            .fold(self.root.clone(), |p, l| p.join(l)))
    }

    pub fn path(&self, tenant_path: &str, id: &str) -> Result<PathBuf> {
        Ok(self
            .tenant_dir(tenant_path)?
            .join(format!("{}.qcow2", safe_id(id)?)))
    }

    async fn make_dir(&self, tenant_path: &str) -> Result<PathBuf> {
        use std::os::unix::fs::DirBuilderExt;
        let dir = self.tenant_dir(tenant_path)?;
        let mut b = std::fs::DirBuilder::new();
        b.recursive(true).mode(0o700);
        let d = dir.clone();
        tokio::task::spawn_blocking(move || b.create(d))
            .await
            .map_err(|e| Error::Io(std::io::Error::other(e)))??;
        Ok(dir)
    }

    async fn stored(path: &Path, sha256: String) -> Result<StoredImage> {
        Ok(StoredImage {
            size_bytes: tokio::fs::metadata(path).await?.len(),
            virtual_bytes: img_info(path, Some("qcow2")).await?.virtual_size,
            sha256,
        })
    }

    /// Whether the image is here.
    pub async fn has(&self, tenant_path: &str, id: &str) -> Result<bool> {
        Ok(tokio::fs::try_exists(self.path(tenant_path, id)?).await?)
    }

    /// Downloads an image into the store, converted to qcow2, unless it is
    /// here already. See [`crate::fetch`] for what downloads may not do.
    /// With `sha256`, the download must be that file.
    pub async fn fetch(
        &self,
        tenant_path: &str,
        id: &str,
        url: &str,
        format: &str,
        policy: FetchPolicy,
        sha256: Option<&str>,
    ) -> Result<StoredImage> {
        let dest = self.path(tenant_path, id)?;
        let lock = lock(id);
        let _held = lock.lock().await;
        if tokio::fs::try_exists(&dest).await? {
            return Self::stored(&dest, String::new()).await;
        }
        self.make_dir(tenant_path).await?;
        let unique = random_suffix();
        let part = dest.with_extension(format!("{unique}.part"));
        let converted = dest.with_extension(format!("{unique}.tmp"));
        let result = async {
            let hash =
                crate::fetch(url, format, &part, &converted, "qcow2", policy, sha256).await?;
            let _ = tokio::fs::remove_file(&part).await;
            set_mode(&converted, 0o600).await?;
            tokio::fs::rename(&converted, &dest).await?;
            Self::stored(&dest, hash).await
        }
        .await;
        let _ = tokio::fs::remove_file(&part).await;
        let _ = tokio::fs::remove_file(&converted).await;
        if result.is_ok() {
            tracing::info!(image = id, path = %dest.display(), "image downloaded into the store");
        }
        result
    }

    /// Copies an image into the store (from another host serving it, or
    /// a shared pool's store), unless it is here already.
    pub async fn copy_in(
        &self,
        tenant_path: &str,
        id: &str,
        src: &CopySource<'_>,
    ) -> Result<StoredImage> {
        let dest = self.path(tenant_path, id)?;
        let lock = lock(id);
        let _held = lock.lock().await;
        if tokio::fs::try_exists(&dest).await? {
            return Self::stored(&dest, String::new()).await;
        }
        self.make_dir(tenant_path).await?;
        let tmp = dest.with_extension(format!("{}.tmp", random_suffix()));
        let result = async {
            crate::transfer::convert_to(src, &tmp).await?;
            set_mode(&tmp, 0o600).await?;
            tokio::fs::rename(&tmp, &dest).await?;
            Self::stored(&dest, String::new()).await
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&tmp).await;
        } else {
            tracing::info!(image = id, "image copied into the store");
        }
        result
    }

    /// Removes an image from the store. Its tenants' directories stay:
    /// on NFS, hosts that saw one go may not see it made again for a while
    /// (they cache that it is gone), so they go only as the store is
    /// pruned or purged.
    pub async fn remove(&self, tenant_path: &str, id: &str) -> Result<()> {
        let path = self.path(tenant_path, id)?;
        let lock = lock(id);
        let _held = lock.lock().await;
        match tokio::fs::remove_file(&path).await {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// Removes the tenants' directories left empty.
    async fn remove_empty_dirs(&self) -> Result<()> {
        // Deepest first: a directory may be empty once its own are gone.
        let mut dirs = vec![];
        let mut todo = vec![self.root.clone()];
        while let Some(dir) = todo.pop() {
            let mut d = match tokio::fs::read_dir(&dir).await {
                Ok(d) => d,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            while let Some(e) = d.next_entry().await? {
                if e.file_type().await?.is_dir() {
                    todo.push(e.path());
                    dirs.push(e.path());
                }
            }
        }
        dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
        for d in dirs {
            let _ = tokio::fs::remove_dir(d).await;
        }
        Ok(())
    }

    /// Every file in the tenants' directories: (path, image ID if it is an
    /// image rather than a download or copy under way, when last written).
    async fn files(&self) -> Result<Vec<(PathBuf, Option<String>, SystemTime)>> {
        let mut out = vec![];
        let mut dirs = vec![];
        // Tenants' directories only: files directly in images/ are the old
        // cache's.
        match tokio::fs::read_dir(&self.root).await {
            Ok(mut d) => {
                while let Some(e) = d.next_entry().await? {
                    if e.file_type().await?.is_dir() {
                        dirs.push(e.path());
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e.into()),
        }
        while let Some(dir) = dirs.pop() {
            let mut d = match tokio::fs::read_dir(&dir).await {
                Ok(d) => d,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            while let Some(e) = d.next_entry().await? {
                let kind = e.file_type().await?;
                if kind.is_dir() {
                    dirs.push(e.path());
                } else if kind.is_file() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let id = name
                        .strip_suffix(".qcow2")
                        .filter(|id| safe_id(id).is_ok())
                        .map(str::to_owned);
                    let modified = e.metadata().await?.modified()?;
                    out.push((e.path(), id, modified));
                }
            }
        }
        Ok(out)
    }

    /// Removes images not in `keep` written before `before`, and the files
    /// of downloads or copies cut off, written before `leftovers_before`.
    /// Returns the IDs of the images removed.
    pub async fn prune(
        &self,
        keep: &HashSet<&str>,
        before: SystemTime,
        leftovers_before: SystemTime,
    ) -> Result<Vec<String>> {
        let mut removed = vec![];
        for (path, id, modified) in self.files().await? {
            let old = match &id {
                Some(_) => modified < before,
                None => modified < leftovers_before,
            };
            if !old || id.as_deref().is_some_and(|i| keep.contains(i)) {
                continue;
            }
            let held = id.as_deref().map(lock);
            let _held = match &held {
                Some(l) => Some(l.lock().await),
                None => None,
            };
            match tokio::fs::remove_file(&path).await {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
            removed.extend(id);
        }
        self.remove_empty_dirs().await?;
        Ok(removed)
    }

    /// Removes everything in the store. Returns how many files went.
    pub async fn purge(&self) -> Result<usize> {
        let files = self.files().await?;
        for (path, _, _) in &files {
            match tokio::fs::remove_file(path).await {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
        }
        self.remove_empty_dirs().await?;
        Ok(files.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = "0199f0a27b3c7d008000000000000001/0199f0a27b3c7d008000000000000002";
    const ID: &str = "0199f0a2-7b3c-7d00-8000-0000000000a1";

    #[test]
    fn paths_stay_in_the_store() {
        let store = ImageStore::new("/var/lib/metastratum");
        assert_eq!(
            store.path(T, ID).unwrap(),
            Path::new("/var/lib/metastratum/images")
                .join(T)
                .join(format!("{ID}.qcow2"))
        );
        for bad in ["", "..", "a/../b", "/abs", "ABC", "a//b", "a/b/", "x y"] {
            assert!(store.path(bad, ID).is_err(), "{bad:?}");
        }
        assert!(store.path(T, "../x").is_err());
    }

    #[tokio::test]
    async fn pruning_keeps_what_exists_and_what_is_new() {
        let root = std::env::temp_dir().join(format!("mst-store-{}", std::process::id()));
        let store = ImageStore::new(&root);
        let other = "0199f0a2-7b3c-7d00-8000-0000000000a2";
        store.make_dir(T).await.unwrap();
        for id in [ID, other] {
            tokio::fs::write(store.path(T, id).unwrap(), "x")
                .await
                .unwrap();
        }
        let part = store.path(T, ID).unwrap().with_extension("123.part");
        tokio::fs::write(&part, "x").await.unwrap();
        // The old cache's files are not the store's.
        let cached = root.join("images").join(format!("{other}.qcow2"));
        tokio::fs::write(&cached, "x").await.unwrap();

        let keep: HashSet<&str> = [ID].into();
        let past = SystemTime::now() - std::time::Duration::from_secs(60);
        assert!(store.prune(&keep, past, past).await.unwrap().is_empty());
        let later = SystemTime::now() + std::time::Duration::from_secs(60);
        assert_eq!(store.prune(&keep, later, past).await.unwrap(), [other]);
        assert!(store.has(T, ID).await.unwrap());
        assert!(part.exists(), "a download under way stays");
        assert!(store.prune(&keep, later, later).await.unwrap().is_empty());
        assert!(!part.exists());
        assert!(cached.exists());

        store.remove(T, ID).await.unwrap();
        store.remove(T, ID).await.unwrap();
        // The tenants' directories stay until pruned.
        let top = root.join("images").join(T.split('/').next().unwrap());
        assert!(top.exists());
        assert!(store.prune(&keep, later, later).await.unwrap().is_empty());
        assert!(!top.exists());
        assert!(cached.exists());
        assert_eq!(store.purge().await.unwrap(), 0);
        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    /// Exercises qemu-img when it is installed: volumes made from an image
    /// (in the local store, or a shared pool's) hold its data and stand on
    /// their own, and a shared pool's image copies into the local store.
    #[tokio::test]
    async fn volumes_are_copies_of_images() {
        if tokio::process::Command::new("qemu-img")
            .arg("--version")
            .output()
            .await
            .is_err()
        {
            return eprintln!("qemu-img not installed; skipping");
        }
        let root = std::env::temp_dir().join(format!("mst-store-vol-{}", std::process::id()));
        let local = crate::FilePool::new(root.join("local"));
        local.init().await.unwrap();
        let shared = crate::FilePool::new(root.join("shared"));
        shared.init().await.unwrap();
        // The image is kept in the shared pool.
        let kept = ImageStore::new(shared.root());
        kept.make_dir(T).await.unwrap();
        let raw = root.join("image.raw");
        tokio::fs::write(&raw, {
            let mut b = b"image data".to_vec();
            b.resize(1 << 20, 0);
            b
        })
        .await
        .unwrap();
        crate::convert(&raw, "raw", &kept.path(T, ID).unwrap(), "qcow2")
            .await
            .unwrap();
        let (local_pool, shared_pool) = (
            crate::Pool::File(local.clone()),
            crate::Pool::File(shared.clone()),
        );
        assert!(shared_pool.has_stored(T, ID).await.unwrap());

        let read = |path: PathBuf| {
            let out = root.join(format!("{}.raw", random_suffix()));
            async move {
                crate::convert(&path, "qcow2", &out, "raw").await.unwrap();
                tokio::fs::read(&out).await.unwrap()
            }
        };
        // Straight from the shared pool's store.
        let vol = "0199f0a2-7b3c-7d00-8000-0000000000b1";
        assert!(matches!(
            local_pool
                .volume_from_stored(vol, 4096, &shared_pool, T, ID)
                .await,
            Err(Error::TooSmall { .. })
        ));
        for _ in 0..2 {
            local_pool
                .volume_from_stored(vol, 2 << 20, &shared_pool, T, ID)
                .await
                .unwrap();
        }
        let info = img_info(&local.volume_path(vol).unwrap(), None)
            .await
            .unwrap();
        assert_eq!((info.virtual_size, info.backing_filename), (2 << 20, None));

        // Copied into the local store, and made from there.
        let store = ImageStore::new(local.root());
        shared_pool.copy_stored_to(&store, T, ID).await.unwrap();
        assert!(local_pool.has_stored(T, ID).await.unwrap());
        let other = "0199f0a2-7b3c-7d00-8000-0000000000b2";
        local_pool
            .volume_from_stored(other, 2 << 20, &local_pool, T, ID)
            .await
            .unwrap();

        // The image can go: the volumes keep its data.
        shared_pool.remove_stored(T, ID).await.unwrap();
        local_pool.remove_stored(T, ID).await.unwrap();
        assert!(matches!(
            shared_pool.copy_stored_to(&store, T, ID).await,
            Err(Error::MissingImage(_))
        ));
        for v in [vol, other] {
            assert!(
                read(local.volume_path(v).unwrap())
                    .await
                    .starts_with(b"image data")
            );
        }
        // Purging a pool takes its store along.
        let shared_pool = crate::Pool::File(shared.clone());
        kept.make_dir(T).await.unwrap();
        tokio::fs::write(kept.path(T, ID).unwrap(), "x")
            .await
            .unwrap();
        assert_eq!(shared_pool.purge().await.unwrap(), 1);
        assert!(!kept.has(T, ID).await.unwrap());
        let _ = tokio::fs::remove_dir_all(&root).await;
    }
}
