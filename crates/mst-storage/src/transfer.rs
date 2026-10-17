//! Copying volumes between hosts: one host serves a volume (read-only)
//! with `qemu-nbd` over TLS, and the other copies it into a pool of its
//! own with `qemu-img`. Each copy has credentials of its own (a CA made for
//! it alone), so only that host's `qemu-img` can read the volume.

use std::path::{Path, PathBuf};

use crate::{Result, random_suffix, run};

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

/// Copies `src` to `dest` (qcow2 or raw), all of the volume's data in it:
/// what it was made from (its image) too.
pub(crate) async fn copy_from_nbd(src: &NbdSource, dest: &Path, dest_format: &str) -> Result<()> {
    let object = format!(
        "tls-creds-x509,id=tls0,dir={},endpoint=client",
        src.tls_dir.to_string_lossy()
    );
    let opts = format!(
        "driver=nbd,server.type=inet,server.host={host},server.port={port},export={export},\
         tls-creds=tls0,tls-hostname={host}",
        host = src.host,
        port = src.port,
        export = src.export,
    );
    let d = dest.to_string_lossy();
    run(
        "qemu-img",
        &[
            "convert",
            "-q",
            "--object",
            &object,
            "--image-opts",
            &opts,
            "-O",
            dest_format,
            &d,
        ],
    )
    .await?;
    Ok(())
}

/// A temporary file beside `path`.
pub(crate) fn temporary(path: &Path) -> PathBuf {
    path.with_extension(format!("{}.part", random_suffix()))
}
