//! Images coming to this host (uploaded, or downloaded by the controller):
//! a file per image in `<data dir>/uploads`, written as its parts arrive
//! (ReceiveImage), until it is added to a store (StoreImage) or the image is
//! deleted. Files left behind (cut off, their image gone while this host
//! was away) are pruned.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

/// An upload untouched this long is gone (the controller gives up on one
/// sooner).
const STALE: Duration = Duration::from_secs(2 * 24 * 3600);

fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join("uploads")
}

/// Where the image's upload is kept.
pub fn path(data_dir: &Path, image_id: &str) -> Result<PathBuf> {
    let id: uuid::Uuid = image_id.parse().context("not an image ID")?;
    Ok(dir(data_dir).join(format!("{}.upload", id.hyphenated())))
}

/// Writes a part of the image's file at `offset` (`total`: its size, or 0
/// when not known): one at 0 starts it over, one beyond what is here is not
/// written (a part was lost: the controller sends from what is here).
/// Returns how much of it is here.
pub async fn receive(
    data_dir: &Path,
    image_id: &str,
    offset: u64,
    data: &[u8],
    total: u64,
    max_bytes: u64,
) -> Result<u64> {
    let end = offset + data.len() as u64;
    if total > max_bytes || end > max_bytes {
        bail!(
            "the image is larger than the {} GiB this host takes",
            max_bytes >> 30
        );
    }
    if total > 0 && end > total {
        bail!("a part beyond the file's {total} bytes");
    }
    let path = path(data_dir, image_id)?;
    {
        use std::os::unix::fs::DirBuilderExt;
        let d = dir(data_dir);
        tokio::task::spawn_blocking(move || {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(d)
        })
        .await??;
    }
    let here = match tokio::fs::metadata(&path).await {
        Ok(m) => m.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => return Err(e.into()),
    };
    if offset > here || data.is_empty() {
        return Ok(here);
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(offset == 0)
        .open(&path)
        .await
        .with_context(|| format!("opening {}", path.display()))?;
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    file.write_all(data).await?;
    file.flush().await?;
    let here = if offset == 0 { end } else { here.max(end) };
    if here == total {
        // All of it: on disk before it is said to be.
        file.sync_all().await?;
        tracing::info!(image = image_id, bytes = total, "upload received");
    }
    Ok(here)
}

/// Removes the image's upload, if any.
pub async fn remove(data_dir: &Path, image_id: &str) -> Result<()> {
    match tokio::fs::remove_file(path(data_dir, image_id)?).await {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// Removes uploads untouched for two days.
pub async fn prune(data_dir: &Path) -> Result<()> {
    let mut entries = match tokio::fs::read_dir(dir(data_dir)).await {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    while let Some(e) = entries.next_entry().await? {
        let stale = e
            .metadata()
            .await?
            .modified()
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_some_and(|age| age > STALE);
        if stale {
            tracing::info!(file = %e.path().display(), "removing an upload left behind");
            tokio::fs::remove_file(e.path()).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn parts_arrive_in_order_or_are_resent() {
        let dir = std::env::temp_dir().join(format!("mst-uploads-{}", std::process::id()));
        let id = mst_core::ids::ImageId::new().to_string();
        let max = 1 << 20;
        assert_eq!(receive(&dir, &id, 0, b"abcd", 10, max).await.unwrap(), 4);
        // Sent again (the answer was lost): taken as it is.
        assert_eq!(receive(&dir, &id, 0, b"abcd", 10, max).await.unwrap(), 4);
        assert_eq!(receive(&dir, &id, 4, b"efg", 10, max).await.unwrap(), 7);
        // Beyond what is here: not written.
        assert_eq!(receive(&dir, &id, 8, b"ij", 10, max).await.unwrap(), 7);
        assert_eq!(receive(&dir, &id, 7, b"hij", 10, max).await.unwrap(), 10);
        let p = path(&dir, &id).unwrap();
        assert_eq!(tokio::fs::read(&p).await.unwrap(), b"abcdefghij");
        // Past the end, or too large for this host.
        assert!(receive(&dir, &id, 9, b"kl", 10, max).await.is_err());
        assert!(receive(&dir, &id, 0, b"a", max + 1, max).await.is_err());
        // Of a size not known: up to what this host takes.
        assert_eq!(receive(&dir, &id, 10, b"k", 0, max).await.unwrap(), 11);
        assert!(receive(&dir, &id, max, b"z", 0, max).await.is_err());
        // Started over.
        assert_eq!(receive(&dir, &id, 0, b"xy", 3, max).await.unwrap(), 2);
        assert_eq!(tokio::fs::read(&p).await.unwrap(), b"xy");
        assert!(path(&dir, "../etc").is_err());
        remove(&dir, &id).await.unwrap();
        remove(&dir, &id).await.unwrap();
        prune(&dir).await.unwrap();
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
