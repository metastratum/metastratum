//! The network model, as loaded from hypermon's database.

use std::net::Ipv4Addr;

use uuid::Uuid;

#[derive(Clone, Debug, Default)]
pub struct Model {
    pub external_networks: Vec<ExternalNetwork>,
    pub vpcs: Vec<Vpc>,
    pub networks: Vec<Network>,
    pub ports: Vec<Port>,
    pub security_groups: Vec<SecurityGroup>,
    pub policies: Vec<Policy>,
    pub floating_ips: Vec<FloatingIp>,
    /// OVN chassis (hosts) and the physical networks they reach.
    pub chassis: Vec<Chassis>,
}

/// An IPv4 prefix such as `10.0.0.0/24`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cidr {
    pub addr: Ipv4Addr,
    pub len: u8,
}

impl Cidr {
    pub fn parse(s: &str) -> Option<Self> {
        let (a, l) = s.split_once('/').unwrap_or((s, "32"));
        let len: u8 = l.parse().ok().filter(|l| *l <= 32)?;
        Some(Self {
            addr: a.parse().ok()?,
            len,
        })
    }

    pub fn is_any(&self) -> bool {
        self.len == 0
    }
}

impl std::fmt::Display for Cidr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.addr, self.len)
    }
}

#[derive(Clone, Debug)]
pub struct ExternalNetwork {
    pub id: Uuid,
    /// Name in the hosts' `ovn-bridge-mappings`.
    pub physical_network: String,
    pub cidr: Cidr,
    pub gateway: Ipv4Addr,
}

#[derive(Clone, Debug)]
pub struct Vpc {
    pub id: Uuid,
    pub tenant_id: Uuid,
    /// Where the VPC's traffic leaves it, with source NAT.
    pub gateway: Option<VpcGateway>,
}

#[derive(Clone, Debug)]
pub struct VpcGateway {
    pub external_network_id: Uuid,
    pub ip: Ipv4Addr,
}

#[derive(Clone, Debug)]
pub struct Network {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub vpc_id: Uuid,
    pub cidr: Cidr,
    pub gateway: Ipv4Addr,
    pub mtu: u32,
}

/// A VM NIC.
#[derive(Clone, Debug)]
pub struct Port {
    pub id: Uuid,
    pub vm_id: Uuid,
    pub tenant_id: Uuid,
    /// The VM's tenant and its ancestors, root first.
    pub tenant_lineage: Vec<Uuid>,
    pub network_id: Uuid,
    pub mac: String,
    pub ip: Ipv4Addr,
    pub security_groups: Vec<Uuid>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Traffic to the VM.
    Ingress,
    /// Traffic from the VM.
    Egress,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Any,
    Tcp,
    Udp,
    Icmp,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Remote {
    Cidr(Cidr),
    /// The members of a security group.
    Group(Uuid),
}

/// What traffic a rule or policy is about.
#[derive(Clone, Debug)]
pub struct Match {
    pub direction: Direction,
    pub protocol: Protocol,
    /// Destination ports (TCP and UDP only), inclusive.
    pub ports: Option<(u16, u16)>,
    pub remote: Remote,
}

#[derive(Clone, Debug)]
pub struct SecurityGroup {
    pub id: Uuid,
    pub tenant_id: Uuid,
    /// Each allows matching traffic (and its replies).
    pub rules: Vec<(Uuid, Match)>,
}

/// A guardrail: denies matching traffic for every VM in the tenant's subtree,
/// whatever their security groups allow.
#[derive(Clone, Debug)]
pub struct Policy {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub rule: Match,
}

#[derive(Clone, Debug)]
pub struct FloatingIp {
    pub id: Uuid,
    pub external_network_id: Uuid,
    pub ip: Ipv4Addr,
    /// The NIC it forwards to.
    pub port: Option<Uuid>,
}

#[derive(Clone, Debug)]
pub struct Chassis {
    /// OVN chassis name (the host's `system-id`).
    pub name: String,
    pub physical_networks: Vec<String>,
}
