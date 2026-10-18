//! Backups' disk files: qcow2 files in a repository, an incremental one
//! holding only what changed since its parent's copy, which is its backing
//! file (named relative to it, `../<parent>/<index>.qcow2`, so the
//! repository may be mounted anywhere). Read through, each is a whole disk.

use std::path::Path;

use crate::{Error, Result, random_suffix, run};

/// Makes an empty qcow2 file of `size` bytes for a backup job to fill:
/// backed by `backing` (relative to the file) when it is to hold changes.
pub async fn create_target(path: &Path, size: u64, backing: Option<&str>) -> Result<()> {
    let (p, size) = (path.to_string_lossy(), size.to_string());
    let mut args = vec!["create", "-q", "-f", "qcow2"];
    if let Some(b) = backing {
        args.extend(["-b", b, "-F", "qcow2"]);
    }
    args.extend([&*p, &*size]);
    run("qemu-img", &args).await?;
    Ok(())
}

/// Stores at `dest` what the qcow2 file `volume` (read through its own
/// backing files) holds that differs from `parent` (a backup's copy,
/// relative to `dest`): the changes since that backup, found by comparing
/// the two in full. For a volume no running QEMU has open.
pub async fn store_changes(volume: &Path, parent: &str, dest: &Path, size: u64) -> Result<()> {
    let tmp = dest.with_extension(format!("{}.part", random_suffix()));
    let result = async {
        // An empty file over the volume reads as the volume; moved onto the
        // parent, it takes in whatever reads differently there.
        let (v, t, size) = (
            volume.to_string_lossy(),
            tmp.to_string_lossy(),
            size.to_string(),
        );
        run(
            "qemu-img",
            &[
                "create", "-q", "-f", "qcow2", "-b", &v, "-F", "qcow2", &t, &size,
            ],
        )
        .await?;
        run(
            "qemu-img",
            &[
                "rebase", "-q", "-f", "qcow2", "-b", parent, "-F", "qcow2", &t,
            ],
        )
        .await?;
        tokio::fs::rename(&tmp, dest).await?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    result
}

/// The backing file a qcow2 file names, as written in it (relative ones as
/// such); `None` for one that has none.
pub async fn backing_of(path: &Path) -> Result<Option<String>> {
    Ok(crate::qcow2_backing_file(path).await?)
}

/// Merges `child` (a qcow2 file backed by `base`) with `base`, for `base`'s
/// backup to go: what `child` holds is committed into `base`, which then
/// takes `child`'s place. `child` reads the same throughout; `base` no longer
/// reads as it did. Run again after an interruption, it finishes the job.
pub async fn merge_into_child(child: &Path, base: &Path) -> Result<()> {
    let c = child.to_string_lossy();
    // `-d`: the child is replaced, no need to empty it.
    run("qemu-img", &["commit", "-q", "-d", "-f", "qcow2", &c]).await?;
    tokio::fs::rename(base, child).await?;
    Ok(())
}

/// The names of the dirty bitmaps (change tracking) in a qcow2 file no QEMU
/// has open.
pub async fn bitmaps(path: &Path) -> Result<Vec<String>> {
    let p = path.to_string_lossy();
    let out = run(
        "qemu-img",
        &[
            "info",
            "--output=json",
            "-f",
            "qcow2",
            "--backing-chain",
            &p,
        ],
    )
    .await?;
    let infos: Vec<serde_json::Value> =
        serde_json::from_slice(&out).map_err(|e| Error::InvalidImage(e.to_string()))?;
    // The first is the file itself; its backing files' bitmaps are theirs.
    Ok(infos
        .first()
        .and_then(|i| i["format-specific"]["data"]["bitmaps"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|b| b["name"].as_str().map(str::to_owned))
        .collect())
}

/// Removes a dirty bitmap from a qcow2 file no QEMU has open.
pub async fn remove_bitmap(path: &Path, name: &str) -> Result<()> {
    let p = path.to_string_lossy();
    run("qemu-img", &["bitmap", "--remove", "-f", "qcow2", &p, name]).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn write(path: &Path, offset: u64, byte: u8, len: u64) {
        let p = path.to_string_lossy();
        let cmd = format!("write -P {byte} {offset} {len}");
        run("qemu-io", &["-f", "qcow2", "-c", &cmd, &p])
            .await
            .unwrap();
    }

    async fn same(a: &Path, b: &Path) -> bool {
        let (a, b) = (a.to_string_lossy(), b.to_string_lossy());
        run("qemu-img", &["compare", "-q", &a, &b]).await.is_ok()
    }

    /// Changes found by comparing, stored on a parent's copy, read back as
    /// the volume; merging a parent into its child keeps the child whole.
    #[tokio::test]
    async fn changes_stored_and_merged() {
        if run("qemu-img", &["--version"]).await.is_err() {
            eprintln!("skipping: no qemu-img");
            return;
        }
        let dir = std::env::temp_dir().join(format!("mst-backups-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let root = dir.as_path();
        let vol = root.join("vol.qcow2");
        for b in ["a", "b", "c"] {
            tokio::fs::create_dir(root.join(b)).await.unwrap();
        }
        create_target(&vol, 8 << 20, None).await.unwrap();
        write(&vol, 0, 0xaa, 1 << 20).await;
        // A full copy, as the first backup.
        crate::convert(&vol, "qcow2", &root.join("a/0.qcow2"), "qcow2")
            .await
            .unwrap();
        write(&vol, 1 << 20, 0xbb, 64 << 10).await;
        store_changes(&vol, "../a/0.qcow2", &root.join("b/0.qcow2"), 8 << 20)
            .await
            .unwrap();
        assert_eq!(
            backing_of(&root.join("b/0.qcow2"))
                .await
                .unwrap()
                .as_deref(),
            Some("../a/0.qcow2")
        );
        assert!(same(&root.join("b/0.qcow2"), &vol).await);
        // Little of it is stored: the 64 KiB that changed, and metadata.
        let len = tokio::fs::metadata(root.join("b/0.qcow2"))
            .await
            .unwrap()
            .len();
        assert!(len < 1 << 20, "{len}");
        // Grown, then changed again.
        let v = vol.to_string_lossy();
        run("qemu-img", &["resize", "-q", &v, "12M"]).await.unwrap();
        write(&vol, 10 << 20, 0xcc, 64 << 10).await;
        store_changes(&vol, "../b/0.qcow2", &root.join("c/0.qcow2"), 12 << 20)
            .await
            .unwrap();
        assert!(same(&root.join("c/0.qcow2"), &vol).await);
        // The first backup goes: merged into the second, which stands alone
        // and still reads as it did; the third, on it, as well.
        merge_into_child(&root.join("b/0.qcow2"), &root.join("a/0.qcow2"))
            .await
            .unwrap();
        assert_eq!(backing_of(&root.join("b/0.qcow2")).await.unwrap(), None);
        assert!(!root.join("a/0.qcow2").exists());
        assert!(same(&root.join("c/0.qcow2"), &vol).await);
        // So does removing the middle one.
        merge_into_child(&root.join("c/0.qcow2"), &root.join("b/0.qcow2"))
            .await
            .unwrap();
        assert!(same(&root.join("c/0.qcow2"), &vol).await);
        assert!(bitmaps(&vol).await.unwrap().is_empty());
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }
}
