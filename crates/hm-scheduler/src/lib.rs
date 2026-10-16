//! VM placement.
//!
//! Pure functions over a snapshot of hosts: filters remove hosts that cannot
//! take the VM, then a weigher picks the best of the rest. Callers take the
//! snapshot under row locks and allocate in the same transaction, so two
//! placements cannot both claim the same capacity.

use std::collections::HashSet;
use std::fmt;

use hm_core::ids::{ClusterId, HostId};

/// A host as the scheduler sees it.
#[derive(Clone, Debug)]
pub struct Host {
    pub id: HostId,
    pub name: String,
    pub cluster: Option<ClusterId>,
    /// Connected and heartbeating.
    pub live: bool,
    /// `ready`, `maintenance` or `decommissioned`.
    pub state: String,
    pub kvm: bool,
    pub cpus: u32,
    pub memory_mib: u64,
    pub cpu_overcommit: f64,
    pub memory_overcommit: f64,
    /// Already allocated to VMs on the host.
    pub used_vcpus: u64,
    pub used_memory_mib: u64,
}

impl Host {
    fn vcpu_capacity(&self) -> f64 {
        f64::from(self.cpus) * self.cpu_overcommit
    }

    fn memory_capacity(&self) -> f64 {
        self.memory_mib as f64 * self.memory_overcommit
    }
}

#[derive(Clone, Debug, Default)]
pub struct Request {
    pub vcpus: u32,
    pub memory_mib: u64,
    /// Clusters the VM may run in; `None` for any.
    pub clusters: Option<HashSet<ClusterId>>,
    /// The VM must run on this host (e.g. its disks are local to it).
    pub host: Option<HostId>,
}

/// Why a host was not chosen.
#[derive(Clone, Debug, PartialEq)]
pub enum Rejection {
    NotConnected,
    NotReady(String),
    NoKvm,
    NoCluster,
    ClusterNotAllowed,
    OtherHostRequired,
    NotEnoughCpu { free: f64 },
    NotEnoughMemory { free_mib: f64 },
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotConnected => f.write_str("not connected"),
            Self::NotReady(s) => write!(f, "in {s}"),
            Self::NoKvm => f.write_str("no KVM"),
            Self::NoCluster => f.write_str("not in a cluster"),
            Self::ClusterNotAllowed => f.write_str("cluster not available to this tenant"),
            Self::OtherHostRequired => f.write_str("the VM's disks live on another host"),
            Self::NotEnoughCpu { free } => write!(f, "only {free:.1} vCPUs free"),
            Self::NotEnoughMemory { free_mib } => write!(f, "only {} MiB free", free_mib.floor()),
        }
    }
}

/// No host can take the VM; each host's reason, for the error message.
#[derive(Clone, Debug, PartialEq)]
pub struct NoPlacement(pub Vec<(String, Rejection)>);

impl fmt::Display for NoPlacement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return f.write_str("no hosts are available");
        }
        f.write_str("no host can take this VM: ")?;
        for (i, (name, why)) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str("; ")?;
            }
            write!(f, "{name} ({why})")?;
        }
        Ok(())
    }
}

impl std::error::Error for NoPlacement {}

fn check(req: &Request, h: &Host) -> Result<(), Rejection> {
    if !h.live {
        return Err(Rejection::NotConnected);
    }
    if h.state != "ready" {
        return Err(Rejection::NotReady(h.state.clone()));
    }
    if !h.kvm {
        return Err(Rejection::NoKvm);
    }
    let Some(cluster) = h.cluster else {
        return Err(Rejection::NoCluster);
    };
    if req.clusters.as_ref().is_some_and(|c| !c.contains(&cluster)) {
        return Err(Rejection::ClusterNotAllowed);
    }
    if req.host.is_some_and(|want| want != h.id) {
        return Err(Rejection::OtherHostRequired);
    }
    let free_cpu = h.vcpu_capacity() - h.used_vcpus as f64;
    if free_cpu < f64::from(req.vcpus) {
        return Err(Rejection::NotEnoughCpu {
            free: free_cpu.max(0.0),
        });
    }
    let free_mem = h.memory_capacity() - h.used_memory_mib as f64;
    if free_mem < req.memory_mib as f64 {
        return Err(Rejection::NotEnoughMemory {
            free_mib: free_mem.max(0.0),
        });
    }
    Ok(())
}

