//! Copying volumes between hosts: this host serves one, read-only, for
//! another to copy (`ExportVolume`, with `qemu-nbd` over TLS), or copies one
//! in (`CopyVolume`), from a pool it reaches or from another host. Images in
//! the image store go from host to host alike (`ExportImage`, `CopyImage`).

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use mst_proto::{CopyImage, CopyVolume, ExportImage, ExportVolume, MigrationTls};
use tokio::process::Command;

use crate::session::Agent;

/// Credentials of copies under way, a directory each.
const RUN_DIR: &str = "/run/metastratum/transfer";
/// The ports hosts open to each other for migrations, which copies use too.
const PORTS: std::ops::RangeInclusive<u16> = 49152..=49215;
/// How long a volume is served at most: longer than the controller waits
/// for any copy (it ends the serving itself once the copy is done).
const SERVED_FOR: Duration = Duration::from_secs(13 * 3600);

/// Volumes being served: the `qemu-nbd` serving each, and its pid file.
static EXPORTS: LazyLock<Mutex<HashMap<String, (u32, PathBuf)>>> = LazyLock::new(Mutex::default);

/// One copy at a time per volume: a copy asked for again waits for the
/// one under way (which then cannot take its place after it).
static COPIES: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(Mutex::default);

fn copy_lock(volume: &str) -> Arc<tokio::sync::Mutex<()>> {
    COPIES
        .lock()
        .unwrap()
        .entry(volume.to_owned())
        .or_default()
        .clone()
}

/// Writes a copy's credentials where QEMU's `tls-creds-x509` takes them:
/// `ca-cert.pem`, and `<role>-cert.pem` and `<role>-key.pem`, readable by
/// root only from the start.
fn credentials(volume: &str, role: &str, tls: &MigrationTls) -> Result<PathBuf> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(RUN_DIR)?;
    let dir = Path::new(RUN_DIR).join(format!("{volume}-{role}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    for (name, pem) in [
        ("ca-cert.pem".to_owned(), &tls.ca_pem),
        (format!("{role}-cert.pem"), &tls.cert_pem),
        (format!("{role}-key.pem"), &tls.key_pem),
    ] {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join(name))?
            .write_all(pem.as_bytes())?;
    }
    Ok(dir)
}

/// The `qemu-nbd` processes serving from our directory, by pid: the one
/// with `pid_file`, or (none given) all of them.
fn servers(pid_file: Option<&Path>) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return vec![];
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
                return false;
            };
            let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
            args.first().is_some_and(|a| a.ends_with(b"qemu-nbd"))
                && args.iter().any(|a| match pid_file {
                    Some(p) => *a == p.as_os_str().as_encoded_bytes(),
                    None => a.starts_with(RUN_DIR.as_bytes()),
                })
        })
        .collect()
}

fn kill(pid: u32) {
    let _ = std::process::Command::new("kill")
        .arg(pid.to_string())
        .status();
}

/// As the agent starts: whatever it served before it restarted stops, and
/// those copies' credentials go.
pub fn clean_up() {
    for pid in servers(None) {
        kill(pid);
    }
    let _ = std::fs::remove_dir_all(RUN_DIR);
}

/// Serves the volume for the host this copy's client certificate belongs
/// to. Returns `{"port": N}` once `qemu-nbd` listens.
pub async fn export(agent: &Agent, c: &ExportVolume) -> Result<String> {
    let pool = agent.pool(c.pool.as_ref()).await?;
    let file = pool
        .volume_file(&c.volume_id)?
        .context("disks on Ceph are copied by a host that reaches their pool")?;
    if !tokio::fs::try_exists(&file).await? {
        bail!("no volume {} here", c.volume_id);
    }
    serve(&c.volume_id, &file, &c.address, c.tls.as_ref()).await
}

/// Serves an image in the store for the host this copy's client
/// certificate belongs to, as [`export`] serves a volume.
pub async fn export_image(agent: &Agent, c: &ExportImage) -> Result<String> {
    let file = agent.pools.store().path(&c.tenant_path, &c.image_id)?;
    if !tokio::fs::try_exists(&file).await? {
        bail!("image {} is not in this host's image store", c.image_id);
    }
    serve(
        &export_name(&c.export_id)?,
        &file,
        &c.address,
        c.tls.as_ref(),
    )
    .await
}

/// Names of servings come from the controller: they name directories too.
fn export_name(name: &str) -> Result<String> {
    if name.is_empty()
        || name.len() > 128
        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        bail!("bad export name {name:?}");
    }
    Ok(name.to_owned())
}

