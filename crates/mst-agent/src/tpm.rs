//! VMs' TPM state on this host, where libvirt keeps it for swtpm:
//! `/var/lib/libvirt/swtpm/<vm>/tpm2/tpm2-00.permall`, one file. It lives
//! on the host the VM runs on; the controller copies it to another host the
//! VM starts on (live migration carries it by itself), and keeps a sealed
//! copy for a VM restarted elsewhere after its host failed.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::Digest;

/// libvirt's directory of swtpm state (`swtpm_storage_dir`).
const STORAGE: &str = "/var/lib/libvirt/swtpm";
/// The state of a TPM 2.0, in its directory.
const STATE_FILE: &str = "tpm2-00.permall";

fn dir(storage: &Path, vm_id: &str) -> Result<PathBuf> {
    let id: uuid::Uuid = vm_id.parse().context("not a VM ID")?;
    Ok(storage.join(id.hyphenated().to_string()).join("tpm2"))
}

fn digest(state: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(state))
}

/// The VM's TPM state here: `{"digest", "state"}`, without the state when
/// its digest is `known`; `{}` when there is none here.
pub async fn export(vm_id: &str, known: &str) -> Result<String> {
    export_in(Path::new(STORAGE), vm_id, known).await
}

async fn export_in(storage: &Path, vm_id: &str, known: &str) -> Result<String> {
    let path = dir(storage, vm_id)?.join(STATE_FILE);
    let state = match tokio::fs::read(&path).await {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok("{}".into()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let digest = digest(&state);
    let mut out = serde_json::json!({ "digest": digest });
    if digest != known {
        use base64::Engine;
        out["state"] = base64::engine::general_purpose::STANDARD
            .encode(&state)
            .into();
    }
    Ok(out.to_string())
}

/// Puts the VM's TPM state in place here, in place of what was: libvirt
/// uses state it finds (making it swtpm's) rather than making new. Without
/// state or digest, removes what there is.
pub async fn import(vm_id: &str, state: &[u8], want: &str) -> Result<()> {
    import_in(Path::new(STORAGE), vm_id, state, want).await
}

async fn import_in(storage: &Path, vm_id: &str, state: &[u8], want: &str) -> Result<()> {
    let dir = dir(storage, vm_id)?;
    if state.is_empty() && want.is_empty() {
        match tokio::fs::remove_dir_all(&dir).await {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("removing {}", dir.display()));
            }
            _ => {}
        }
        tracing::info!(vm = vm_id, "TPM state removed");
        return Ok(());
    }
    if digest(state) != want {
        bail!("the TPM state does not match its digest");
    }
    let tmp = dir.join(format!("{STATE_FILE}.{}", std::process::id()));
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let parent = dir.parent().context("no parent")?.to_owned();
        let storage = parent.parent().context("no storage")?.to_owned();
        let d = dir.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            // As libvirt makes them (whatever the agent's umask): swtpm's
            // user may pass through the storage and the VM's directory, and
            // only it into the TPM's (libvirt hands it over as it starts).
            let mode =
                |p: &Path, m| std::fs::set_permissions(p, std::fs::Permissions::from_mode(m));
            for p in [&storage, &parent] {
                if !p.exists() {
                    std::fs::DirBuilder::new().mode(0o711).create(p)?;
                    mode(p, 0o711)?;
                }
            }
            mode(&parent, 0o711)?;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&d)?;
            mode(&d, 0o700)
        })
        .await??;
    }
    tokio::fs::write(&tmp, state).await?;
    tokio::fs::rename(&tmp, dir.join(STATE_FILE)).await?;
    tracing::info!(vm = vm_id, "TPM state put in place");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn state_goes_out_and_in_whole() {
        let storage = std::env::temp_dir().join(format!("mst-tpm-test-{}", std::process::id()));
        let vm = mst_core::ids::VmId::new().to_string();
        let json = |s: String| serde_json::from_str::<serde_json::Value>(&s).unwrap();
        assert_eq!(
            json(export_in(&storage, &vm, "").await.unwrap()),
            serde_json::json!({})
        );

        let state = b"tpm state".to_vec();
        assert!(import_in(&storage, &vm, &state, "wrong").await.is_err());
        import_in(&storage, &vm, &state, &digest(&state))
            .await
            .unwrap();
        // swtpm's user passes through to its own directory, whatever the
        // agent's umask.
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let d = dir(&storage, &vm).unwrap();
        assert_eq!(
            (mode(&storage), mode(d.parent().unwrap()), mode(&d)),
            (0o711, 0o711, 0o700)
        );
        let out = json(export_in(&storage, &vm, "").await.unwrap());
        assert_eq!(out["digest"], digest(&state));
        use base64::Engine;
        let back = base64::engine::general_purpose::STANDARD
            .decode(out["state"].as_str().unwrap())
            .unwrap();
        assert_eq!(back, state);
        // Known already: the digest alone.
        let out = json(export_in(&storage, &vm, &digest(&state)).await.unwrap());
        assert!(out.get("state").is_none(), "{out}");
        // Removed: none again.
        import_in(&storage, &vm, b"", "").await.unwrap();
        import_in(&storage, &vm, b"", "").await.unwrap();
        assert_eq!(
            json(export_in(&storage, &vm, "").await.unwrap()),
            serde_json::json!({})
        );
        assert!(dir(&storage, "not-a-vm").is_err());
        std::fs::remove_dir_all(&storage).unwrap();
    }
}
