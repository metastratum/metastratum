//! Generated gRPC types for the controller <-> agent protocol.

#[allow(clippy::all)]
pub mod v1 {
    tonic::include_proto!("metastratum.agent.v1");
}

pub use v1::*;

/// The protocol revision this build speaks. Bump it when the protocol
/// changes in a way the other side must know about; new commands are
/// covered by capabilities instead.
///
/// 1: before versioning (agents sent no version). 2: versions, capabilities
/// and the controller's welcome. 3: migrations the controller drives
/// (`MigrationStep`), so hosts' libvirt need not listen for each other.
/// 4: credentials of their own for each migration (`MigrationStep.tls`).
/// 5: the image store; local disks always migrate whole.
pub const PROTOCOL_VERSION: u32 = 5;

/// The oldest agent protocol a controller of this build still serves.
/// Upgrades go controllers first, then agents; agents between this and
/// [`PROTOCOL_VERSION`] keep working meanwhile. 5 is what 0.5 speaks:
/// older agents put disks on cached images, which this build no longer
/// does, and are refused.
pub const MIN_AGENT_PROTOCOL: u32 = 5;

/// A command's name, as agents list them among their capabilities.
pub fn capability(cmd: &command::Cmd) -> &'static str {
    use command::Cmd::*;
    match cmd {
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
        MigrationStats(_) => "migration_stats",
        AbortMigration(_) => "abort_migration",
        DomainInfo(_) => "domain_info",
        RenewCertificate(_) => "renew_certificate",
        MigrationStep(_) => "migration_step",
        SetDiskLimits(_) => "set_disk_limits",
        RemovePool(_) => "remove_pool",
        DeleteImage(_) => "delete_image",
        Decommission(_) => "decommission",
        BackupVm(_) => "backup_vm",
        RestoreVolume(_) => "restore_volume",
        DeleteBackup(_) => "delete_backup",
        SnapshotVm(_) => "snapshot_vm",
        RevertSnapshot(_) => "revert_snapshot",
        DeleteSnapshot(_) => "delete_snapshot",
        ResizeDisk(_) => "resize_disk",
        CloneVolume(_) => "clone_volume",
        SetMemory(_) => "set_memory",
        MemoryStats(_) => "memory_stats",
        SetVcpus(_) => "set_vcpus",
        AttachDisk(_) => "attach_disk",
        DetachDisk(_) => "detach_disk",
        ExportVolume(_) => "export_volume",
        EndExport(_) => "end_export",
        CopyVolume(_) => "copy_volume",
        FetchImage(_) => "fetch_image",
        ExportImage(_) => "export_image",
        CopyImage(_) => "copy_image",
        VolumeFromImage(_) => "volume_from_image",
        PruneStore(_) => "prune_store",
        GuestFileExists(_) => "guest_file_exists",
        AttachNic(_) => "attach_nic",
        DetachNic(_) => "detach_nic",
        ReplugNic(_) => "replug_nic",
        ExportTpm(_) => "export_tpm",
        ImportTpm(_) => "import_tpm",
    }
}

/// Every command this build's agent handles.
pub const CAPABILITIES: &[&str] = &[
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
    "migration_stats",
    "abort_migration",
    "domain_info",
    "renew_certificate",
    "migration_step",
    "set_disk_limits",
    "remove_pool",
    "delete_image",
    "decommission",
    "backup_vm",
    "restore_volume",
    "delete_backup",
    "snapshot_vm",
    "revert_snapshot",
    "delete_snapshot",
    "resize_disk",
    "clone_volume",
    "set_memory",
    "memory_stats",
    "set_vcpus",
    "attach_disk",
    "detach_disk",
    "export_volume",
    "end_export",
    "copy_volume",
    "fetch_image",
    "export_image",
    "copy_image",
    "volume_from_image",
    "prune_store",
    "guest_file_exists",
    "attach_nic",
    "detach_nic",
    "replug_nic",
    "export_tpm",
    "import_tpm",
];

/// What this build's agent does beyond its commands' first versions,
/// announced with its capabilities; older agents lack them.
pub const FEATURES: &[&str] = &[];

#[cfg(test)]
mod tests {
    #[test]
    fn every_command_is_a_capability() {
        // A new command added to the protocol must be named here too; this
        // count is a reminder (prost has no variant list to check against).
        assert_eq!(super::CAPABILITIES.len(), 46);
        let unique: std::collections::HashSet<_> = super::CAPABILITIES.iter().collect();
        assert_eq!(unique.len(), super::CAPABILITIES.len());
    }
}
