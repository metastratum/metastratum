//! Generated gRPC types for the controller <-> agent protocol.

#[allow(clippy::all)]
pub mod v1 {
    tonic::include_proto!("hypermon.agent.v1");
}

pub use v1::*;

/// The protocol revision this build speaks. Bump it when the protocol
/// changes in a way the other side must know about; new commands are
/// covered by capabilities instead.
///
/// 1: before versioning (agents sent no version). 2: versions, capabilities
/// and the controller's welcome. 3: migrations the controller drives
/// (`MigrationStep`), so hosts' libvirt need not listen for each other.
pub const PROTOCOL_VERSION: u32 = 3;

/// The oldest agent protocol a controller of this build still serves.
/// Upgrades go controllers first, then agents; agents between this and
/// [`PROTOCOL_VERSION`] keep working meanwhile.
pub const MIN_AGENT_PROTOCOL: u32 = 1;

/// A command's name, as agents list them among their capabilities.
pub fn capability(cmd: &command::Cmd) -> &'static str {
    use command::Cmd::*;
    match cmd {
        EnsureImage(_) => "ensure_image",
        EnsureVolume(_) => "ensure_volume",
        DeleteVolume(_) => "delete_volume",
        DefineVm(_) => "define_vm",
        StartVm(_) => "start_vm",
        StopVm(_) => "stop_vm",
        RebootVm(_) => "reboot_vm",
        UndefineVm(_) => "undefine_vm",
        OpenConsole(_) => "open_console",
        BaselineCpu(_) => "baseline_cpu",
        PrepareMigration(_) => "prepare_migration",
        MigrateVm(_) => "migrate_vm",
        MigrationStats(_) => "migration_stats",
        AbortMigration(_) => "abort_migration",
        DomainInfo(_) => "domain_info",
        RenewCertificate(_) => "renew_certificate",
        MigrationStep(_) => "migration_step",
        LibvirtTls(_) => "libvirt_tls",
        InstallCrl(_) => "install_crl",
        SetDiskLimits(_) => "set_disk_limits",
    }
}

/// Every command this build's agent handles.
pub const CAPABILITIES: &[&str] = &[
    "ensure_image",
    "ensure_volume",
    "delete_volume",
    "define_vm",
    "start_vm",
    "stop_vm",
    "reboot_vm",
    "undefine_vm",
    "open_console",
    "baseline_cpu",
    "prepare_migration",
    "migrate_vm",
    "migration_stats",
    "abort_migration",
    "domain_info",
    "renew_certificate",
    "migration_step",
    "libvirt_tls",
    "install_crl",
    "set_disk_limits",
];

#[cfg(test)]
mod tests {
    #[test]
    fn every_command_is_a_capability() {
        // A new command added to the protocol must be named here too; this
        // count is a reminder (prost has no variant list to check against).
        assert_eq!(super::CAPABILITIES.len(), 20);
        let unique: std::collections::HashSet<_> = super::CAPABILITIES.iter().collect();
        assert_eq!(unique.len(), super::CAPABILITIES.len());
    }
}
