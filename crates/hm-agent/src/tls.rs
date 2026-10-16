//! TLS between hosts for live migration.
//!
//! The source host's libvirt connects to the destination's
//! (`qemu+tls://dst/system`), and QEMU streams memory and disks over TLS. Both
//! use the host's identity from enrollment: its certificate is signed by the
//! controller's CA for client and server use, with the host's name and
//! address. It is installed where libvirt and QEMU look for it.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::enroll::HostIdentity;

/// libvirt's TLS port.
const LIBVIRT_TLS_PORT: u16 = 16514;

/// Who may read a file: (mode, group). QEMU runs unprivileged and must read
/// its key; libvirt runs as root.
#[derive(Clone, Copy)]
enum Access {
    Public,
    RootOnly,
    Qemu(u32),
}

fn write_if_changed(path: &Path, data: &str, access: Access) -> Result<bool> {
    use std::os::unix::fs::{OpenOptionsExt, chown};
    let mode = match access {
        Access::Public => 0o644,
        Access::RootOnly => 0o600,
        Access::Qemu(_) => 0o640,
    };
    let gid = match access {
        Access::Qemu(g) => g,
        _ => 0,
    };
    if let (Ok(cur), Ok(meta)) = (std::fs::read_to_string(path), std::fs::metadata(path))
        && cur == data
        && meta.permissions().mode() & 0o777 == mode
        && meta.gid() == gid
    {
        return Ok(false);
    }
    std::fs::create_dir_all(path.parent().expect("absolute path"))?;
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)?;
        f.write_all(data.as_bytes())?;
        f.sync_all()?;
    }
    chown(&tmp, Some(0), Some(gid))?;
    std::fs::rename(&tmp, path).with_context(|| format!("installing {}", path.display()))?;
    Ok(true)
}

/// The group QEMU runs in on this distribution.
fn qemu_group() -> u32 {
    let groups = std::fs::read_to_string("/etc/group").unwrap_or_default();
    for want in ["libvirt-qemu", "qemu", "kvm"] {
        if let Some(gid) = groups
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split(':').collect();
                (f.len() > 2 && f[0] == want)
                    .then(|| f[2].parse().ok())
                    .flatten()
            })
            .next()
        {
            return gid;
        }
    }
    0
}

/// Installs the host identity for libvirt and QEMU TLS. Returns whether any
/// file changed (a running daemon then needs restarting to notice).
pub fn install(id: &HostIdentity) -> Result<bool> {
    let qemu = Access::Qemu(qemu_group());
    let files: [(&str, &str, Access); 10] = [
        ("/etc/pki/CA/cacert.pem", &id.ca_pem, Access::Public),
        (
            "/etc/pki/libvirt/servercert.pem",
            &id.cert_pem,
            Access::Public,
        ),
        (
            "/etc/pki/libvirt/clientcert.pem",
            &id.cert_pem,
            Access::Public,
        ),
        (
            "/etc/pki/libvirt/private/serverkey.pem",
            &id.key_pem,
            Access::RootOnly,
        ),
        (
            "/etc/pki/libvirt/private/clientkey.pem",
            &id.key_pem,
            Access::RootOnly,
        ),
        ("/etc/pki/qemu/ca-cert.pem", &id.ca_pem, Access::Public),
        (
            "/etc/pki/qemu/server-cert.pem",
            &id.cert_pem,
            Access::Public,
        ),
        (
            "/etc/pki/qemu/client-cert.pem",
            &id.cert_pem,
            Access::Public,
        ),
        ("/etc/pki/qemu/server-key.pem", &id.key_pem, qemu),
        ("/etc/pki/qemu/client-key.pem", &id.key_pem, qemu),
    ];
    let mut changed = false;
    for (path, data, access) in files {
        changed |= write_if_changed(Path::new(path), data, access)?;
    }
    Ok(changed)
}

/// Makes libvirt pick up renewed certificates for new TLS connections,
/// without restarting it.
pub async fn reload_libvirt() {
    for server in ["libvirtd", "virtproxyd"] {
        let out = tokio::process::Command::new("virt-admin")
            .args(["server-update-tls", server])
            .output()
            .await;
        if matches!(&out, Ok(o) if o.status.success()) {
            return;
        }
    }
    tracing::warn!(
        "could not reload libvirt's TLS certificates; they apply after its next restart"
    );
}

/// Whether libvirt listens for TLS connections (the installer enables the
/// listener; see deploy/).
pub async fn listening() -> bool {
    matches!(
        tokio::time::timeout(
            Duration::from_secs(2),
            tokio::net::TcpStream::connect(("127.0.0.1", LIBVIRT_TLS_PORT)),
        )
        .await,
        Ok(Ok(_))
    )
}
