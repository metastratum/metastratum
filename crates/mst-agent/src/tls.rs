//! TLS between hosts for live migration: QEMU streams memory and disks from
//! one host to the other over TLS. Each migration has credentials of its own
//! (see [`install_migration_tls`]); between migrations QEMU's TLS directory
//! holds the host's identity from enrollment, signed by the controller's CA
//! with the host's name and address.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result};

use crate::enroll::HostIdentity;

/// Who may read a file. QEMU runs unprivileged and must read its key.
#[derive(Clone, Copy)]
pub(crate) enum Access {
    Public,
    RootOnly,
    /// Root, and this group.
    Group(u32),
}

pub(crate) fn write_if_changed(path: &Path, data: &str, access: Access) -> Result<bool> {
    use std::os::unix::fs::{OpenOptionsExt, chown};
    let mode = match access {
        Access::Public => 0o644,
        Access::RootOnly => 0o600,
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
const INSTALLED: [&str; 6] = [
    "/etc/pki/qemu/ca-cert.pem",
    "/etc/pki/qemu/ca-crl.pem",
    "/etc/pki/qemu/server-cert.pem",
    "/etc/pki/qemu/client-cert.pem",
    "/etc/pki/qemu/server-key.pem",
    "/etc/pki/qemu/client-key.pem",
];

/// Removes the TLS files the agent installed for QEMU (decommissioning;
/// the OVN ones go with [`crate::ovn::restore`]). Returns how many it
/// removed.
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

/// Installs the host identity for QEMU's TLS. QEMU reads the files as a
/// migration starts, so a renewed identity applies from the next one.
pub fn install(id: &HostIdentity) -> Result<()> {
    let qemu = Access::Group(qemu_group());
    let files: [(&str, &str, Access); 5] = [
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
    for (path, data, access) in files {
        write_if_changed(Path::new(path), data, access)?;
    }
    Ok(())
}

/// QEMU's TLS files (`/etc/pki/qemu`): the host's identity, except while a
/// migration's own credentials are put in place for QEMU to load.
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
/// Perform starts on the source); [`restore_qemu_identity`] then puts the
/// host's own back. The caller holds the agent's QEMU TLS lock throughout.
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
    // The CA's revocation list is not this CA's.
    match std::fs::remove_file(file("ca-crl.pem")) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// Puts the host's own identity, and the CA's revocation list (kept in
/// `state_dir` meanwhile), back for QEMU.
pub fn restore_qemu_identity(id: &HostIdentity, state_dir: &Path) -> Result<()> {
    install(id)?;
    if let Ok(crl) = std::fs::read_to_string(state_dir.join(CRL)) {
        write_if_changed(
            Path::new(&format!("{QEMU_DIR}/ca-crl.pem")),
            &crl,
            Access::Public,
        )?;
    }
    Ok(())
}

/// The CA's revocation list, as kept in the agent's state directory.
const CRL: &str = "ca-crl.pem";

/// Installs the controller CA's revocation list where QEMU looks for it, so
/// migrations refuse revoked certificates, and keeps it in `state_dir` (a
/// migration's own credentials take QEMU's place meanwhile).
pub fn install_crl(pem: &str, state_dir: &Path) -> Result<()> {
    if !pem.trim_start().starts_with("-----BEGIN X509 CRL-----") {
        anyhow::bail!("not a revocation list");
    }
    write_if_changed(&state_dir.join(CRL), pem, Access::RootOnly)?;
    write_if_changed(
        Path::new(&format!("{QEMU_DIR}/ca-crl.pem")),
        pem,
        Access::Public,
    )?;
    Ok(())
}

/// Whether this host can take part in live migrations the controller drives:
/// QEMU has its TLS certificates (the migration stream is encrypted).
pub fn ready_for_migration() -> bool {
    Path::new("/etc/pki/qemu/server-cert.pem").exists()
        && Path::new("/etc/pki/qemu/client-cert.pem").exists()
}
