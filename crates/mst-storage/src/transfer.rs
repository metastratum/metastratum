//! Copying volumes for moves: between pools a host reaches, or from another
//! host that serves the volume (read-only) with `qemu-nbd` over TLS. Each
//! copy from another host has credentials of its own (a CA made for it
//! alone), so only that host's `qemu-img` can read the volume.
//!
//! A copy always goes to a name of its own and then takes the volume's
//! place, replacing what was there: never is something already there taken
//! for the copy (it may be stale, or partial).

use std::path::{Path, PathBuf};

use crate::rbd::RbdPool;
use crate::{Error, FilePool, Pool, Result, random_suffix, run, set_mode};

/// A volume another host serves for this one to copy.
#[derive(Clone, Debug)]
pub struct NbdSource {
    /// The serving host's address; its certificate names it.
    pub host: String,
    pub port: u16,
    pub export: String,
    /// `ca-cert.pem`, `client-cert.pem` and `client-key.pem`, as QEMU's
    /// `tls-creds-x509` takes them.
    pub tls_dir: PathBuf,
}

/// What a copy reads.
pub enum CopySource<'a> {
    /// A volume in a pool of files here (qcow2, on its image if any).
    File(&'a Path),
    /// A volume in a Ceph pool this host reaches: its ID.
    Rbd(&'a RbdPool, &'a str),
    /// A volume another host serves.
    Nbd(&'a NbdSource),
}

/// A file holding a Ceph key for `qemu-img`'s `secret` object, removed when
/// dropped.
struct KeyFile(PathBuf);

impl KeyFile {
    async fn new(pool: &RbdPool) -> Result<Self> {
        tokio::fs::create_dir_all(pool.work_dir()).await?;
        let path = pool.work_dir().join(format!("{}.key", random_suffix()));
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        use tokio::io::AsyncWriteExt;
        let mut f = options.open(&path).await?;
        f.write_all(pool.key.as_bytes()).await?;
        f.flush().await?;
        Ok(Self(path))
    }
}

impl Drop for KeyFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// `qemu-img` arguments that name an image: `--object`s it needs, and the
/// image itself.
struct ImageArgs {
    objects: Vec<String>,
    /// `--image-opts <opts>`, or `-f <format> <path>`.
    image: Vec<String>,
    _key: Option<KeyFile>,
}

fn rbd_opts(pool: &RbdPool, image: &str, secret: &str) -> String {
    let mut opts = format!(
        "driver=rbd,pool={},image={image},user={},key-secret={secret}",
        pool.pool, pool.user
    );
    for (i, (host, port)) in pool.monitor_addrs().iter().enumerate() {
        opts.push_str(&format!(",server.{i}.host={host},server.{i}.port={port}"));
    }
    opts
}

async fn source_args(src: &CopySource<'_>) -> Result<ImageArgs> {
    Ok(match src {
        CopySource::File(path) => ImageArgs {
            objects: vec![],
            image: vec!["-f".into(), "qcow2".into(), path.to_string_lossy().into()],
            _key: None,
        },
        CopySource::Rbd(pool, id) => {
            let key = KeyFile::new(pool).await?;
            ImageArgs {
                objects: vec![format!(
                    "secret,id=srckey,file={},format=base64",
                    key.0.to_string_lossy()
                )],
                image: vec![
                    "--image-opts".into(),
                    rbd_opts(pool, &RbdPool::volume_name(id)?, "srckey"),
                ],
                _key: Some(key),
            }
        }
        CopySource::Nbd(nbd) => ImageArgs {
            objects: vec![format!(
                "tls-creds-x509,id=tls0,dir={},endpoint=client",
                nbd.tls_dir.to_string_lossy()
            )],
            image: vec![
                "--image-opts".into(),
                format!(
                    "driver=nbd,server.type=inet,server.host={host},server.port={port},\
                     export={export},tls-creds=tls0,tls-hostname={host}",
                    host = nbd.host,
                    port = nbd.port,
                    export = nbd.export,
                ),
            ],
            _key: None,
        },
    })
}

fn objects(args: &[&ImageArgs]) -> Vec<String> {
    args.iter()
        .flat_map(|a| a.objects.iter())
        .flat_map(|o| ["--object".to_owned(), o.clone()])
        .collect()
}

async fn qemu_img(args: Vec<String>) -> Result<Vec<u8>> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run("qemu-img", &args).await
}

/// The source's virtual size.
async fn virtual_size(src: &ImageArgs) -> Result<u64> {
    let mut args = vec!["info".to_owned(), "--output=json".to_owned()];
    args.extend(objects(&[src]));
    args.extend(src.image.iter().cloned());
    let out = qemu_img(args).await?;
    let info: crate::ImgInfo =
        serde_json::from_slice(&out).map_err(|e| Error::InvalidImage(e.to_string()))?;
    Ok(info.virtual_size)
}

/// Makes volume `id` in `dest` a copy of `src` (with what its image held:
/// the copy depends on nothing), of at least `size` bytes, replacing what
/// is there under that name.
pub async fn copy_into(dest: &Pool, id: &str, size: u64, src: &CopySource<'_>) -> Result<()> {
    let source = source_args(src).await?;
    match dest {
        Pool::File(pool) => copy_to_file(pool, id, size, src, &source).await,
        Pool::Rbd(pool) => copy_to_rbd(pool, id, size, &source).await,
    }
}

