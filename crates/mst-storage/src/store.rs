//! The image store: images downloaded once, when they are added, and copied
//! from host to host from then on. Each is a qcow2 file of its own (no
//! backing file) in a pool of files (the local pool, or a shared one an
//! image is kept in), under its tenant's lineage:
//! `images/<tenant>/<sub-tenant>/.../<image id>.qcow2`, tenants named by ID,
//! with `<image id>.json` beside it: the SHA-256 of the file downloaded,
//! and the image's digest (the SHA-256 of the disk it holds, as a raw disk),
//! which every copy is checked against. Ceph pools keep them as RBD images
//! (see [`crate::RbdPool::store_name`]). VMs' disks are copies of them, so
//! an image here is only ever read, to make a disk or another copy.
//!
//! An image in a store is never replaced: hosts sharing a pool may store
//! the same one at once, and the first to finish keeps it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::transfer::CopySource;
use crate::{Error, FetchPolicy, Result, img_info, random_suffix, run, safe_id, set_mode};

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
#[derive(Clone, Debug, Serialize)]
pub struct StoredImage {
    /// The file's size.
    pub size_bytes: u64,
    /// The size of the disk it holds.
    pub virtual_bytes: u64,
    /// SHA-256 of the file downloaded from the image's URL (hex); empty when
    /// it was not downloaded into this store.
    pub sha256: String,
    /// SHA-256 of the disk it holds, read as a raw disk (hex).
    pub digest: String,
}

/// What is kept beside an image.
#[derive(Default, Serialize, Deserialize)]
pub(crate) struct Meta {
    #[serde(default)]
    pub(crate) sha256: String,
    #[serde(default)]
    pub(crate) digest: String,
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

/// Refuses an image that does not hold what it should.
pub(crate) fn check_digest(id: &str, got: &str, want: Option<&str>) -> Result<()> {
    match want {
        Some(want) if !got.eq_ignore_ascii_case(want) => Err(Error::InvalidImage(format!(
            "image {id} does not hold what was first downloaded (its digest is {got}, not {want})"
        ))),
        _ => Ok(()),
    }
}

/// Files removed once dropped: what a download or copy leaves, also when
/// it is cut off (its image deleted meanwhile).
pub(crate) struct Leftovers(pub(crate) Vec<PathBuf>);

impl Drop for Leftovers {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Makes a directory of the store (private), or marks it in use if it is
/// there: pruning leaves directories in use alone (see
/// [`ImageStore::prune`]), so one is not removed, as empty, just before a
/// file is written into it.
pub(crate) async fn ensure_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let d = dir.to_owned();
    tokio::task::spawn_blocking(move || {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&d)?;
        std::fs::File::open(&d)?.set_modified(SystemTime::now())
    })
    .await
    .map_err(|e| Error::Io(std::io::Error::other(e)))??;
    Ok(())
}

/// SHA-256 of a qcow2 file's disk, read as a raw disk: converted to a
/// (sparse) raw file in `scratch`, which is then read.
pub(crate) async fn digest_file(path: &Path, scratch: &Path) -> Result<String> {
    ensure_dir(scratch).await?;
    let raw = scratch.join(format!("{}.raw", random_suffix()));
    let _left = Leftovers(vec![raw.clone()]);
    let (p, r) = (path.to_string_lossy(), raw.to_string_lossy());
    run(
        "qemu-img",
        &["convert", "-q", "-f", "qcow2", "-O", "raw", &p, &r],
    )
    .await?;
    hash_reader(tokio::fs::File::open(&raw).await?).await
}

/// SHA-256 of what a reader yields (hex).
pub(crate) async fn hash_reader(mut r: impl tokio::io::AsyncRead + Unpin) -> Result<String> {
    use sha2::Digest;
    use tokio::io::AsyncReadExt;
    let mut hash = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = r.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(hex::encode(hash.finalize()))
}

