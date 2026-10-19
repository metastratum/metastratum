//! TLS between hosts for live migration: QEMU streams memory and disks from
//! one host to the other over TLS. Each migration has credentials of its own
//! (see [`install_migration_tls`]), in QEMU's TLS directory only while QEMU
//! loads them; between migrations it holds nothing of metastratum's. The
//! host's own identity (its key the controller knows it by) is never there:
//! a guest breaking out into QEMU must not get to act as the host.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result};

/// Who may read a file. QEMU runs unprivileged and must read its key.
#[derive(Clone, Copy)]
pub(crate) enum Access {
    Public,
    /// Root, and this group.
    Group(u32),
}

pub(crate) fn write_if_changed(path: &Path, data: &str, access: Access) -> Result<bool> {
    use std::os::unix::fs::{OpenOptionsExt, chown};
    let mode = match access {
        Access::Public => 0o644,
        Access::Group(_) => 0o640,
    };
    let gid = match access {
        Access::Group(g) => g,
        _ => 0,
    };
    if let (Ok(cur), Ok(meta)) = (std::fs::read_to_string(path), std::fs::metadata(path))
        && cur == data
        && meta.permissions().mode() & 0o777 == mode
        && meta.gid() == gid
    {
        return Ok(false);
    }
    let dir = path.parent().expect("absolute path");
    // The agent's umask keeps what it makes private; these are read by QEMU
    // and OVN too, so every directory made on the way is open to them.
    let missing: Vec<&Path> = dir.ancestors().take_while(|d| !d.exists()).collect();
    for d in missing.into_iter().rev() {
        std::fs::create_dir(d)?;
        std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o755))?;
    }
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
        // `mode` above is narrowed by the umask: set it as meant.
        f.set_permissions(std::fs::Permissions::from_mode(mode))?;
        f.sync_all()?;
    }
    chown(&tmp, Some(0), Some(gid))?;
    std::fs::rename(&tmp, path).with_context(|| format!("installing {}", path.display()))?;
    Ok(true)
}

/// The group QEMU runs in on this distribution.
fn qemu_group() -> u32 {
    group_id(&["libvirt-qemu", "qemu", "kvm"])
}

/// The ID of the first of these groups that exists, else root's.
pub(crate) fn group_id(names: &[&str]) -> u32 {
    let groups = std::fs::read_to_string("/etc/group").unwrap_or_default();
    for want in names {
        if let Some(gid) = groups
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split(':').collect();
                (f.len() > 2 && f[0] == *want)
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

/// Every file the agent puts in place for QEMU's TLS.
const INSTALLED: [&str; 5] = [
    "/etc/pki/qemu/ca-cert.pem",
    "/etc/pki/qemu/server-cert.pem",
    "/etc/pki/qemu/client-cert.pem",
    "/etc/pki/qemu/server-key.pem",
    "/etc/pki/qemu/client-key.pem",
];

/// Removes a migration's credentials from QEMU's TLS directory, once QEMU
/// has loaded them (and as the agent starts, or the host is
/// decommissioned). Returns how many files it removed.
pub fn remove_installed() -> Result<usize> {
    let mut n = 0;
    for path in INSTALLED {
        match std::fs::remove_file(path) {
            Ok(()) => n += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {path}")),
        }
    }
    Ok(n)
}

/// QEMU's TLS files (`/etc/pki/qemu`).
const QEMU_DIR: &str = "/etc/pki/qemu";

/// Which side of a migration's TLS this host is.
#[derive(Clone, Copy, Debug)]
pub enum MigrationEnd {
    /// The destination, accepting the VM.
    Server,
    /// The source, sending it.
    Client,
}

/// Puts one migration's credentials where QEMU reads them: its CA as the
/// only one trusted, and this end's certificate. QEMU loads them when
/// libvirt sets up the migration's TLS (in Prepare on the destination, as
/// Perform starts on the source); [`remove_installed`] then removes them.
/// The caller holds the agent's QEMU TLS lock throughout.
pub fn install_migration_tls(tls: &mst_proto::MigrationTls, end: MigrationEnd) -> Result<()> {
    let key = Access::Group(qemu_group());
    let name = match end {
        MigrationEnd::Server => "server",
        MigrationEnd::Client => "client",
    };
    let file = |f: &str| format!("{QEMU_DIR}/{f}");
    write_if_changed(Path::new(&file("ca-cert.pem")), &tls.ca_pem, Access::Public)?;
    write_if_changed(
        Path::new(&file(&format!("{name}-cert.pem"))),
        &tls.cert_pem,
        Access::Public,
    )?;
    write_if_changed(
        Path::new(&file(&format!("{name}-key.pem"))),
        &tls.key_pem,
        key,
    )?;
    Ok(())
}

/// Whether this host can take part in live migrations the controller drives:
/// QEMU runs in a group the agent can give a migration's TLS key to (the
/// migration stream is encrypted).
pub fn ready_for_migration() -> bool {
    qemu_group() != 0
}
