//! Repository tests for the network model: VPCs, networks, external
//! addresses, security groups, policies, and the snapshot the reconciler
//! compiles.

use chrono::Utc;
use hm_core::ids::{ExternalNetworkId, HostId, NetworkId, NicId, PolicyId, VmId};
use hm_core::vm::{VmSpec, mac_for};
use hm_db::{external, hosts, netmodel, netsec, networks, quotas, tenants, vms};
use sqlx::PgPool;

async fn tenant(pool: &PgPool) -> (tenants::Tenant, tenants::Tenant) {
    let root = tenants::create(pool, None, "provider", "Provider")
        .await
        .unwrap();
    let acme = tenants::create(pool, Some(root.id), "acme", "Acme")
        .await
        .unwrap();
    (root, acme)
}

async fn ext(pool: &PgPool) -> external::ExternalNetwork {
    external::create(
        pool,
        ExternalNetworkId::new(),
        &external::ExternalSpec {
            name: "internet",
            physical_network: "external",
            cidr: "203.0.113.0/24",
            gateway: "203.0.113.1",
            pool_start: "203.0.113.1",
            pool_end: "203.0.113.4",
        },
    )
    .await
    .unwrap()
}

#[sqlx::test(migrator = "hm_db::MIGRATOR")]
async fn default_networks_and_overlap_rules(pool: PgPool) {
    let (_, acme) = tenant(&pool).await;
    let mut conn = pool.acquire().await.unwrap();
    let net = networks::default_for(&mut conn, acme.id).await.unwrap();
    assert_eq!(
        (net.cidr.as_str(), net.gateway.as_str()),
        ("10.0.0.0/24", "10.0.0.1")
    );
    // Idempotent.
    assert_eq!(
        networks::default_for(&mut conn, acme.id).await.unwrap().id,
        net.id
    );
    let vpc = networks::get_vpc(&pool, net.vpc_id).await.unwrap();
    assert_eq!(vpc.name, "default");

    // Within a VPC ranges may not overlap; across VPCs they may.
    let err = networks::create(
        &pool,
        NetworkId::new(),
        &vpc,
        "b",
        "10.0.0.128/25",
        "10.0.0.129",
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("overlaps"), "{err}");
    let other = networks::create_vpc(&pool, hm_core::ids::VpcId::new(), acme.id, "other")
        .await
        .unwrap();
    networks::create(
        &pool,
        NetworkId::new(),
        &other,
        "same",
        "10.0.0.0/24",
        "10.0.0.1",
    )
    .await
    .unwrap();
    // Host bits are cleared; the gateway must be inside.
    let n = networks::create(
        &pool,
        NetworkId::new(),
        &other,
        "c",
        "10.9.9.9/24",
        "10.9.9.1",
    )
    .await
    .unwrap();
    assert_eq!(n.cidr, "10.9.9.0/24");
    assert!(
        networks::create(
            &pool,
            NetworkId::new(),
            &other,
            "d",
            "10.8.0.0/24",
            "10.7.0.1"
        )
        .await
        .is_err()
    );

    // A VPC with networks cannot go.
    assert!(networks::delete_vpc(&pool, other.id).await.is_err());
}

