//! Deciding whether and where a VM can live-migrate.

use std::collections::HashSet;

use mst_core::ids::{HostId, TaskId};
use mst_db::sqlx::PgConnection;
use mst_db::vms::Vm;

use crate::state::AppState;

/// Why a migration cannot happen, in words for whoever asked.
#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("{0}")]
    Refused(String),
    #[error(transparent)]
    Db(#[from] mst_db::Error),
}

fn refuse<T>(why: impl Into<String>) -> Result<T, Refusal> {
    Err(Refusal::Refused(why.into()))
}

/// Checks that `vm` can live-migrate and picks its destination: `want` if
/// given (and suitable), otherwise the scheduler's choice within the VM's
/// cluster. Records the migration in `tx`, which reserves the destination's
/// capacity and marks the VM migrating. Returns the destination.
pub async fn plan(
    tx: &mut PgConnection,
    vm: &Vm,
    want: Option<HostId>,
    exclude: &[HostId],
    task: TaskId,
) -> Result<(HostId, String), Refusal> {
    if vm.status != "ready" {
        return refuse(format!("{} is busy ({})", vm.name, vm.status));
    }
    if !vm.spec.0.migratable {
        return refuse(format!(
            "{} is not migratable: it uses its host's full CPU and disk caching",
            vm.name
        ));
    }
    if vm.power != "running" {
        return refuse(format!(
            "{} is not running; only running VMs can be live-migrated",
            vm.name
        ));
    }
    let Some(source) = vm.host_id else {
        return refuse(format!("{} is not on a host", vm.name));
    };
    if want == Some(source) {
        return refuse(format!("{} already runs there", vm.name));
    }
    let hosts = mst_db::clusters::candidates_locked(tx).await?;
    let Some(src) = hosts.iter().find(|h| h.id == source) else {
        return refuse("the VM's host no longer exists");
    };
    if !src.live {
        return refuse(format!("{} is not connected", src.name));
    }
    let Some(cluster) = src.cluster_id else {
        return refuse(format!(
            "{} is in no cluster; VMs migrate within their host's cluster",
            src.name
        ));
    };
    // Migration-specific requirements; the scheduler checks the rest.
    let mut rejected = vec![];
    let candidates: Vec<mst_scheduler::Host> = hosts
        .iter()
        .filter(|h| h.id != source && !exclude.contains(&h.id) && want.is_none_or(|w| w == h.id))
        .filter(|h| {
            if h.cluster_id != Some(cluster) {
                rejected.push(format!("{}: in another cluster", h.name));
                false
            } else if !h.migration_ready {
                rejected.push(format!(
                    "{}: does not accept migrations (libvirt TLS is not set up)",
                    h.name
                ));
                false
            } else {
                true
            }
        })
        .map(|c| mst_scheduler::Host {
            id: c.id,
            name: c.name.clone(),
            cluster: c.cluster_id,
            live: c.live,
            state: c.state.clone(),
            kvm: c.kvm,
            cpus: c.cpu_count.max(0) as u32,
            memory_mib: c.memory_mib.max(0) as u64,
            cpu_overcommit: f64::from(c.cpu_overcommit),
            memory_overcommit: f64::from(c.memory_overcommit),
            used_vcpus: c.used_vcpus.max(0) as u64,
            used_memory_mib: c.used_memory_mib.max(0) as u64,
        })
        .collect();
    if want.is_some() && candidates.is_empty() && rejected.is_empty() {
        return refuse("that host does not exist");
    }
    let spec = &vm.spec.0;
    let request = mst_scheduler::Request {
        vcpus: spec.vcpus,
        memory_mib: spec.memory_mib,
        clusters: Some(HashSet::from([cluster])),
        host: None,
    };
    let dest = match mst_scheduler::place(&request, &candidates) {
        Ok(d) => d,
        Err(no) => {
            rejected.extend(no.0.into_iter().map(|(name, why)| format!("{name}: {why}")));
            return refuse(if rejected.is_empty() {
                format!("no other host in {}'s cluster", src.name)
            } else {
                format!("no host can take {}: {}", vm.name, rejected.join("; "))
            });
        }
    };
    let name = hosts
        .iter()
        .find(|h| h.id == dest)
        .map(|h| h.name.clone())
        .unwrap_or_default();
    mst_db::migrations::begin(tx, vm.id, task, source, dest, spec.vcpus, spec.memory_mib).await?;
    Ok((dest, name))
}

/// The baseline CPU of `host`'s cluster (or the host's own model when it is
/// in none), computed by libvirt on that host. `None` when it cannot be
/// computed: the VM then gets the host's CPU as is, and can only migrate to
/// identical hosts.
pub async fn baseline_cpu(state: &AppState, host: HostId) -> Option<mst_core::vm::CpuSpec> {
    let hosts = match mst_db::hosts::list(&state.db).await {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(error = %e, "listing hosts for the CPU baseline");
            return None;
        }
    };
    let cluster = hosts.iter().find(|h| h.id == host)?.cluster_id;
    let cpus: Vec<String> = hosts
        .iter()
        .filter(|h| h.state != "decommissioned" && !h.cpu_json.is_empty())
        .filter(|h| {
            if cluster.is_some() {
                h.cluster_id == cluster
            } else {
                h.id == host
            }
        })
        .map(|h| h.cpu_json.clone())
        .collect();
    if cpus.is_empty() {
        tracing::warn!(%host, "no host reported its CPU model; the VM gets the host's CPU");
        return None;
    }
    let cmd = mst_proto::command::Cmd::BaselineCpu(mst_proto::BaselineCpu { cpus_json: cpus });
    match state
        .agents
        .query(host, cmd, std::time::Duration::from_secs(60))
        .await
    {
        Ok(cpu) => Some(cpu),
        Err(e) => {
            tracing::warn!(%host, error = %e, "computing the cluster's CPU baseline; the VM gets the host's CPU");
            None
        }
    }
}
