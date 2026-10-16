//! The network model the reconciler compiles into OVN, as one snapshot.

use std::collections::HashMap;

use mst_core::tenant::TenantPath;
use mst_netplan as np;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::Result;

fn cidr(s: &str) -> np::Cidr {
    // Postgres produced it; a bad one means corruption, and /32 is harmless.
    np::Cidr::parse(s).unwrap_or(np::Cidr {
        addr: std::net::Ipv4Addr::UNSPECIFIED,
        len: 32,
    })
}

fn ip(s: &str) -> std::net::Ipv4Addr {
    s.parse().unwrap_or(std::net::Ipv4Addr::UNSPECIFIED)
}

fn direction(s: &str) -> np::Direction {
    if s == "ingress" {
        np::Direction::Ingress
    } else {
        np::Direction::Egress
    }
}

fn protocol(s: &str) -> np::Protocol {
    match s {
        "tcp" => np::Protocol::Tcp,
        "udp" => np::Protocol::Udp,
        "icmp" => np::Protocol::Icmp,
        _ => np::Protocol::Any,
    }
}

fn ports(min: Option<i32>, max: Option<i32>) -> Option<(u16, u16)> {
    Some((u16::try_from(min?).ok()?, u16::try_from(max?).ok()?))
}

/// Reads the whole model. Run it in a repeatable-read transaction so the
/// snapshot is consistent.
pub async fn load(db: &mut PgConnection, mtu: u32) -> Result<np::Model> {
    let external_networks = sqlx::query_as::<_, (Uuid, String, String, String)>(
        "SELECT id, physical_network, cidr::text, host(gateway) FROM external_networks",
    )
    .fetch_all(&mut *db)
    .await?
    .into_iter()
    .map(|(id, physical_network, c, g)| np::ExternalNetwork {
        id,
        physical_network,
        cidr: cidr(&c),
        gateway: ip(&g),
    })
    .collect();

    let vpcs = sqlx::query_as::<_, (Uuid, Uuid, Option<Uuid>, Option<String>)>(
        "SELECT v.id, v.tenant_id, g.external_network_id, host(g.ip)
         FROM vpcs v LEFT JOIN external_ips g ON g.vpc_id = v.id",
    )
    .fetch_all(&mut *db)
    .await?
    .into_iter()
    .map(|(id, tenant_id, ext, gw)| np::Vpc {
        id,
        tenant_id,
        gateway: ext.zip(gw).map(|(external_network_id, g)| np::VpcGateway {
            external_network_id,
            ip: ip(&g),
        }),
    })
    .collect();

    let networks = sqlx::query_as::<_, (Uuid, Uuid, Uuid, String, String)>(
        "SELECT id, tenant_id, vpc_id, cidr::text, host(gateway) FROM networks",
    )
    .fetch_all(&mut *db)
    .await?
    .into_iter()
    .map(|(id, tenant_id, vpc_id, c, g)| np::Network {
        id,
        tenant_id,
        vpc_id,
        cidr: cidr(&c),
        gateway: ip(&g),
        mtu,
    })
    .collect();

    type PortRow = (
        Uuid,
        Uuid,
        Uuid,
        String,
        Uuid,
        String,
        String,
        Vec<Uuid>,
        Vec<String>,
        bool,
        Option<i32>,
    );
    let nic_ports = sqlx::query_as::<_, PortRow>(
        "SELECT n.id, n.vm_id, v.tenant_id, t.path::text, n.network_id, n.mac, host(n.ip),
                coalesce(array_agg(g.security_group_id) FILTER (WHERE g.security_group_id IS NOT NULL), '{}'),
                -- The VM's host, and while migrating its destination.
                array_remove(ARRAY[nullif(h.ovn_chassis, ''), nullif(d.ovn_chassis, '')], NULL),
                m.vm_id IS NOT NULL,
                -- The limit of the VM's cluster.
                c.vm_net_mbps
         FROM nics n JOIN vms v ON v.id = n.vm_id JOIN tenants t ON t.id = v.tenant_id
         LEFT JOIN hosts h ON h.id = v.host_id
         LEFT JOIN clusters c ON c.id = h.cluster_id
         LEFT JOIN migrations m ON m.vm_id = v.id AND m.state IN ('running', 'committing')
         LEFT JOIN hosts d ON d.id = m.dest_host_id
         LEFT JOIN nic_security_groups g ON g.nic_id = n.id
         GROUP BY n.id, v.tenant_id, t.path, h.ovn_chassis, d.ovn_chassis, m.vm_id, c.vm_net_mbps",
    )
    .fetch_all(&mut *db)
    .await?
    .into_iter()
    .map(
        |(id, vm_id, tenant_id, path, network_id, mac, addr, security_groups, chassis, migrating, mbps)| np::Port {
            id,
            vm_id,
            tenant_id,
            tenant_lineage: TenantPath::parse(&path)
                .map(|p| p.lineage().iter().map(|t| t.0).collect())
                .unwrap_or_else(|| vec![tenant_id]),
            network_id,
            mac,
            ip: ip(&addr),
            security_groups,
            chassis,
            migrating,
            bandwidth: mbps.map(|m| np::Bandwidth {
                kbps: (m as u32).saturating_mul(1000),
            }),
        },
    )
    .collect();

    let mut groups: Vec<np::SecurityGroup> =
        sqlx::query_as::<_, (Uuid, Uuid)>("SELECT id, tenant_id FROM security_groups")
            .fetch_all(&mut *db)
            .await?
            .into_iter()
            .map(|(id, tenant_id)| np::SecurityGroup {
                id,
                tenant_id,
                rules: vec![],
            })
            .collect();
    let index: HashMap<Uuid, usize> = groups.iter().enumerate().map(|(i, g)| (g.id, i)).collect();
    type RuleRow = (
        Uuid,
        Uuid,
        String,
        String,
        Option<i32>,
        Option<i32>,
        Option<String>,
        Option<Uuid>,
    );
    for (id, group, dir, proto, min, max, remote_cidr, remote_group) in sqlx::query_as::<_, RuleRow>(
        "SELECT id, security_group_id, direction, protocol, port_min, port_max, remote_cidr::text, remote_group_id
         FROM security_group_rules ORDER BY created_at, id",
    )
    .fetch_all(&mut *db)
    .await?
    {
        let remote = match (remote_cidr, remote_group) {
            (Some(c), _) => np::Remote::Cidr(cidr(&c)),
            (None, Some(g)) => np::Remote::Group(g),
            (None, None) => continue,
        };
        if let Some(&i) = index.get(&group) {
            groups[i].rules.push((
                id,
                np::Match { direction: direction(&dir), protocol: protocol(&proto), ports: ports(min, max), remote },
            ));
        }
    }

    let policies = sqlx::query_as::<_, (Uuid, Uuid, String, String, Option<i32>, Option<i32>, String)>(
        "SELECT id, tenant_id, direction, protocol, port_min, port_max, remote_cidr::text FROM network_policies",
    )
    .fetch_all(&mut *db)
    .await?
    .into_iter()
    .map(|(id, tenant_id, dir, proto, min, max, remote)| np::Policy {
        id,
        tenant_id,
        rule: np::Match {
            direction: direction(&dir),
            protocol: protocol(&proto),
            ports: ports(min, max),
            remote: np::Remote::Cidr(cidr(&remote)),
        },
    })
    .collect();

    let floating_ips = sqlx::query_as::<_, (Uuid, Uuid, String, Option<Uuid>)>(
        "SELECT id, external_network_id, host(ip), nic_id FROM external_ips WHERE kind = 'floating'",
    )
    .fetch_all(&mut *db)
    .await?
    .into_iter()
    .map(|(id, external_network_id, addr, port)| np::FloatingIp { id, external_network_id, ip: ip(&addr), port })
    .collect();

    let chassis = sqlx::query_as::<_, (String, Vec<String>)>(
        "SELECT ovn_chassis, physical_networks FROM hosts WHERE ovn_chassis <> '' AND state <> 'decommissioned'",
    )
    .fetch_all(&mut *db)
    .await?
    .into_iter()
    .map(|(name, physical_networks)| np::Chassis { name, physical_networks })
    .collect();

    Ok(np::Model {
        external_networks,
        vpcs,
        networks,
        ports: nic_ports,
        security_groups: groups,
        policies,
        floating_ips,
        chassis,
    })
}
