//! Copying volumes between hosts: this host serves one, read-only, for
//! another to copy (`ExportVolume`, with `qemu-nbd` over TLS), or copies one
//! in (`CopyVolume`), from a pool it reaches or from another host.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use mst_proto::{CopyVolume, ExportVolume, MigrationTls};
use tokio::process::Command;

use crate::session::Agent;

/// Credentials of copies under way, a directory each.
const RUN_DIR: &str = "/run/metastratum/transfer";
/// The ports hosts open to each other for migrations, which copies use too.
const PORTS: std::ops::RangeInclusive<u16> = 49152..=49215;
/// How long a volume is served at most, copied or not.
const SERVED_FOR: Duration = Duration::from_secs(3600);

/// Volumes being served: the `qemu-nbd` serving each.
static EXPORTS: LazyLock<Mutex<HashMap<String, u32>>> = LazyLock::new(Mutex::default);

/// Writes a copy's credentials where QEMU's `tls-creds-x509` takes them:
/// `ca-cert.pem`, and `<role>-cert.pem` and `<role>-key.pem`.
async fn credentials(volume: &str, role: &str, tls: &MigrationTls) -> Result<PathBuf> {
    let dir = Path::new(RUN_DIR).join(format!("{volume}-{role}"));
    mst_storage::private_dir(Path::new(RUN_DIR)).await?;
    let _ = tokio::fs::remove_dir_all(&dir).await;
    tokio::fs::create_dir(&dir).await?;
    for (name, pem) in [
        ("ca-cert.pem".to_owned(), &tls.ca_pem),
        (format!("{role}-cert.pem"), &tls.cert_pem),
        (format!("{role}-key.pem"), &tls.key_pem),
    ] {
        tokio::fs::write(dir.join(name), pem).await?;
    }
    mst_storage::private_dir(&dir).await?;
    Ok(dir)
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
    end(&c.volume_id);
    let tls = c.tls.as_ref().context("no credentials for the copy")?;
    let dir = credentials(&c.volume_id, "server", tls).await?;
    let object = format!(
        "tls-creds-x509,id=tls0,dir={},endpoint=server,verify-peer=on",
        dir.to_string_lossy()
    );
    let pid_file = dir.join("pid");
    for port in PORTS {
        // Forks once it listens; serves one client, then exits.
        let out = Command::new("qemu-nbd")
            .arg("--object")
            .arg(&object)
            .args(["--tls-creds", "tls0", "--read-only", "--format", "qcow2"])
            .args(["--shared", "1", "--export-name", &c.volume_id])
            .args(["--bind", &c.address, "--port", &port.to_string()])
            .arg("--fork")
            .arg("--pid-file")
            .arg(&pid_file)
            .arg(&file)
            .output()
            .await
            .context("running qemu-nbd")?;
        if out.status.success() {
            let pid: u32 = tokio::fs::read_to_string(&pid_file)
                .await?
                .trim()
                .parse()
                .context("qemu-nbd's pid")?;
            EXPORTS.lock().unwrap().insert(c.volume_id.clone(), pid);
            let volume = c.volume_id.clone();
            tokio::spawn(async move {
                tokio::time::sleep(SERVED_FOR).await;
                if EXPORTS.lock().unwrap().get(&volume) == Some(&pid) {
                    end(&volume);
                }
            });
            tracing::info!(volume = %c.volume_id, port, "serving a volume for another host to copy");
            return Ok(serde_json::json!({ "port": port }).to_string());
        }
        let err = String::from_utf8_lossy(&out.stderr);
        if !err.contains("in use") {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            bail!("qemu-nbd: {}", err.trim());
        }
    }
    let _ = tokio::fs::remove_dir_all(&dir).await;
    bail!("no free port in 49152-49215 to serve the volume on")
}

/// Stops serving the volume, if it still is.
pub fn end(volume: &str) {
    if let Some(pid) = EXPORTS.lock().unwrap().remove(volume) {
        // Only if it still is that qemu-nbd (it exits after its client).
        let ours = std::fs::read(format!("/proc/{pid}/cmdline"))
            .is_ok_and(|c| c.split(|b| *b == 0).any(|a| a.ends_with(b"qemu-nbd")));
        if ours {
            let _ = std::process::Command::new("kill")
                .arg(pid.to_string())
                .status();
        }
    }
    let _ = std::fs::remove_dir_all(Path::new(RUN_DIR).join(format!("{volume}-server")));
}

/// Copies a volume in, from a pool here or from another host.
pub async fn copy(agent: &Agent, c: &CopyVolume) -> Result<()> {
    let pool = agent.pool(c.pool.as_ref()).await?;
    let scratch = agent.pools.local().root().join("transfer");
    mst_storage::private_dir(&scratch).await?;
    if let Some(source) = &c.source_pool {
        let source = agent.pool(Some(source)).await?;
        match source.volume_file(&c.volume_id)? {
            // Read with its image, so the copy depends on nothing.
            Some(file) => {
                pool.import_volume(&c.volume_id, c.size_bytes, &file)
                    .await?
            }
            None => {
                let file = scratch.join(format!("{}.qcow2", c.volume_id));
                let result = async {
                    source.export_volume(&c.volume_id, &file).await?;
                    pool.import_volume(&c.volume_id, c.size_bytes, &file).await
                }
                .await;
                let _ = tokio::fs::remove_file(&file).await;
                result?;
            }
        }
    } else {
        let tls = c.tls.as_ref().context("no credentials for the copy")?;
        let dir = credentials(&c.volume_id, "client", tls).await?;
        let port = u16::try_from(c.nbd_port).context("no port to copy from")?;
        let source = mst_storage::NbdSource {
            host: c.nbd_address.clone(),
            port,
            export: c.volume_id.clone(),
            tls_dir: dir.clone(),
        };
        let result = pool
            .import_nbd(&c.volume_id, c.size_bytes, &source, &scratch)
            .await;
        let _ = tokio::fs::remove_dir_all(&dir).await;
        result?;
    }
    tracing::info!(volume = %c.volume_id, "volume copied in");
    Ok(())
}