/// Puts `tmp` in place as `dest` unless something is there already (the
/// first to finish keeps it). Returns whether it was put in place.
async fn publish(tmp: &Path, dest: &Path) -> Result<bool> {
    let won = match tokio::fs::hard_link(tmp, dest).await {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(e) => return Err(e.into()),
    };
    let _ = tokio::fs::remove_file(tmp).await;
    Ok(won)
}

#[derive(Clone, Debug)]
pub struct ImageStore {
    root: PathBuf,
    /// Where digests are worked out: a raw copy of a disk, sparse, is read
    /// there. On the host's own disk.
    scratch: PathBuf,
}

impl ImageStore {
    /// The store of the pool of files at `root` (the local pool's is the
    /// agent's data directory).
    pub fn new(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref().join("images");
        Self {
            scratch: root.join(".scratch"),
            root,
        }
    }

    /// The same store, working out digests in `dir` (for a shared pool's:
    /// on the host's own disk).
    pub fn scratch_in(mut self, dir: &Path) -> Self {
        self.scratch = dir.to_owned();
        self
    }

    /// Where this store works out digests.
    pub fn scratch(&self) -> &Path {
        &self.scratch
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

    fn meta_path(&self, tenant_path: &str, id: &str) -> Result<PathBuf> {
        Ok(self
            .tenant_dir(tenant_path)?
            .join(format!("{}.json", safe_id(id)?)))
    }

    async fn make_dir(&self, tenant_path: &str) -> Result<PathBuf> {
        let dir = self.tenant_dir(tenant_path)?;
        ensure_dir(&dir).await?;
        Ok(dir)
    }

    async fn write_meta(&self, tenant_path: &str, id: &str, meta: &Meta) -> Result<()> {
        let path = self.meta_path(tenant_path, id)?;
        let tmp = path.with_extension(format!("{}.tmp", random_suffix()));
        let json = serde_json::to_vec(meta).map_err(|e| Error::Io(std::io::Error::other(e)))?;
        tokio::fs::write(&tmp, json).await?;
        set_mode(&tmp, 0o600).await?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(())
    }

    /// What the store holds of an image: its sizes, and what is kept beside
    /// it (worked out again, should that be missing).
    async fn stored(&self, tenant_path: &str, id: &str) -> Result<StoredImage> {
        let path = self.path(tenant_path, id)?;
        let meta: Option<Meta> = tokio::fs::read(self.meta_path(tenant_path, id)?)
            .await
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .filter(|m: &Meta| !m.digest.is_empty());
        let meta = match meta {
            Some(m) => m,
            None => {
                let m = Meta {
                    sha256: String::new(),
                    digest: digest_file(&path, &self.scratch).await?,
                };
                self.write_meta(tenant_path, id, &m).await?;
                m
            }
        };
        Ok(StoredImage {
            size_bytes: tokio::fs::metadata(&path).await?.len(),
            virtual_bytes: img_info(&path, Some("qcow2")).await?.virtual_size,
            sha256: meta.sha256,
            digest: meta.digest,
        })
    }

    /// The image if it is here (the caller holds its lock). With `digest`,
    /// one holding something else goes: left by a download or copy never
    /// recorded (cut off as its agent reconnected, say), it may be of
    /// what the image's URL served before; the digest recorded is what
    /// counts.
    async fn existing(
        &self,
        tenant_path: &str,
        id: &str,
        digest: Option<&str>,
    ) -> Result<Option<StoredImage>> {
        if !tokio::fs::try_exists(self.path(tenant_path, id)?).await? {
            return Ok(None);
        }
        let stored = self.stored(tenant_path, id).await?;
        match check_digest(id, &stored.digest, digest) {
            Ok(()) => Ok(Some(stored)),
            Err(e) => {
                tracing::warn!(image = id, error = %e, "replacing a copy holding something else");
                self.remove_files(tenant_path, id).await?;
                Ok(None)
            }
        }
    }

    /// Whether the image is here.
    pub async fn has(&self, tenant_path: &str, id: &str) -> Result<bool> {
        Ok(tokio::fs::try_exists(self.path(tenant_path, id)?).await?)
    }

    /// The image, if it is here; with `digest`, it must hold that.
    pub async fn held(
        &self,
        tenant_path: &str,
        id: &str,
        digest: Option<&str>,
    ) -> Result<Option<StoredImage>> {
        let lock = lock(id);
        let _held = lock.lock().await;
        if !tokio::fs::try_exists(self.path(tenant_path, id)?).await? {
            return Ok(None);
        }
        let stored = self.stored(tenant_path, id).await?;
        check_digest(id, &stored.digest, digest)?;
        Ok(Some(stored))
    }

    /// Adds an image to the store from a file this host received (see
    /// [`crate::fetch`] for what images may not do), converted to qcow2,
    /// unless it is here already.
    pub async fn add(
        &self,
        tenant_path: &str,
        id: &str,
        file: &Path,
        format: &str,
        policy: FetchPolicy,
    ) -> Result<StoredImage> {
        let dest = self.path(tenant_path, id)?;
        let lock = lock(id);
        let _held = lock.lock().await;
        if let Some(stored) = self.existing(tenant_path, id, None).await? {
            return Ok(stored);
        }
        self.make_dir(tenant_path).await?;
        let converted = dest.with_extension(format!("{}.tmp", random_suffix()));
        let _left = Leftovers(vec![converted.clone()]);
        let sha256 = crate::import(file, format, &converted, "qcow2", policy).await?;
        let got = digest_file(&converted, &self.scratch).await?;
        set_mode(&converted, 0o600).await?;
        if publish(&converted, &dest).await? {
            let meta = Meta {
                sha256,
                digest: got,
            };
            self.write_meta(tenant_path, id, &meta).await?;
        }
        let stored = self.stored(tenant_path, id).await?;
        tracing::info!(image = id, path = %dest.display(), "image added to the store");
        Ok(stored)
    }

    /// Copies an image into the store (from another host serving it, or
    /// a shared pool's store), unless it is here already. With `digest`,
    /// the copy must hold that, or it is thrown away.
    pub async fn copy_in(
        &self,
        tenant_path: &str,
        id: &str,
        src: &CopySource<'_>,
        digest: Option<&str>,
    ) -> Result<StoredImage> {
        let dest = self.path(tenant_path, id)?;
        let lock = lock(id);
        let _held = lock.lock().await;
        if let Some(stored) = self.existing(tenant_path, id, digest).await? {
            return Ok(stored);
        }
        self.make_dir(tenant_path).await?;
        let tmp = dest.with_extension(format!("{}.tmp", random_suffix()));
        let _left = Leftovers(vec![tmp.clone()]);
        crate::transfer::convert_to(src, &tmp).await?;
        let got = digest_file(&tmp, &self.scratch).await?;
        check_digest(id, &got, digest)?;
        set_mode(&tmp, 0o600).await?;
        if publish(&tmp, &dest).await? {
            let meta = Meta {
                sha256: String::new(),
                digest: got,
            };
            self.write_meta(tenant_path, id, &meta).await?;
        }
        let stored = self.stored(tenant_path, id).await?;
        tracing::info!(image = id, "image copied into the store");
        Ok(stored)
    }

    /// Removes an image from the store. Its tenants' directories stay:
    /// on NFS, hosts that saw one go may not see it made again for a while
    /// (they cache that it is gone), so they go only as the store is
    /// pruned or purged.
    pub async fn remove(&self, tenant_path: &str, id: &str) -> Result<()> {
        let lock = lock(id);
        let _held = lock.lock().await;
        self.remove_files(tenant_path, id).await
    }

    /// Removes an image's files (the caller holds its lock).
    async fn remove_files(&self, tenant_path: &str, id: &str) -> Result<()> {
        for path in [
            self.path(tenant_path, id)?,
            self.meta_path(tenant_path, id)?,
        ] {
            match tokio::fs::remove_file(&path).await {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
        }
        Ok(())
    }

    /// Removes the tenants' directories left empty, last used before
    /// `before` (if given): one just made, or about to be written into,
    /// stays (see [`ensure_dir`]).
    async fn remove_empty_dirs(&self, before: Option<SystemTime>) -> Result<()> {
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
            if let Some(before) = before {
                match tokio::fs::metadata(&d).await.and_then(|m| m.modified()) {
                    Ok(at) if at < before => {}
                    _ => continue,
                }
            }
            let _ = tokio::fs::remove_dir(d).await;
        }
        Ok(())
    }

    /// Every file in the store's directories: (path, the image ID it is of,
    /// unless a download, copy or digest under way, when last written).
    async fn files(&self) -> Result<Vec<(PathBuf, Option<String>, SystemTime)>> {
        let mut out = vec![];
        let mut dirs = vec![];
        // Directories only: files directly in images/ are not the store's.
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
                        .or_else(|| name.strip_suffix(".json"))
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
    /// of downloads, copies or digests cut off, written before
    /// `leftovers_before`. Returns the IDs of the images removed.
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
            if let Some(id) = id
                && path.extension().is_some_and(|e| e == "qcow2")
            {
                removed.push(id);
            }
        }
        self.remove_empty_dirs(Some(before)).await?;
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
        self.remove_empty_dirs(None).await?;
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
        // A copy that is not the image is thrown away.
        let wrong = "0".repeat(64);
        assert!(matches!(
            shared_pool
                .copy_stored_to(&store, T, ID, Some(&wrong))
                .await,
            Err(Error::InvalidImage(_))
        ));
        assert!(!store.has(T, ID).await.unwrap());
        let kept_digest = ImageStore::new(shared.root())
            .held(T, ID, None)
            .await
            .unwrap()
            .unwrap()
            .digest;
        let copied = shared_pool
            .copy_stored_to(&store, T, ID, Some(&kept_digest))
            .await
            .unwrap();
        assert_eq!(copied.digest, kept_digest);
        // The digest is kept beside it, and checked when asked again.
        assert!(store.meta_path(T, ID).unwrap().exists());
        assert!(matches!(
            shared_pool
                .copy_stored_to(&store, T, ID, Some(&wrong))
                .await,
            Err(Error::InvalidImage(_))
        ));
        // Never replaced: a second copy, finished later, is thrown away.
        let other = root.join("other.qcow2");
        crate::convert(&raw, "raw", &other, "qcow2").await.unwrap();
        let before = tokio::fs::metadata(store.path(T, ID).unwrap())
            .await
            .unwrap();
        let tmp = root.join("late.tmp");
        tokio::fs::copy(&other, &tmp).await.unwrap();
        assert!(!publish(&tmp, &store.path(T, ID).unwrap()).await.unwrap());
        assert!(!tmp.exists());
        use std::os::unix::fs::MetadataExt;
        let after = tokio::fs::metadata(store.path(T, ID).unwrap())
            .await
            .unwrap();
        assert_eq!(before.ino(), after.ino());
        assert!(local_pool.has_stored(T, ID).await.unwrap());
        let other = "0199f0a2-7b3c-7d00-8000-0000000000b2";
        local_pool
            .volume_from_stored(other, 2 << 20, &local_pool, T, ID)
            .await
            .unwrap();

        // The image can go: the volumes keep its data.
        shared_pool.remove_stored(T, ID).await.unwrap();
        local_pool.remove_stored(T, ID).await.unwrap();
        let fresh = ImageStore::new(root.join("fresh"));
        assert!(matches!(
            shared_pool.copy_stored_to(&fresh, T, ID, None).await,
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
