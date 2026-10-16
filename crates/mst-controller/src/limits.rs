//! Per-VM limits a cluster sets: disk throttling here (network limits are
//! OVN QoS rules, compiled with the rest of the network).

use std::time::Duration;

use mst_core::ids::{ClusterId, HostId};

use crate::state::AppState;

/// The disk limits for VMs on `host` (its cluster's), as agents take them.
pub async fn disk_limits(
    db: &mst_db::sqlx::PgPool,
    host: HostId,
) -> mst_db::Result<Option<mst_proto::DiskLimits>> {
    let (iops, mbps) = mst_db::clusters::disk_limits_of_host(db, host).await?;
    Ok(Some(mst_proto::DiskLimits {
        iops: iops.map_or(0, |v| v as u64),
        bytes_per_sec: mbps.map_or(0, |v| v as u64 * 1_000_000),
    }))
}

/// Disk targets of a VM's spec, as its domain names them.
pub fn disk_targets(spec: &mst_core::vm::VmSpec) -> Vec<String> {
    (0..spec.disks.len())
        .map(|i| format!("vd{}", (b'a' + i as u8) as char))
        .collect()
}

/// Applies a cluster's (changed) disk limits to the VMs on its hosts, live
/// where they run.
pub async fn apply_disk_limits(state: AppState, cluster: ClusterId) {
    let Ok(hosts) = mst_db::hosts::list(&state.db).await else {
        return;
    };
    for h in hosts.iter().filter(|h| h.cluster_id == Some(cluster)) {
        let Ok(Some(limits)) = disk_limits(&state.db, h.id).await else {
            continue;
        };
        let Ok(vms) = mst_db::vms::list_on_host(&state.db, h.id).await else {
            continue;
        };
        for vm in vms {
            let cmd = mst_proto::command::Cmd::SetDiskLimits(mst_proto::SetDiskLimits {
                vm_id: vm.id.to_string(),
                targets: disk_targets(&vm.spec.0),
                limits: Some(limits),
            });
            if let Err(e) = state.agents.call(h.id, cmd, Duration::from_secs(60)).await {
                // It gets them when next defined (started, migrated).
                tracing::warn!(vm = %vm.id, host = %h.name, error = %e, "setting disk limits");
            }
        }
    }
}