/// Serves `file` (qcow2) as export `name`, read-only, to the one client
/// `tls` admits.
async fn serve(
    name: &str,
    file: &Path,
    address: &str,
    tls: Option<&MigrationTls>,
) -> Result<String> {
    end(name);
    let tls = tls.context("no credentials for the copy")?;
    let dir = credentials(name, "server", tls)?;
    let object = format!(
        "tls-creds-x509,id=tls0,dir={},endpoint=server,verify-peer=on",
        dir.to_string_lossy()
    );
    let pid_file = dir.join("pid");
    for port in PORTS {
        // Forks once it listens; serves one client, then exits. A client
        // gets 10 seconds to finish its TLS handshake (QEMU's default).
        let out = Command::new("qemu-nbd")
            .arg("--object")
            .arg(&object)
            .args(["--tls-creds", "tls0", "--read-only", "--format", "qcow2"])
            .args(["--shared", "1", "--export-name", name])
            .args(["--bind", address, "--port", &port.to_string()])
            .arg("--fork")
            .arg("--pid-file")
            .arg(&pid_file)
            .arg(file)
            .output()
            .await
            .context("running qemu-nbd")?;
        if out.status.success() {
            let pid: u32 = tokio::fs::read_to_string(&pid_file)
                .await?
                .trim()
                .parse()
                .context("qemu-nbd's pid")?;
            EXPORTS
                .lock()
                .unwrap()
                .insert(name.to_owned(), (pid, pid_file.clone()));
            let served = name.to_owned();
            tokio::spawn(async move {
                tokio::time::sleep(SERVED_FOR).await;
                let current = EXPORTS.lock().unwrap().get(&served).map(|e| e.0);
                if current == Some(pid) {
                    end(&served);
                }
            });
            tracing::info!(export = name, port, "serving for another host to copy");
            return Ok(serde_json::json!({ "port": port }).to_string());
        }
        let err = String::from_utf8_lossy(&out.stderr);
        if !err.contains("in use") {
            let _ = std::fs::remove_dir_all(&dir);
            bail!("qemu-nbd: {}", err.trim());
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    bail!("no free port in 49152-49215 to serve on")
}

/// Stops serving the volume (or image serving), if it still is.
pub fn end(name: &str) {
    let served = EXPORTS.lock().unwrap().remove(name);
    if let Some((_, pid_file)) = served {
        // Only that qemu-nbd (it exits after its client, and its pid may be
        // another process's by now).
        for pid in servers(Some(&pid_file)) {
            kill(pid);
        }
    }
    let _ = std::fs::remove_dir_all(Path::new(RUN_DIR).join(format!("{name}-server")));
}

/// Copies a volume in, from a pool here or from another host, replacing
/// whatever is under its name in the pool (see `mst_storage::copy_into`).
pub async fn copy(agent: &Agent, c: &CopyVolume) -> Result<()> {
    let lock = copy_lock(&c.volume_id);
    let _held = lock.lock().await;
    let pool = agent.pool(c.pool.as_ref()).await?;
    if let Some(source) = &c.source_pool {
        let source = agent.pool(Some(source)).await?;
        match (&source, source.volume_file(&c.volume_id)?) {
            (_, Some(file)) => {
                let src = mst_storage::CopySource::File(&file);
                mst_storage::copy_into(&pool, &c.volume_id, c.size_bytes, &src).await?;
            }
            (mst_storage::Pool::Rbd(rbd), None) => {
                let src = mst_storage::CopySource::Rbd(rbd, &c.volume_id);
                mst_storage::copy_into(&pool, &c.volume_id, c.size_bytes, &src).await?;
            }
            (mst_storage::Pool::File(_), None) => bail!("no volume file to copy"),
        }
    } else {
        let tls = c.tls.as_ref().context("no credentials for the copy")?;
        let dir = credentials(&c.volume_id, "client", tls)?;
        let port = u16::try_from(c.nbd_port).context("no port to copy from")?;
        let source = mst_storage::NbdSource {
            host: c.nbd_address.clone(),
            port,
            export: c.volume_id.clone(),
            tls_dir: dir.clone(),
        };
        let src = mst_storage::CopySource::Nbd(&source);
        let result = mst_storage::copy_into(&pool, &c.volume_id, c.size_bytes, &src).await;
        let _ = std::fs::remove_dir_all(&dir);
        result?;
    }
    tracing::info!(volume = %c.volume_id, "volume copied in");
    Ok(())
}

/// Copies an image into the store from the host serving it (see
/// [`export_image`]). Returns what [`mst_storage::ImageStore::copy_in`] does,
/// as JSON.
pub async fn copy_image(agent: &Agent, c: &CopyImage) -> Result<String> {
    let name = export_name(&c.export_id)?;
    let tls = c.tls.as_ref().context("no credentials for the copy")?;
    let port = u16::try_from(c.nbd_port).context("no port to copy from")?;
    let dir = credentials(&name, "client", tls)?;
    let source = mst_storage::NbdSource {
        host: c.nbd_address.clone(),
        port,
        export: name,
        tls_dir: dir.clone(),
    };
    let result = agent
        .pools
        .store()
        .copy_in(&c.tenant_path, &c.image_id, &source)
        .await;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(serde_json::to_string(&result?)?)
}