#[sqlx::test(migrator = "hm_db::MIGRATOR")]
async fn external_addresses_come_from_the_pool_and_skip_the_gateway(pool: PgPool) {
    let (_, acme) = tenant(&pool).await;
    let e = ext(&pool).await;
    let mut conn = pool.acquire().await.unwrap();
    let mut got = vec![];
    for _ in 0..3 {
        got.push(
            external::allocate(&mut conn, e.id, acme.id, None)
                .await
                .unwrap()
                .ip,
        );
    }
    assert_eq!(got, ["203.0.113.2", "203.0.113.3", "203.0.113.4"]);
    let err = external::allocate(&mut conn, e.id, acme.id, None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no free"), "{err}");
    // Released addresses are handed out again.
    let all = external::floating_ips(&pool, acme.id, false).await.unwrap();
    external::release(&pool, all[0].id).await.unwrap();
    assert_eq!(
        external::allocate(&mut conn, e.id, acme.id, None)
            .await
            .unwrap()
            .ip,
        "203.0.113.2"
    );
    // Floating IPs count against quota.
    assert_eq!(
        quotas::usage(&pool, acme.id).await.unwrap()["floating_ips"],
        3
    );
    // In use, the network stays.
    assert!(external::delete(&pool, e.id).await.is_err());
}

#[sqlx::test(migrator = "hm_db::MIGRATOR")]
async fn the_model_snapshot_has_everything_the_compiler_needs(pool: PgPool) {
    let (root, acme) = tenant(&pool).await;
    let e = ext(&pool).await;
    let host = hosts::create(
        &pool,
        HostId::new(),
        &hosts::Facts {
            name: "h1",
            address: "10.77.0.11",
            cpu_count: 8,
            memory_mib: 8192,
            cpu_model: "",
            kvm: true,
            libvirt_version: "",
            ovn_chassis: "hm-host1",
            agent_version: "",
            physical_networks: &["external".to_owned()],
            cpu_json: "",
            migration_ready: false,
        },
        "fp",
        Utc::now(),
        "01",
    )
    .await
    .unwrap();

    let mut tx = pool.begin().await.unwrap();
    let net = networks::default_for(&mut tx, acme.id).await.unwrap();
    networks::set_vpc_external(&mut *tx, net.vpc_id, Some(e.id))
        .await
        .unwrap();
    external::allocate(&mut tx, e.id, acme.id, Some(net.vpc_id))
        .await
        .unwrap();
    let group = netsec::default_group(&mut tx, acme.id).await.unwrap();
    // Idempotent, and its rules come once.
    assert_eq!(
        netsec::default_group(&mut tx, acme.id).await.unwrap().id,
        group.id
    );
    let vm = VmId::new();
    vms::create(
        &mut *tx,
        vms::NewVm {
            id: vm,
            tenant_id: acme.id,
            name: "web",
            host_id: host.id,
            spec: &VmSpec {
                cpu: None,
                migratable: true,
                vcpus: 1,
                memory_mib: 512,
                firmware: Default::default(),
                disks: vec![],
                nics: vec![],
                cloud_init: None,
            },
            desired_power: "running",
            created_by: None,
        },
    )
    .await
    .unwrap();
    let nic_id = NicId::new();
    let nic = networks::create_nic(&mut tx, nic_id, vm, net.id, &mac_for(nic_id))
        .await
        .unwrap();
    networks::set_nic_groups(&mut tx, nic_id, &[group.id])
        .await
        .unwrap();
    let fip = external::allocate(&mut tx, e.id, acme.id, None)
        .await
        .unwrap();
    external::associate(&mut *tx, fip.id, Some(nic_id))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut conn = pool.acquire().await.unwrap();
    let m = netmodel::load(&mut conn, 1442).await.unwrap();
    assert_eq!(m.vpcs.len(), 1);
    assert_eq!(
        m.vpcs[0].gateway.as_ref().unwrap().ip.to_string(),
        "203.0.113.2"
    );
    let port = &m.ports[0];
    assert_eq!(port.ip.to_string(), nic.ip);
    assert_eq!(port.security_groups, [group.id.0]);
    assert_eq!(port.tenant_lineage, [root.id.0, acme.id.0]);
    let g = m
        .security_groups
        .iter()
        .find(|g| g.id == group.id.0)
        .unwrap();
    assert_eq!(g.rules.len(), 2);
    assert_eq!(m.floating_ips[0].port, Some(nic_id.0));
    assert_eq!(m.chassis[0].physical_networks, ["external"]);
    // And it compiles cleanly.
    let d = hm_netplan::compile(&m);
    assert!(d.warnings.is_empty(), "{:?}", d.warnings);

    // A deleted tenant takes its network configuration with it.
    vms::delete(&pool, vm).await.unwrap();
    tenants::delete(&mut conn, acme.id).await.unwrap();
    let m = netmodel::load(&mut conn, 1442).await.unwrap();
    assert!(m.vpcs.is_empty() && m.networks.is_empty() && m.floating_ips.is_empty());
}

#[sqlx::test(migrator = "hm_db::MIGRATOR")]
async fn policies_apply_down_the_tree(pool: PgPool) {
    let (root, acme) = tenant(&pool).await;
    let admin = hm_db::users::create(
        &pool,
        hm_db::users::NewUser {
            tenant_id: root.id,
            username: "admin",
            display_name: "admin",
            email: None,
            password_hash: Some("x"),
        },
    )
    .await
    .unwrap();
    let spec = |port| netsec::RuleSpec {
        direction: "ingress",
        protocol: "tcp",
        ports: Some((port, port)),
        remote_cidr: Some("10.0.0.0/8"),
        remote_group_id: None,
        description: "",
    };
    netsec::create_policy(&pool, PolicyId::new(), root.id, &spec(22), admin.id)
        .await
        .unwrap();
    netsec::create_policy(&pool, PolicyId::new(), acme.id, &spec(3389), admin.id)
        .await
        .unwrap();
    let mine = netsec::policies_for(&pool, acme.id).await.unwrap();
    // Outermost first.
    assert_eq!(
        mine.iter().map(|p| p.port_min).collect::<Vec<_>>(),
        [Some(22), Some(3389)]
    );
    assert_eq!(netsec::policies_for(&pool, root.id).await.unwrap().len(), 1);
    // Ports need a protocol that has them.
    let bad = netsec::RuleSpec {
        protocol: "icmp",
        ..spec(1)
    };
    assert!(
        netsec::create_policy(&pool, PolicyId::new(), root.id, &bad, admin.id)
            .await
            .is_err()
    );
}
