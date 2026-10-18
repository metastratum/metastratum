//! The model → northbound rows.
//!
//! | metastratum            | OVN                                                        |
//! |---------------------|------------------------------------------------------------|
//! | VPC                 | Logical_Router `mst-vpc-<id>`                               |
//! | network             | Logical_Switch `mst-net-<id>`, router port, DHCP_Options    |
//! | NIC                 | Logical_Switch_Port named by the NIC ID (the OVS iface-id) |
//! | external network    | Logical_Switch `mst-ext-<id>` with a localnet port          |
//! | VPC gateway         | router port `mst-gw-<vpc>` on gateway chassis, SNAT, route  |
//! | floating IP         | NAT dnat_and_snat                                          |
//! | load balancer       | Load_Balancer `mst-lb-<id>-<proto>` on the VPC's router and switches |
//! | security group      | Port_Group `mst_sg_<id>` with allow-related ACLs            |
//! | guardrail policies  | Port_Group `mst_gr_<tenant>` with drop ACLs                 |
//!
//! ACL priorities form bands. Every VM port is in `mst_vms`, whose drops at
//! [`PRIO_DEFAULT_DROP`] make "not allowed" the default; security groups
//! allow above that; guardrails drop above every allow, so nothing a
//! security group says can override them. Guardrails only deny, so their
//! order among themselves does not matter. DHCP sits above everything:
//! without it a VM has no address at all.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::Ipv4Addr;

use uuid::Uuid;

use crate::model::*;
use crate::row::{Datum, KEY, MANAGED, Row, Table};

/// The port group of every VM port.
const PG_VMS: &str = "mst_vms";

pub const PRIO_DEFAULT_DROP: i64 = 1000;
pub const PRIO_ALLOW: i64 = 1001;
pub const PRIO_GUARDRAIL: i64 = 2000;
pub const PRIO_DHCP: i64 = 3000;
/// QoS rules (a table of their own; one rule per port and direction).
pub const PRIO_QOS: i64 = 100;

/// At most this many hosts back each VPC gateway (one active, the rest
/// standing by).
const GATEWAY_CHASSIS: usize = 3;

/// The rows that implement a model.
#[derive(Debug, Default)]
pub struct Desired {
    pub rows: Vec<Row>,
    /// Things that are configured but cannot work yet, e.g. a gateway with no
    /// host that reaches its external network.
    pub warnings: Vec<String>,
}

impl Desired {
    pub fn find(&self, key: &str) -> Option<&Row> {
        self.rows.iter().find(|r| r.key == key)
    }
}

pub fn switch_name(network: Uuid) -> String {
    format!("mst-net-{network}")
}

pub fn port_group_name(prefix: &str, id: Uuid) -> String {
    format!("mst_{prefix}_{}", id.simple())
}

/// A locally administered MAC from an ID's random tail, with a per-role
/// prefix so a VPC's ports never share one.
fn mac(prefix: u8, id: Uuid) -> String {
    let b = id.as_bytes();
    format!(
        "02:{prefix:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        b[12], b[13], b[14], b[15]
    )
}

/// The DHCP server's MAC, as M1 derived it (kept so existing rows match).
fn dhcp_server_mac(network: Uuid) -> String {
    let hex = network.simple().to_string();
    let t = &hex[hex.len() - 8..];
    format!("02:69:{}:{}:{}:{}", &t[0..2], &t[2..4], &t[4..6], &t[6..8])
}

struct Builder {
    rows: Vec<Row>,
}