/// How attractive a host is: spread VMs by preferring the most free memory
/// (as a share of capacity), then the most free CPU.
fn score(req: &Request, h: &Host) -> (f64, f64) {
    let mem = 1.0 - (h.used_memory_mib + req.memory_mib) as f64 / h.memory_capacity();
    let cpu = 1.0 - (h.used_vcpus + u64::from(req.vcpus)) as f64 / h.vcpu_capacity();
    (mem, cpu)
}

pub fn place(req: &Request, hosts: &[Host]) -> Result<HostId, NoPlacement> {
    let mut rejected = Vec::new();
    let mut best: Option<(&Host, (f64, f64))> = None;
    for h in hosts {
        match check(req, h) {
            Err(why) => rejected.push((h.name.clone(), why)),
            Ok(()) => {
                let s = score(req, h);
                // Ties go to the alphabetically first host, for determinism.
                if best
                    .as_ref()
                    .is_none_or(|(b, bs)| s > *bs || (s == *bs && h.name < b.name))
                {
                    best = Some((h, s));
                }
            }
        }
    }
    best.map(|(h, _)| h.id).ok_or(NoPlacement(rejected))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(name: &str, cluster: ClusterId, mem: u64, used_mem: u64) -> Host {
        Host {
            id: HostId::new(),
            name: name.into(),
            cluster: Some(cluster),
            live: true,
            state: "ready".into(),
            kvm: true,
            cpus: 4,
            memory_mib: mem,
            cpu_overcommit: 4.0,
            memory_overcommit: 1.0,
            used_vcpus: 0,
            used_memory_mib: used_mem,
        }
    }

    #[test]
    fn spreads_by_free_memory_share() {
        let c = ClusterId::new();
        // `big` has the most free MiB, `small` the largest free share.
        let hosts = [host("big", c, 16384, 8192), host("small", c, 4096, 0)];
        let req = Request {
            vcpus: 1,
            memory_mib: 1024,
            ..Default::default()
        };
        assert_eq!(place(&req, &hosts).unwrap(), hosts[1].id);
    }

    #[test]
    fn filters_explain_themselves() {
        let (c1, c2) = (ClusterId::new(), ClusterId::new());
        let mut offline = host("offline", c1, 8192, 0);
        offline.live = false;
        let mut maint = host("maint", c1, 8192, 0);
        maint.state = "maintenance".into();
        let mut nokvm = host("nokvm", c1, 8192, 0);
        nokvm.kvm = false;
        let mut busy = host("busy", c1, 8192, 0);
        busy.used_vcpus = 15;
        let full = host("full", c1, 2048, 1500);
        let other = host("other", c2, 65536, 0);
        let mut lonely = host("lonely", c1, 65536, 0);
        lonely.cluster = None;
        let hosts = [offline, maint, nokvm, busy, full, other, lonely];
        let req = Request {
            vcpus: 2,
            memory_mib: 1024,
            clusters: Some([c1].into_iter().collect()),
            host: None,
        };
        let err = place(&req, &hosts).unwrap_err();
        let reasons: Vec<_> = err.0.iter().map(|(n, r)| format!("{n}: {r}")).collect();
        assert_eq!(
            reasons,
            [
                "offline: not connected",
                "maint: in maintenance",
                "nokvm: no KVM",
                "busy: only 1.0 vCPUs free",
                "full: only 548 MiB free",
                "other: cluster not available to this tenant",
                "lonely: not in a cluster",
            ]
        );
        assert!(
            err.to_string()
                .starts_with("no host can take this VM: offline (not connected); ")
        );
    }

    #[test]
    fn overcommit_scales_capacity_and_pinning_is_respected() {
        let c = ClusterId::new();
        let mut h = host("h", c, 4096, 3500);
        h.memory_overcommit = 1.5;
        let other = host("x", c, 65536, 0);
        let req = Request {
            vcpus: 16,
            memory_mib: 2048,
            host: Some(h.id),
            ..Default::default()
        };
        assert_eq!(place(&req, &[h.clone(), other]).unwrap(), h.id);
        let req = Request { vcpus: 17, ..req };
        assert!(matches!(
            place(&req, &[h]).unwrap_err().0[0].1,
            Rejection::NotEnoughCpu { .. }
        ));
    }
}
