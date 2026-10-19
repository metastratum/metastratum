//! Which hosts run a VM's CPU: hosts' CPUs and VMs' CPU models as lists of
//! every feature they have (libvirt works them out: `BaselineCpu` with
//! `expand`), compared when a VM is placed, migrated or restarted elsewhere.
//!
//! A VM that may migrate has its cluster's baseline model (what all its
//! hosts ran when it was made); one that may not, its host's CPU as is. A
//! host runs the first if it offers every feature of the model; the second
//! if it offers every feature of that host.

use std::time::Duration;

use anyhow::{Context, Result};
use mst_core::ids::HostId;
use mst_core::vm::CpuSpec;
use mst_db::hosts::Host;
use mst_db::vms::Vm;

use crate::state::AppState;

const QUICK: Duration = Duration::from_secs(60);
/// How often features still unknown are worked out.
const EVERY: Duration = Duration::from_secs(60);

/// What a VM's CPU needs of a host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Needs {
    pub vendor: Option<String>,
    pub features: Vec<String>,
}

/// What `vm` needs of a host's CPU: its model's features, or (with its
/// host's CPU as is) that host's. `None` while not known.
pub fn needs(vm: &Vm, hosts: &[Host]) -> Option<Needs> {
    match (&vm.spec.0.cpu, vm.spec.0.migratable) {
        (Some(cpu), true) => Some(Needs {
            vendor: cpu.vendor.clone(),
            features: vm.cpu_features.clone()?,
        }),
        _ => {
            let host = hosts.iter().find(|h| Some(h.id) == vm.host_id)?;
            Some(Needs {
                vendor: host.cpu_vendor.clone(),
                features: host.cpu_features.clone()?,
            })
        }
    }
}

/// What the host lacks of `needs`: another vendor, or features (empty:
/// it runs the VM). `None` while the host's features are not known.
pub fn lacking(host: &Host, needs: &Needs) -> Option<Vec<String>> {
    let features = host.cpu_features.as_ref()?;
    if let (Some(want), Some(have)) = (&needs.vendor, &host.cpu_vendor)
        && want != have
    {
        return Some(vec![format!("a {want} CPU (it has {have})")]);
    }
    Some(
        needs
            .features
            .iter()
            .filter(|f| !features.contains(f))
            .cloned()
            .collect(),
    )
}

/// Says what the host lacks, for messages: "lacks avx512f, sha-ni and 3 more".
pub fn describe(lacks: &[String]) -> String {
    match lacks.len() {
        0 => "nothing".into(),
        1..=3 => lacks.join(", "),
        n => format!("{} and {} more", lacks[..3].join(", "), n - 3),
    }
}

/// Every feature of `cpu`, worked out by libvirt on `host`; `None` from an
/// agent that cannot (older than this).
pub async fn expand(state: &AppState, host: HostId, cpu: &CpuSpec) -> Result<Option<Needs>> {
    let cmd = mst_proto::command::Cmd::BaselineCpu(mst_proto::BaselineCpu {
        cpus_json: vec![serde_json::to_string(cpu)?],
        expand: true,
    });
    let answer: serde_json::Value = state
        .agents
        .query(host, cmd, QUICK)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    if answer["expanded"] != true {
        return Ok(None);
    }
    let cpu: CpuSpec = serde_json::from_value(answer).context("an expanded CPU model")?;
    Ok(Some(Needs {
        vendor: cpu.vendor,
        features: cpu
            .features
            .into_iter()
            .filter(|f| f.policy == "require")
            .map(|f| f.name)
            .collect(),
    }))
}

/// Works out the features of hosts' CPUs and VMs' CPU models not known yet
/// (on the leader).
pub async fn learn(state: AppState) {
    loop {
        if let Err(e) = learn_once(&state).await {
            tracing::warn!(error = %format!("{e:#}"), "working out CPU features");
        }
        tokio::time::sleep(EVERY).await;
    }
}

async fn learn_once(state: &AppState) -> Result<()> {
    let hosts: Vec<Host> = mst_db::hosts::list(&state.db).await?;
    for h in hosts
        .iter()
        .filter(|h| h.is_live() && !h.cpu_json.is_empty())
    {
        if h.cpu_features_of.as_deref() == Some(h.cpu_json.as_str()) {
            continue;
        }
        let cpu: CpuSpec = serde_json::from_str(&h.cpu_json)?;
        if let Some(n) = expand(state, h.id, &cpu).await? {
            mst_db::hosts::set_cpu_features(
                &state.db,
                h.id,
                n.vendor.as_deref(),
                &n.features,
                &h.cpu_json,
            )
            .await?;
        }
    }
    for vm in mst_db::vms::all(&state.db).await? {
        let Some(cpu) = vm.spec.0.cpu.as_ref().filter(|_| vm.cpu_features.is_none()) else {
            continue;
        };
        // Any connected host of its cluster works it out.
        let cluster = hosts
            .iter()
            .find(|h| Some(h.id) == vm.host_id)
            .and_then(|h| h.cluster_id);
        let Some(host) = hosts.iter().find(|h| {
            h.is_live()
                && (Some(h.id) == vm.host_id || (cluster.is_some() && h.cluster_id == cluster))
        }) else {
            continue;
        };
        if let Some(n) = expand(state, host.id, cpu).await? {
            mst_db::vms::set_cpu_features(&state.db, vm.id, Some(&n.features)).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(vendor: &str, features: &[&str]) -> Host {
        Host {
            id: HostId::new(),
            name: "h".into(),
            address: String::new(),
            state: "ready".into(),
            cpu_count: 1,
            memory_mib: 1,
            cpu_model: String::new(),
            kvm: true,
            libvirt_version: String::new(),
            ovn_chassis: String::new(),
            physical_networks: vec![],
            cpu_json: String::new(),
            migration_ready: true,
            fence_kind: None,
            connected_since: None,
            agent_protocol: 4,
            agent_version: String::new(),
            cert_fingerprint: String::new(),
            cert_expires_at: chrono::Utc::now(),
            cluster_id: None,
            connected_to: None,
            last_seen_at: None,
            created_at: chrono::Utc::now(),
            pending_facts: None,
            cpu_vendor: Some(vendor.into()),
            cpu_features: Some(features.iter().map(|f| (*f).to_owned()).collect()),
            cpu_features_of: None,
        }
    }

    #[test]
    fn a_host_runs_a_model_it_has_every_feature_of() {
        let needs = Needs {
            vendor: Some("AMD".into()),
            features: vec!["sse4.2".into(), "avx2".into(), "vaes".into()],
        };
        assert_eq!(
            lacking(&host("AMD", &["sse4.2", "avx2", "vaes", "pku"]), &needs),
            Some(vec![])
        );
        assert_eq!(
            lacking(&host("AMD", &["sse4.2", "avx2"]), &needs),
            Some(vec!["vaes".into()])
        );
        let intel = lacking(&host("Intel", &["sse4.2", "avx2", "vaes"]), &needs).unwrap();
        assert!(intel[0].contains("AMD"), "{intel:?}");
        let mut unknown = host("AMD", &[]);
        unknown.cpu_features = None;
        assert_eq!(lacking(&unknown, &needs), None);
        assert_eq!(
            describe(&["a".into(), "b".into(), "c".into(), "d".into(), "e".into()]),
            "a, b, c and 2 more"
        );
    }
}