impl Builder {
    fn add(
        &mut self,
        table: Table,
        key: String,
        tenant: Option<Uuid>,
        cols: Vec<(&'static str, Datum)>,
    ) {
        let mut ids = BTreeMap::from([
            (MANAGED.to_owned(), "true".to_owned()),
            (KEY.to_owned(), key.clone()),
        ]);
        if let Some(t) = tenant {
            ids.insert("metastratum-tenant".into(), t.to_string());
        }
        let mut cols: BTreeMap<_, _> = cols.into_iter().collect();
        cols.insert("external_ids", Datum::Map(ids));
        self.rows.push(Row { table, key, cols });
    }
}

fn s(v: impl Into<String>) -> Datum {
    Datum::Str(v.into())
}

fn strs<I: IntoIterator<Item = S>, S: Into<String>>(v: I) -> Datum {
    Datum::Strs(v.into_iter().map(Into::into).collect())
}

fn map<'a>(kv: impl IntoIterator<Item = (&'a str, String)>) -> Datum {
    Datum::Map(kv.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

fn refs(v: impl IntoIterator<Item = String>) -> Datum {
    Datum::Refs(v.into_iter().collect())
}

/// An ACL match for traffic to (ingress) or from (egress) a port group.
pub fn acl_match(port_group: &str, m: &Match) -> String {
    let (port, remote_field) = match m.direction {
        Direction::Ingress => (format!("outport == @{port_group}"), "ip4.src"),
        Direction::Egress => (format!("inport == @{port_group}"), "ip4.dst"),
    };
    let mut out = format!("{port} && ip4");
    match &m.remote {
        Remote::Cidr(c) if c.is_any() => {}
        Remote::Cidr(c) if c.len == 32 => out += &format!(" && {remote_field} == {}", c.addr),
        Remote::Cidr(c) => out += &format!(" && {remote_field} == {c}"),
        Remote::Group(g) => {
            out += &format!(" && {remote_field} == ${}_ip4", port_group_name("sg", *g))
        }
    }
    let l4 = match m.protocol {
        Protocol::Any => None,
        Protocol::Icmp => {
            out += " && icmp4";
            None
        }
        Protocol::Tcp => Some("tcp"),
        Protocol::Udp => Some("udp"),
    };
    if let Some(p) = l4 {
        out += &format!(" && {p}");
        match m.ports {
            Some((a, b)) if a == b => out += &format!(" && {p}.dst == {a}"),
            Some((a, b)) => out += &format!(" && {p}.dst >= {a} && {p}.dst <= {b}"),
            None => {}
        }
    }
    out
}

fn acl_direction(d: Direction) -> &'static str {
    match d {
        // Evaluated as traffic leaves the switch towards the port.
        Direction::Ingress => "to-lport",
        Direction::Egress => "from-lport",
    }
}

/// OVN's DHCP option for name servers: one address, or `{a, b}`.
fn dns_option(servers: &[Ipv4Addr]) -> Option<(&'static str, String)> {
    let list: Vec<String> = servers.iter().map(ToString::to_string).collect();
    match list.len() {
        0 => None,
        1 => Some(("dns_server", list[0].clone())),
        _ => Some(("dns_server", format!("{{{}}}", list.join(", ")))),
    }
}

pub fn compile(m: &Model) -> Desired {
    let mut b = Builder { rows: vec![] };
    let mut warnings = vec![];

    let networks: HashMap<Uuid, &Network> = m.networks.iter().map(|n| (n.id, n)).collect();
    let externals: HashMap<Uuid, &ExternalNetwork> =
        m.external_networks.iter().map(|e| (e.id, e)).collect();
    let vpcs: HashMap<Uuid, &Vpc> = m.vpcs.iter().map(|v| (v.id, v)).collect();
    let ports: HashMap<Uuid, &Port> = m.ports.iter().map(|p| (p.id, p)).collect();

    // Floating IPs that can work: the NIC's VPC leaves through the floating
    // IP's external network.
    let mut fips_by_vpc: BTreeMap<Uuid, Vec<(&FloatingIp, &Port)>> = BTreeMap::new();
    for f in &m.floating_ips {
        let Some(port) = f.port.and_then(|p| ports.get(&p)) else {
            continue;
        };
        let vpc = networks
            .get(&port.network_id)
            .and_then(|n| vpcs.get(&n.vpc_id));
        match vpc.and_then(|v| v.gateway.as_ref().map(|g| (v, g))) {
            Some((v, g)) if g.external_network_id == f.external_network_id => {
                fips_by_vpc.entry(v.id).or_default().push((f, port));
            }
            _ => warnings.push(format!(
                "floating IP {} cannot reach its NIC: the NIC's VPC does not use that external network",
                f.ip
            )),
        }
    }

    // Load balancers: a row per protocol, on the VPC's router (traffic from
    // outside) and every one of its switches (traffic from its VMs).
    let mut lbs_by_vpc: BTreeMap<Uuid, Vec<String>> = BTreeMap::new();
    for lb in &m.load_balancers {
        if !vpcs.contains_key(&lb.vpc_id) {
            warnings.push(format!("load balancer {} is in an unknown VPC", lb.id));
            continue;
        }
        let members: Vec<&Port> = lb
            .members
            .iter()
            .filter_map(|id| match ports.get(id) {
                Some(p)
                    if networks
                        .get(&p.network_id)
                        .is_some_and(|n| n.vpc_id == lb.vpc_id) =>
                {
                    Some(*p)
                }
                _ => {
                    warnings.push(format!(
                        "load balancer {}: member {id} is not a NIC in its VPC",
                        lb.id
                    ));
                    None
                }
            })
            .collect();
        for (proto, name) in [(Protocol::Tcp, "tcp"), (Protocol::Udp, "udp")] {
            let listeners: Vec<&Listener> = lb
                .listeners
                .iter()
                .filter(|l| l.protocol == proto)
                .collect();
            if listeners.is_empty() {
                continue;
            }
            let mut vips = BTreeMap::new();
            let mut checks = vec![];
            for l in &listeners {
                let backends: Vec<String> = members
                    .iter()
                    .map(|p| format!("{}:{}", p.ip, l.target_port))
                    .collect();
                let vip = format!("{}:{}", lb.vip, l.port);
                vips.insert(vip.clone(), backends.join(","));
                if lb.health_check && !members.is_empty() {
                    let key = format!("lbhc/{}/{name}/{}", lb.id, l.port);
                    b.add(
                        Table::LoadBalancerHealthCheck,
                        key.clone(),
                        Some(lb.tenant_id),
                        vec![
                            ("vip", s(vip)),
                            (
                                "options",
                                map([
                                    ("interval", "5".to_owned()),
                                    ("timeout", "3".to_owned()),
                                    ("success_count", "2".to_owned()),
                                    ("failure_count", "3".to_owned()),
                                ]),
                            ),
                        ],
                    );
                    checks.push(key);
                }
            }
            // Health checks come from an address of each member's network.
            // OVN lets them past every ACL itself (by its service monitor
            // MAC address, which guests cannot send from), so no rule here
            // lets anything in for them.
            let mut mappings = BTreeMap::new();
            if !checks.is_empty() {
                for p in &members {
                    match networks.get(&p.network_id).and_then(|n| n.monitor_ip) {
                        Some(src) => {
                            mappings.insert(p.ip.to_string(), format!("{}:{src}", p.id));
                        }
                        None => warnings.push(format!(
                            "load balancer {}: network {} has no health check address",
                            lb.id, p.network_id
                        )),
                    }
                }
            }
            let key = format!("lb/{}/{name}", lb.id);
            b.add(
                Table::LoadBalancer,
                key.clone(),
                Some(lb.tenant_id),
                vec![
                    ("name", s(format!("mst-lb-{}-{name}", lb.id))),
                    ("protocol", strs([name])),
                    ("vips", Datum::Map(vips)),
                    ("health_check", refs(checks)),
                    ("ip_port_mappings", Datum::Map(mappings)),
                ],
            );
            lbs_by_vpc.entry(lb.vpc_id).or_default().push(key);
        }
    }

    // External networks.
    for e in &m.external_networks {
        let attached: Vec<Uuid> = m
            .vpcs
            .iter()
            .filter(|v| {
                v.gateway
                    .as_ref()
                    .is_some_and(|g| g.external_network_id == e.id)
            })
            .map(|v| v.id)
            .collect();
        let localnet = format!("lsp/ext/{}/localnet", e.id);
        b.add(
            Table::LogicalSwitchPort,
            localnet.clone(),
            None,
            vec![
                ("name", s(format!("mst-ext-{}-localnet", e.id))),
                ("type", s("localnet")),
                ("addresses", strs(["unknown"])),
                ("port_security", strs::<[&str; 0], _>([])),
                (
                    "options",
                    map([("network_name", e.physical_network.clone())]),
                ),
                ("dhcpv4_options", Datum::Ref(None)),
            ],
        );
        let mut sw_ports = vec![localnet];
        sw_ports.extend(attached.iter().map(|v| format!("lsp/gw/{v}")));
        b.add(
            Table::LogicalSwitch,
            format!("ls/ext/{}", e.id),
            None,
            vec![
                ("name", s(format!("mst-ext-{}", e.id))),
                ("ports", refs(sw_ports)),
            ],
        );
    }

    // VPCs: routers, and gateways where they have one.
    for v in &m.vpcs {
        let nets: Vec<&Network> = m.networks.iter().filter(|n| n.vpc_id == v.id).collect();
        let mut router_ports: Vec<String> =
            nets.iter().map(|n| format!("lrp/net/{}", n.id)).collect();
        let mut nat = vec![];
        let mut routes = vec![];
        if let Some(g) = &v.gateway {
            match externals.get(&g.external_network_id) {
                None => warnings.push(format!("VPC {} uses an unknown external network", v.id)),
                Some(e) => {
                    let mut candidates: Vec<&Chassis> = m
                        .chassis
                        .iter()
                        .filter(|c| c.physical_networks.contains(&e.physical_network))
                        .collect();
                    candidates.sort_by(|a, b| a.name.cmp(&b.name));
                    if candidates.is_empty() {
                        warnings.push(format!(
                            "no host reaches physical network {} for VPC {}'s gateway",
                            e.physical_network, v.id
                        ));
                    } else {
                        // Spread VPCs' active gateways over the hosts.
                        let start = (v.id.as_u128() % candidates.len() as u128) as usize;
                        candidates.rotate_left(start);
                    }
                    let mut chassis_keys = vec![];
                    for (i, c) in candidates.iter().take(GATEWAY_CHASSIS).enumerate() {
                        let key = format!("gc/{}/{}", v.id, c.name);
                        b.add(
                            Table::GatewayChassis,
                            key.clone(),
                            Some(v.tenant_id),
                            vec![
                                ("name", s(format!("mst-gw-{}-{}", v.id, c.name))),
                                ("chassis_name", s(c.name.clone())),
                                ("priority", Datum::Int((GATEWAY_CHASSIS - i) as i64)),
                            ],
                        );
                        chassis_keys.push(key);
                    }
                    b.add(
                        Table::LogicalRouterPort,
                        format!("lrp/gw/{}", v.id),
                        Some(v.tenant_id),
                        vec![
                            ("name", s(format!("mst-gw-{}", v.id))),
                            ("mac", s(mac(0x6e, v.id))),
                            ("networks", strs([format!("{}/{}", g.ip, e.cidr.len)])),
                            ("gateway_chassis", refs(chassis_keys)),
                        ],
                    );
                    router_ports.push(format!("lrp/gw/{}", v.id));
                    b.add(
                        Table::LogicalSwitchPort,
                        format!("lsp/gw/{}", v.id),
                        Some(v.tenant_id),
                        vec![
                            ("name", s(format!("mst-gsp-{}", v.id))),
                            ("type", s("router")),
                            ("addresses", strs(["router"])),
                            ("port_security", strs::<[&str; 0], _>([])),
                            (
                                "options",
                                map([("router-port", format!("mst-gw-{}", v.id))]),
                            ),
                            ("dhcpv4_options", Datum::Ref(None)),
                        ],
                    );
                    let route = format!("route/{}/default", v.id);
                    b.add(
                        Table::StaticRoute,
                        route.clone(),
                        Some(v.tenant_id),
                        vec![
                            ("ip_prefix", s("0.0.0.0/0")),
                            ("nexthop", s(e.gateway.to_string())),
                        ],
                    );
                    routes.push(route);
                    for n in &nets {
                        let key = format!("nat/snat/{}", n.id);
                        b.add(
                            Table::Nat,
                            key.clone(),
                            Some(v.tenant_id),
                            vec![
                                ("type", s("snat")),
                                ("external_ip", s(g.ip.to_string())),
                                ("logical_ip", s(n.cidr.to_string())),
                            ],
                        );
                        nat.push(key);
                    }
                    for (f, port) in fips_by_vpc.get(&v.id).into_iter().flatten() {
                        let key = format!("nat/fip/{}", f.id);
                        b.add(
                            Table::Nat,
                            key.clone(),
                            Some(v.tenant_id),
                            vec![
                                ("type", s("dnat_and_snat")),
                                ("external_ip", s(f.ip.to_string())),
                                ("logical_ip", s(port.ip.to_string())),
                            ],
                        );
                        nat.push(key);
                    }
                }
            }
        }
        b.add(
            Table::LogicalRouter,
            format!("lr/{}", v.id),
            Some(v.tenant_id),
            vec![
                ("name", s(format!("mst-vpc-{}", v.id))),
                ("ports", refs(router_ports)),
                ("nat", refs(nat)),
                ("static_routes", refs(routes)),
                (
                    "load_balancer",
                    refs(lbs_by_vpc.get(&v.id).cloned().unwrap_or_default()),
                ),
            ],
        );
    }

    // Networks: a switch, its side of the router, DHCP.
    for n in &m.networks {
        b.add(
            Table::LogicalRouterPort,
            format!("lrp/net/{}", n.id),
            Some(n.tenant_id),
            vec![
                ("name", s(format!("mst-rp-{}", n.id))),
                ("mac", s(mac(0x6d, n.id))),
                ("networks", strs([format!("{}/{}", n.gateway, n.cidr.len)])),
                ("gateway_chassis", refs([])),
            ],
        );
        b.add(
            Table::LogicalSwitchPort,
            format!("lsp/rtr/{}", n.id),
            Some(n.tenant_id),
            vec![
                ("name", s(format!("mst-rsp-{}", n.id))),
                ("type", s("router")),
                ("addresses", strs(["router"])),
                ("port_security", strs::<[&str; 0], _>([])),
                (
                    "options",
                    map([("router-port", format!("mst-rp-{}", n.id))]),
                ),
                ("dhcpv4_options", Datum::Ref(None)),
            ],
        );
        b.add(
            Table::DhcpOptions,
            format!("dhcp/{}", n.id),
            Some(n.tenant_id),
            vec![
                ("cidr", s(n.cidr.to_string())),
                (
                    "options",
                    map([
                        ("server_id", n.gateway.to_string()),
                        ("server_mac", dhcp_server_mac(n.id)),
                        ("lease_time", "3600".to_owned()),
                        ("router", n.gateway.to_string()),
                        ("mtu", n.mtu.to_string()),
                    ]
                    .into_iter()
                    .chain(dns_option(&m.dns_servers))),
                ),
            ],
        );
        let mut sw_ports = vec![format!("lsp/rtr/{}", n.id)];
        sw_ports.extend(
            m.ports
                .iter()
                .filter(|p| p.network_id == n.id)
                .map(|p| format!("lsp/{}", p.id)),
        );
        // Rate limits: metered in both directions, per limited port.
        let mut qos = vec![];
        for p in m.ports.iter().filter(|p| p.network_id == n.id) {
            let Some(bw) = p.bandwidth else { continue };
            for (dir, direction, matcher) in [
                ("out", "from-lport", format!("inport == \"{}\"", p.id)),
                ("in", "to-lport", format!("outport == \"{}\"", p.id)),
            ] {
                let key = format!("qos/{}/{dir}", p.id);
                b.add(
                    Table::Qos,
                    key.clone(),
                    Some(p.tenant_id),
                    vec![
                        ("direction", s(direction)),
                        ("priority", Datum::Int(PRIO_QOS)),
                        ("match", s(matcher)),
                        (
                            "bandwidth",
                            Datum::IntMap(BTreeMap::from([
                                ("rate".to_owned(), i64::from(bw.kbps)),
                                ("burst".to_owned(), i64::from(bw.burst_kbit())),
                            ])),
                        ),
                    ],
                );
                qos.push(key);
            }
        }
        // DHCP gets through whatever security groups and guardrails say, but
        // only to OVN's own DHCP server: broadcast, or its address (which
        // is the gateway's) when a lease is renewed. Per switch, as each
        // network has its own.
        let dhcp = format!("acl/dhcp/{}", n.id);
        b.add(
            Table::Acl,
            dhcp.clone(),
            Some(n.tenant_id),
            vec![
                ("direction", s("from-lport")),
                ("priority", Datum::Int(PRIO_DHCP)),
                (
                    "match",
                    s(format!(
                        "inport == @{PG_VMS} && ip4 && udp && udp.src == 68 && udp.dst == 67 \
                         && ip4.dst == {{255.255.255.255, {}}}",
                        n.gateway
                    )),
                ),
                ("action", s("allow-related")),
            ],
        );
        b.add(
            Table::LogicalSwitch,
            format!("ls/{}", n.id),
            Some(n.tenant_id),
            vec![
                ("name", s(switch_name(n.id))),
                ("ports", refs(sw_ports)),
                ("acls", refs(vec![dhcp])),
                ("qos_rules", refs(qos)),
                (
                    "load_balancer",
                    refs(lbs_by_vpc.get(&n.vpc_id).cloned().unwrap_or_default()),
                ),
            ],
        );
        if !vpcs.contains_key(&n.vpc_id) {
            warnings.push(format!("network {} belongs to an unknown VPC", n.id));
        }
    }

    // VM ports. Port security pins each to its MAC and IP: no spoofing.
    for p in &m.ports {
        if !networks.contains_key(&p.network_id) {
            warnings.push(format!("NIC {} is on an unknown network", p.id));
            continue;
        }
        let address = format!("{} {}", p.mac, p.ip);
        // Only the VM's host may bind the port, so no other host can claim
        // it. A migrating port is bound on both hosts (multi-chassis).
        let mut options = BTreeMap::new();
        if !p.chassis.is_empty() {
            options.insert("requested-chassis".to_owned(), p.chassis.join(","));
        }
        if p.migrating && p.chassis.len() > 1 {
            options.insert("activation-strategy".to_owned(), "rarp".to_owned());
        }
        b.add(
            Table::LogicalSwitchPort,
            format!("lsp/{}", p.id),
            Some(p.tenant_id),
            vec![
                ("name", s(p.id.to_string())),
                ("type", s("")),
                ("addresses", strs([address.clone()])),
                ("port_security", strs([address])),
                ("options", Datum::Map(options)),
                (
                    "dhcpv4_options",
                    Datum::Ref(Some(format!("dhcp/{}", p.network_id))),
                ),
            ],
        );
    }
    let vm_ports: Vec<&Port> = m
        .ports
        .iter()
        .filter(|p| networks.contains_key(&p.network_id))
        .collect();

    // Default deny for every VM port; DHCP always works.
    let pg_vms = PG_VMS;
    let mut acls = vec![];
    for (name, direction, prio, matcher, action) in [
        (
            "drop-in",
            "to-lport",
            PRIO_DEFAULT_DROP,
            format!("outport == @{pg_vms} && ip"),
            "drop",
        ),
        (
            "drop-out",
            "from-lport",
            PRIO_DEFAULT_DROP,
            format!("inport == @{pg_vms} && ip"),
            "drop",
        ),
    ] {
        let key = format!("acl/vms/{name}");
        acls.push(key.clone());
        b.add(
            Table::Acl,
            key.clone(),
            None,
            vec![
                ("direction", s(direction)),
                ("priority", Datum::Int(prio)),
                ("match", s(matcher)),
                ("action", s(action)),
            ],
        );
    }
    b.add(
        Table::PortGroup,
        "pg/vms".into(),
        None,
        vec![
            ("name", s(pg_vms)),
            (
                "ports",
                refs(vm_ports.iter().map(|p| format!("lsp/{}", p.id))),
            ),
            ("acls", refs(acls)),
        ],
    );

    // Security groups. A group's port group exists even when empty, since
    // other groups' rules may refer to its members' addresses.
    let groups: BTreeSet<Uuid> = m.security_groups.iter().map(|g| g.id).collect();
    for g in &m.security_groups {
        let pg = port_group_name("sg", g.id);
        let mut acls = vec![];
        for (rule_id, rule) in &g.rules {
            if let Remote::Group(r) = rule.remote
                && !groups.contains(&r)
            {
                warnings.push(format!(
                    "rule {rule_id} refers to an unknown security group"
                ));
                continue;
            }
            let key = format!("acl/sg/{}/{rule_id}", g.id);
            b.add(
                Table::Acl,
                key.clone(),
                Some(g.tenant_id),
                vec![
                    ("direction", s(acl_direction(rule.direction))),
                    ("priority", Datum::Int(PRIO_ALLOW)),
                    ("match", s(acl_match(&pg, rule))),
                    ("action", s("allow-related")),
                ],
            );
            acls.push(key);
        }
        let members = vm_ports
            .iter()
            .filter(|p| p.security_groups.contains(&g.id));
        b.add(
            Table::PortGroup,
            format!("pg/sg/{}", g.id),
            Some(g.tenant_id),
            vec![
                ("name", s(pg)),
                ("ports", refs(members.map(|p| format!("lsp/{}", p.id)))),
                ("acls", refs(acls)),
            ],
        );
    }

    // Guardrails, per tenant that sets any: they cover the tenant's subtree.
    let mut policies: BTreeMap<Uuid, Vec<&Policy>> = BTreeMap::new();
    for p in &m.policies {
        policies.entry(p.tenant_id).or_default().push(p);
    }
    for (tenant, list) in policies {
        let pg = port_group_name("gr", tenant);
        let mut acls = vec![];
        for p in list {
            let key = format!("acl/gr/{tenant}/{}", p.id);
            b.add(
                Table::Acl,
                key.clone(),
                Some(tenant),
                vec![
                    ("direction", s(acl_direction(p.rule.direction))),
                    ("priority", Datum::Int(PRIO_GUARDRAIL)),
                    ("match", s(acl_match(&pg, &p.rule))),
                    ("action", s("drop")),
                ],
            );
            acls.push(key);
        }
        let covered = vm_ports
            .iter()
            .filter(|p| p.tenant_lineage.contains(&tenant));
        b.add(
            Table::PortGroup,
            format!("pg/gr/{tenant}"),
            Some(tenant),
            vec![
                ("name", s(pg)),
                ("ports", refs(covered.map(|p| format!("lsp/{}", p.id)))),
                ("acls", refs(acls)),
            ],
        );
    }

    Desired {
        rows: b.rows,
        warnings,
    }
}

/// The first usable address of a network, the conventional gateway.
pub fn first_host(c: Cidr) -> Ipv4Addr {
    let base = u32::from(c.addr) & u32::MAX.checked_shl(32 - u32::from(c.len)).unwrap_or(0);
    Ipv4Addr::from(base + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn cidr(s: &str) -> Cidr {
        Cidr::parse(s).unwrap()
    }

    /// Provider (1) > acme (2) > dev (3). Two VMs in dev, one VPC with a
    /// gateway, one floating IP, a security group allowing SSH, and a
    /// provider guardrail denying SSH from 10.0.0.0/8.
    fn model() -> Model {
        let (root, acme, dev) = (id(1), id(2), id(3));
        let (vpc, net, ext, sg) = (id(10), id(11), id(12), id(13));
        let port = |n: u128, ip: [u8; 4]| Port {
            id: id(n),
            vm_id: id(n + 100),
            tenant_id: dev,
            tenant_lineage: vec![root, acme, dev],
            network_id: net,
            mac: format!("02:00:00:00:00:{n:02x}"),
            ip: Ipv4Addr::from(ip),
            security_groups: vec![sg],
            chassis: vec![],
            migrating: false,
            bandwidth: None,
        };
        Model {
            external_networks: vec![ExternalNetwork {
                id: ext,
                physical_network: "external".into(),
                cidr: cidr("10.78.0.0/24"),
                gateway: "10.78.0.1".parse().unwrap(),
            }],
            vpcs: vec![Vpc {
                id: vpc,
                tenant_id: dev,
                gateway: Some(VpcGateway {
                    external_network_id: ext,
                    ip: "10.78.0.200".parse().unwrap(),
                }),
            }],
            networks: vec![Network {
                id: net,
                tenant_id: dev,
                vpc_id: vpc,
                cidr: cidr("10.0.0.0/24"),
                gateway: "10.0.0.1".parse().unwrap(),
                mtu: 1442,
                monitor_ip: None,
            }],
            ports: vec![port(20, [10, 0, 0, 2]), port(21, [10, 0, 0, 3])],
            security_groups: vec![SecurityGroup {
                id: sg,
                tenant_id: dev,
                rules: vec![(
                    id(30),
                    Match {
                        direction: Direction::Ingress,
                        protocol: Protocol::Tcp,
                        ports: Some((22, 22)),
                        remote: Remote::Cidr(cidr("0.0.0.0/0")),
                    },
                )],
            }],
            policies: vec![Policy {
                id: id(40),
                tenant_id: root,
                rule: Match {
                    direction: Direction::Ingress,
                    protocol: Protocol::Tcp,
                    ports: Some((22, 22)),
                    remote: Remote::Cidr(cidr("10.0.0.0/8")),
                },
            }],
            floating_ips: vec![FloatingIp {
                id: id(50),
                external_network_id: ext,
                ip: "10.78.0.201".parse().unwrap(),
                port: Some(id(20)),
            }],
            load_balancers: vec![],
            chassis: ["mst-host1", "mst-host2", "mst-host3", "mst-host4"]
                .map(|n| Chassis {
                    name: n.into(),
                    physical_networks: vec!["external".into()],
                })
                .into(),
            dns_servers: vec![],
        }
    }

    #[test]
    fn dhcp_hands_out_the_name_servers_given() {
        let dns = |m: &Model| {
            let row = compile(m)
                .find(&format!("dhcp/{}", m.networks[0].id))
                .cloned()
                .unwrap();
            match row.get("options") {
                Some(Datum::Map(o)) => o.get("dns_server").cloned(),
                other => panic!("{other:?}"),
            }
        };
        let mut m = model();
        assert_eq!(dns(&m), None);
        m.dns_servers = vec!["192.0.2.53".parse().unwrap()];
        assert_eq!(dns(&m).as_deref(), Some("192.0.2.53"));
        m.dns_servers.push("198.51.100.53".parse().unwrap());
        assert_eq!(dns(&m).as_deref(), Some("{192.0.2.53, 198.51.100.53}"));
    }

    #[test]
    fn load_balancers_spread_over_members_and_check_them() {
        let mut m = model();
        m.networks[0].monitor_ip = Some("10.0.0.254".parse().unwrap());
        let (lb, vpc, net) = (id(60), m.vpcs[0].id, m.networks[0].id);
        let (a, b) = (m.ports[0].id, m.ports[1].id);
        m.load_balancers.push(LoadBalancer {
            id: lb,
            tenant_id: m.vpcs[0].tenant_id,
            vpc_id: vpc,
            vip: "10.78.0.210".parse().unwrap(),
            listeners: vec![
                Listener {
                    protocol: Protocol::Tcp,
                    port: 80,
                    target_port: 8080,
                },
                Listener {
                    protocol: Protocol::Tcp,
                    port: 443,
                    target_port: 443,
                },
            ],
            members: vec![a, b, id(99)],
            health_check: true,
        });
        let d = compile(&m);
        let row = d.find(&format!("lb/{lb}/tcp")).expect("TCP load balancer");
        assert!(
            d.find(&format!("lb/{lb}/udp")).is_none(),
            "no UDP listeners"
        );
        assert_eq!(row.get("protocol"), Some(&Datum::Strs(vec!["tcp".into()])));
        let vips = BTreeMap::from([
            (
                "10.78.0.210:80".to_owned(),
                "10.0.0.2:8080,10.0.0.3:8080".to_owned(),
            ),
            (
                "10.78.0.210:443".to_owned(),
                "10.0.0.2:443,10.0.0.3:443".to_owned(),
            ),
        ]);
        assert_eq!(row.get("vips"), Some(&Datum::Map(vips)));
        assert_eq!(
            row.get("ip_port_mappings"),
            Some(&Datum::Map(BTreeMap::from([
                ("10.0.0.2".to_owned(), format!("{a}:10.0.0.254")),
                ("10.0.0.3".to_owned(), format!("{b}:10.0.0.254")),
            ])))
        );
        let checks = row.refs("health_check");
        assert_eq!(checks.len(), 2);
        assert_eq!(
            d.find(checks[1]).unwrap().str("vip"),
            Some("10.78.0.210:443")
        );
        // On the router and every switch of the VPC.
        let key = format!("lb/{lb}/tcp");
        assert!(
            d.find(&format!("lr/{vpc}"))
                .unwrap()
                .refs("load_balancer")
                .contains(&key.as_str())
        );
        assert!(
            d.find(&format!("ls/{net}"))
                .unwrap()
                .refs("load_balancer")
                .contains(&key.as_str())
        );
        // Health checks need no rule: OVN lets its own through, and one
        // by their source address (a tenant's choosing) would let anyone in.
        assert!(!d.rows.iter().any(|r| r.key.contains("health")));
        assert!(
            d.warnings
                .iter()
                .any(|w| w.contains("not a NIC in its VPC"))
        );

        // Without health checks: no checks, no mappings, no ACL.
        m.load_balancers[0].health_check = false;
        let d = compile(&m);
        let row = d.find(&format!("lb/{lb}/tcp")).unwrap();
        assert!(row.refs("health_check").is_empty());
        assert_eq!(
            row.get("ip_port_mappings"),
            Some(&Datum::Map(BTreeMap::new()))
        );
    }

    #[test]
    fn rate_limits_meter_each_limited_port_both_ways() {
        let mut m = model();
        m.ports[0].bandwidth = Some(Bandwidth { kbps: 50_000 });
        let d = compile(&m);
        let (port, net) = (m.ports[0].id, m.ports[0].network_id);
        let out = d.find(&format!("qos/{port}/out")).expect("egress rule");
        assert_eq!(out.str("direction"), Some("from-lport"));
        assert_eq!(
            out.str("match"),
            Some(format!("inport == \"{port}\"").as_str())
        );
        assert_eq!(
            out.get("bandwidth"),
            Some(&Datum::IntMap(BTreeMap::from([
                ("rate".to_owned(), 50_000),
                ("burst".to_owned(), 12_500),
            ])))
        );
        let inbound = d.find(&format!("qos/{port}/in")).expect("ingress rule");
        assert_eq!(inbound.str("direction"), Some("to-lport"));
        // Hung off the port's switch; the unlimited port gets none.
        let switch = d.find(&format!("ls/{net}")).unwrap();
        assert_eq!(switch.refs("qos_rules").len(), 2);
        assert!(d.find(&format!("qos/{}/out", m.ports[1].id)).is_none());
    }

    #[test]
    fn every_reference_resolves_and_every_key_is_unique() {
        let d = compile(&model());
        assert!(d.warnings.is_empty(), "{:?}", d.warnings);
        let keys: BTreeSet<&str> = d.rows.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys.len(), d.rows.len(), "duplicate keys");
        for r in &d.rows {
            for c in r.table.columns() {
                if let Some(Datum::Refs(v)) = r.get(c) {
                    for k in v {
                        assert!(keys.contains(k.as_str()), "{} refers to missing {k}", r.key);
                    }
                }
            }
        }
        // Non-root rows exist only while referenced; each must be.
        let referenced: BTreeSet<&str> = d
            .rows
            .iter()
            .flat_map(|r| r.cols.keys().flat_map(|c| r.refs(c)))
            .collect();
        for r in d.rows.iter().filter(|r| !r.table.is_root()) {
            assert!(
                referenced.contains(r.key.as_str()),
                "{} is never referenced",
                r.key
            );
        }
    }

    #[test]
    fn vm_ports_are_pinned_to_their_address() {
        let d = compile(&model());
        let p = d.find(&format!("lsp/{}", id(20))).unwrap();
        assert_eq!(p.str("name"), Some(id(20).to_string().as_str()));
        assert_eq!(
            p.get("port_security"),
            Some(&Datum::Strs(vec!["02:00:00:00:00:14 10.0.0.2".into()]))
        );
        assert_eq!(p.refs("dhcpv4_options"), [format!("dhcp/{}", id(11))]);
    }

    #[test]
    fn guardrails_outrank_every_allow() {
        let d = compile(&model());
        let gr = d.find(&format!("acl/gr/{}/{}", id(1), id(40))).unwrap();
        let allow = d.find(&format!("acl/sg/{}/{}", id(13), id(30))).unwrap();
        let (Some(Datum::Int(g)), Some(Datum::Int(a))) =
            (gr.get("priority"), allow.get("priority"))
        else {
            panic!()
        };
        assert!(g > a);
        assert_eq!(gr.str("action"), Some("drop"));
        assert_eq!(
            gr.str("match"),
            Some(
                format!(
                    "outport == @mst_gr_{} && ip4 && ip4.src == 10.0.0.0/8 && tcp && tcp.dst == 22",
                    id(1).simple()
                )
                .as_str()
            )
        );
        // The provider's guardrail covers its grandchildren's VMs.
        let pg = d.find(&format!("pg/gr/{}", id(1))).unwrap();
        assert_eq!(pg.refs("ports").len(), 2);

        // DHCP outranks guardrails too, but reaches only OVN's DHCP server:
        // no way out past a guardrail on UDP 68 to 67.
        let net = id(11);
        let dhcp = d.find(&format!("acl/dhcp/{net}")).unwrap();
        let Some(Datum::Int(p)) = dhcp.get("priority") else {
            panic!()
        };
        assert!(p > g);
        let gw = &model().networks[0].gateway;
        assert_eq!(
            dhcp.str("match"),
            Some(
                format!(
                    "inport == @mst_vms && ip4 && udp && udp.src == 68 && udp.dst == 67 \
                     && ip4.dst == {{255.255.255.255, {gw}}}"
                )
                .as_str()
            )
        );
        let ls = d.find(&format!("ls/{net}")).unwrap();
        assert_eq!(ls.refs("acls"), [format!("acl/dhcp/{net}").as_str()]);
        assert!(d.find("acl/vms/dhcp").is_none());
    }

    #[test]
    fn security_group_rules_become_stateful_allows() {
        let d = compile(&model());
        let allow = d.find(&format!("acl/sg/{}/{}", id(13), id(30))).unwrap();
        assert_eq!(allow.str("action"), Some("allow-related"));
        assert_eq!(allow.str("direction"), Some("to-lport"));
        assert_eq!(
            allow.str("match"),
            Some(
                format!(
                    "outport == @mst_sg_{} && ip4 && tcp && tcp.dst == 22",
                    id(13).simple()
                )
                .as_str()
            )
        );
    }

    #[test]
    fn matches_cover_groups_ranges_and_icmp() {
        let m = |direction, protocol, ports, remote| Match {
            direction,
            protocol,
            ports,
            remote,
        };
        assert_eq!(
            acl_match(
                "pg",
                &m(
                    Direction::Egress,
                    Protocol::Udp,
                    Some((53, 54)),
                    Remote::Group(id(7))
                )
            ),
            format!(
                "inport == @pg && ip4 && ip4.dst == $mst_sg_{}_ip4 && udp && udp.dst >= 53 && udp.dst <= 54",
                id(7).simple()
            )
        );
        assert_eq!(
            acl_match(
                "pg",
                &m(
                    Direction::Ingress,
                    Protocol::Icmp,
                    None,
                    Remote::Cidr(cidr("192.0.2.7/32"))
                )
            ),
            "outport == @pg && ip4 && ip4.src == 192.0.2.7 && icmp4"
        );
        assert_eq!(
            acl_match(
                "pg",
                &m(
                    Direction::Egress,
                    Protocol::Any,
                    None,
                    Remote::Cidr(cidr("0.0.0.0/0"))
                )
            ),
            "inport == @pg && ip4"
        );
    }

    #[test]
    fn gateways_get_snat_a_route_and_spread_chassis() {
        let d = compile(&model());
        let lr = d.find(&format!("lr/{}", id(10))).unwrap();
        assert_eq!(
            lr.refs("nat").len(),
            2,
            "SNAT for the network and the floating IP"
        );
        let gw = d.find(&format!("lrp/gw/{}", id(10))).unwrap();
        assert_eq!(
            gw.get("networks"),
            Some(&Datum::Strs(vec!["10.78.0.200/24".into()]))
        );
        assert_eq!(gw.refs("gateway_chassis").len(), 3);
        let fip = d.find(&format!("nat/fip/{}", id(50))).unwrap();
        assert_eq!(fip.str("logical_ip"), Some("10.0.0.2"));
        // A different VPC starts at a different host.
        let first = |vpc: Uuid| {
            let mut m = model();
            m.vpcs[0].id = vpc;
            m.networks[0].vpc_id = vpc;
            let d = compile(&m);
            d.rows
                .iter()
                .find(|r| {
                    r.table == Table::GatewayChassis && r.get("priority") == Some(&Datum::Int(3))
                })
                .and_then(|r| r.str("chassis_name").map(str::to_owned))
                .unwrap()
        };
        assert_ne!(first(id(100)), first(id(101)));
    }

    #[test]
    fn unworkable_configuration_is_reported_not_built() {
        let mut m = model();
        m.chassis.clear();
        m.vpcs[0].gateway = None;
        let d = compile(&m);
        assert!(d.find(&format!("nat/fip/{}", id(50))).is_none());
        assert_eq!(d.warnings.len(), 1, "{:?}", d.warnings);
    }

    #[test]
    fn ports_are_bound_to_their_host_and_both_while_migrating() {
        let mut m = model();
        m.ports[0].chassis = vec!["mst-host1".into()];
        let opts = |m: &Model| match compile(m)
            .find(&format!("lsp/{}", id(20)))
            .unwrap()
            .get("options")
        {
            Some(Datum::Map(o)) => o.clone(),
            _ => panic!(),
        };
        assert_eq!(
            opts(&m).get("requested-chassis").map(String::as_str),
            Some("mst-host1")
        );
        assert!(!opts(&m).contains_key("activation-strategy"));
        m.ports[0].chassis.push("mst-host2".into());
        m.ports[0].migrating = true;
        assert_eq!(
            opts(&m).get("requested-chassis").map(String::as_str),
            Some("mst-host1,mst-host2")
        );
        assert_eq!(
            opts(&m).get("activation-strategy").map(String::as_str),
            Some("rarp")
        );
    }

    #[test]
    fn gateways_are_the_first_host() {
        assert_eq!(first_host(cidr("10.1.2.0/24")).to_string(), "10.1.2.1");
        assert_eq!(first_host(cidr("10.1.2.77/16")).to_string(), "10.1.0.1");
    }
}