async fn copy_to_file(
    pool: &FilePool,
    id: &str,
    size: u64,
    src: &CopySource<'_>,
    source: &ImageArgs,
) -> Result<()> {
    let dest = pool.volume_path(id)?;
    // Two pools on the same storage would make the copy its own source,
    // which is then removed as the old copy.
    if let CopySource::File(path) = src
        && let (Ok(a), Ok(b)) = (
            tokio::fs::canonicalize(path).await,
            tokio::fs::canonicalize(&dest).await,
        )
        && a == b
    {
        return Err(Error::Pool(
            "the copy would be its own source: the pools are the same storage".into(),
        ));
    }
    let tmp = dest.with_extension(format!("{}.part", random_suffix()));
    let result = async {
        let mut args = vec!["convert".to_owned(), "-q".to_owned()];
        args.extend(objects(&[source]));
        args.extend(source.image.iter().cloned());
        args.extend(["-O".into(), "qcow2".into(), tmp.to_string_lossy().into()]);
        qemu_img(args).await?;
        if size > crate::img_info(&tmp, Some("qcow2")).await?.virtual_size {
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

async fn copy_to_rbd(pool: &RbdPool, id: &str, size: u64, source: &ImageArgs) -> Result<()> {
    let vol = RbdPool::volume_name(id)?;
    let tmp = format!("{vol}-part-{}", random_suffix());
    let bytes = virtual_size(source).await?.max(size);
    let mib = bytes.div_ceil(1 << 20).to_string();
    pool.rbd(&["create", "--no-progress", "--size", &mib, &pool.spec(&tmp)])
        .await?;
    let result = async {
        let key = KeyFile::new(pool).await?;
        let target = ImageArgs {
            objects: vec![format!(
                "secret,id=dstkey,file={},format=base64",
                key.0.to_string_lossy()
            )],
            image: vec![],
            _key: None,
        };
        // Into the new (zeroed) image: what reads as zero is not written.
        let mut args = vec![
            "convert".to_owned(),
            "-q".into(),
            "-n".into(),
            "--target-is-zero".into(),
        ];
        args.extend(objects(&[source, &target]));
        args.extend(source.image.iter().cloned());
        args.extend(["--target-image-opts".into(), rbd_opts(pool, &tmp, "dstkey")]);
        qemu_img(args).await?;
        drop(key);
        if pool.exists(&pool.spec(&vol)).await? {
            pool.rbd(&["rm", "--no-progress", &pool.spec(&vol)]).await?;
        }
        pool.rbd(&["rename", &pool.spec(&tmp), &pool.spec(&vol)])
            .await?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = pool.rbd(&["rm", "--no-progress", &pool.spec(&tmp)]).await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0199f0a2-7b3c-7d00-8000-000000000001";

    /// A qcow2 volume in `pool` holding `text`.
    async fn volume(pool: &FilePool, text: &str) {
        let raw = pool.root().join(format!("{text}.raw"));
        tokio::fs::write(&raw, {
            let mut b = text.as_bytes().to_vec();
            b.resize(1 << 20, 0);
            b
        })
        .await
        .unwrap();
        let dest = pool.volume_path(ID).unwrap();
        run(
            "qemu-img",
            &[
                "convert",
                "-q",
                "-f",
                "raw",
                "-O",
                "qcow2",
                &raw.to_string_lossy(),
                &dest.to_string_lossy(),
            ],
        )
        .await
        .unwrap();
    }

    async fn content(pool: &FilePool) -> String {
        let out = pool.root().join("out.raw");
        let src = pool.volume_path(ID).unwrap();
        run(
            "qemu-img",
            &[
                "convert",
                "-q",
                "-f",
                "qcow2",
                "-O",
                "raw",
                &src.to_string_lossy(),
                &out.to_string_lossy(),
            ],
        )
        .await
        .unwrap();
        String::from_utf8_lossy(&tokio::fs::read(&out).await.unwrap()[..5]).into_owned()
    }

    #[tokio::test]
    async fn copies_replace_what_is_there_and_never_copy_onto_themselves() {
        if tokio::process::Command::new("qemu-img")
            .arg("--version")
            .output()
            .await
            .is_err()
        {
            return eprintln!("qemu-img not installed; skipping");
        }
        let root = std::env::temp_dir().join(format!("mst-copy-{}", std::process::id()));
        let (a, b) = (FilePool::new(root.join("a")), FilePool::new(root.join("b")));
        a.init().await.unwrap();
        b.init().await.unwrap();
        volume(&a, "fresh").await;
        // A stale copy (a move that failed after all) is no copy of now.
        volume(&b, "stale").await;
        let src = a.volume_path(ID).unwrap();
        copy_into(&Pool::File(b.clone()), ID, 2 << 20, &CopySource::File(&src))
            .await
            .unwrap();
        assert_eq!(content(&b).await, "fresh");
        // A second pool on the same storage: the copy would be its source.
        let alias = FilePool::new(root.join("a"));
        let err = copy_into(&Pool::File(alias), ID, 2 << 20, &CopySource::File(&src))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("same storage"), "{err}");
        assert_eq!(content(&a).await, "fresh", "the source is untouched");
        tokio::fs::remove_dir_all(&root).await.unwrap();
    }
}
