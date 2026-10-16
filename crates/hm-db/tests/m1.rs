//! Repository tests for hosts, tokens, sessions, images, networks and VMs.

use std::time::Duration;

use chrono::Utc;
use hm_core::ids::{HostId, NetworkId, NicId, VmId};
use hm_core::vm::VmSpec;
use hm_db::{console, hosts, images, join_tokens, networks, sessions, tenants, users, vms};
use sqlx::PgPool;

async fn root(pool: &PgPool) -> tenants::Tenant {
    tenants::create(pool, None, "provider", "Provider")
        .await
        .unwrap()
}

async fn user(pool: &PgPool, tenant: &tenants::Tenant, name: &str) -> users::User {
    users::create(
        pool,
        users::NewUser {
            tenant_id: tenant.id,
            username: name,
            display_name: name,
            email: None,
            password_hash: Some("x"),
        },
    )
    .await
    .unwrap()
}

async fn host(pool: &PgPool) -> hosts::Host {
    let facts = hosts::Facts {
        name: "hm-host1",
        address: "10.77.0.11",
        cpu_count: 4,
        memory_mib: 6000,
        cpu_model: "x86_64",
        kvm: true,
        libvirt_version: "11.3.0",
        ovn_chassis: "hm-host1",
        agent_version: "0.1.0",
    };
    hosts::create(pool, HostId::new(), &facts, "fp", Utc::now())
        .await
        .unwrap()
}

async fn vm(pool: &PgPool, tenant: &tenants::Tenant, host: &hosts::Host, name: &str) -> vms::Vm {
    let spec = VmSpec {
        vcpus: 1,
        memory_mib: 512,
        firmware: Default::default(),
        disks: vec![],
        nics: vec![],
        cloud_init: None,
    };
    vms::create(
        pool,
        vms::NewVm {
            id: VmId::new(),
            tenant_id: tenant.id,
            name,
            host_id: host.id,
            spec: &spec,
            desired_power: "running",
            created_by: None,
        },
    )
    .await
    .unwrap()
}

#[sqlx::test(migrator = "hm_db::MIGRATOR")]
async fn ip_allocation_is_ordered_skips_gateway_and_exhausts(pool: PgPool) {
    let t = root(&pool).await;
    let h = host(&pool).await;
    let v = vm(&pool, &t, &h, "a").await;
    let mut conn = pool.acquire().await.unwrap();

    let net = networks::default_for(&mut conn, t.id).await.unwrap();
    assert_eq!(
        (net.cidr.as_str(), net.gateway.as_str()),
        ("10.0.0.0/24", "10.0.0.1")
    );
    assert_eq!(
        networks::default_for(&mut conn, t.id).await.unwrap().id,
        net.id,
        "idempotent"
    );

    let a = networks::create_nic(&mut conn, NicId::new(), v.id, net.id, "02:00:00:00:00:01")
        .await
        .unwrap();
    let b = networks::create_nic(&mut conn, NicId::new(), v.id, net.id, "02:00:00:00:00:02")
        .await
        .unwrap();
    assert_eq!((a.ip.as_str(), b.ip.as_str()), ("10.0.0.2", "10.0.0.3"));

    // A /29 has 6 usable addresses; minus the gateway leaves 5.
    let small = NetworkId::new();
    sqlx::query("INSERT INTO networks (id, tenant_id, name, cidr, gateway) VALUES ($1, $2, 'small', '192.168.5.0/29', '192.168.5.1')")
        .bind(small)
        .bind(t.id)
        .execute(&pool)
        .await
        .unwrap();
    let mut ips = vec![];
    for i in 0..5 {
        let nic = networks::create_nic(
            &mut conn,
            NicId::new(),
            v.id,
            small,
            &format!("02:00:00:00:01:{i:02}"),
        )
        .await
        .unwrap();
        ips.push(nic.ip);
    }
    assert_eq!(
        ips,
        [
            "192.168.5.2",
            "192.168.5.3",
            "192.168.5.4",
            "192.168.5.5",
            "192.168.5.6"
        ]
    );
    let err = networks::create_nic(&mut conn, NicId::new(), v.id, small, "02:00:00:00:01:99")
        .await
        .unwrap_err();
    assert!(
        matches!(err, hm_db::Error::Domain(hm_core::Error::Conflict(_))),
        "{err:?}"
    );
}

#[sqlx::test(migrator = "hm_db::MIGRATOR")]
async fn concurrent_ip_allocation_never_duplicates(pool: PgPool) {
    let t = root(&pool).await;
    let h = host(&pool).await;
    let v = vm(&pool, &t, &h, "a").await;
    let net = networks::default_for(&mut pool.acquire().await.unwrap(), t.id)
        .await
        .unwrap();

    let tasks: Vec<_> = (0..20)
        .map(|i| {
            let pool = pool.clone();
            tokio::spawn(async move {
                let mut conn = pool.acquire().await.unwrap();
                networks::create_nic(
                    &mut conn,
                    NicId::new(),
                    v.id,
                    net.id,
                    &format!("02:00:00:00:02:{i:02}"),
                )
                .await
                .unwrap()
                .ip
            })
        })
        .collect();
    let mut ips = vec![];
    for t in tasks {
        ips.push(t.await.unwrap());
    }
    ips.sort();
    ips.dedup();
    assert_eq!(ips.len(), 20);
}

#[sqlx::test(migrator = "hm_db::MIGRATOR")]
async fn join_tokens_are_single_use_and_expire(pool: PgPool) {
    root(&pool).await;
    join_tokens::create(
        &pool,
        "t1",
        "hash",
        None,
        Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    join_tokens::create(
        &pool,
        "t2",
        "hash",
        None,
        Utc::now() - chrono::Duration::seconds(1),
    )
    .await
    .unwrap();

    assert!(!join_tokens::consume(&pool, "t1", "wrong").await.unwrap());
    assert!(join_tokens::consume(&pool, "t1", "hash").await.unwrap());
    assert!(
        !join_tokens::consume(&pool, "t1", "hash").await.unwrap(),
        "reuse"
    );
    assert!(
        !join_tokens::consume(&pool, "t2", "hash").await.unwrap(),
        "expired"
    );
}

#[sqlx::test(migrator = "hm_db::MIGRATOR")]
async fn sessions_resolve_until_expiry_or_disable(pool: PgPool) {
    let t = root(&pool).await;
    let u = user(&pool, &t, "alice").await;
    sessions::create(&pool, "s1", u.id, Duration::from_secs(60))
        .await
        .unwrap();
    sessions::create(&pool, "s2", u.id, Duration::from_secs(0))
        .await
        .unwrap();
    assert_eq!(sessions::resolve(&pool, "s1").await.unwrap(), Some(u.id));
    assert_eq!(sessions::resolve(&pool, "s2").await.unwrap(), None);

    sqlx::query("UPDATE users SET disabled = true WHERE id = $1")
        .bind(u.id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(sessions::resolve(&pool, "s1").await.unwrap(), None);

    assert_eq!(sessions::purge_expired(&pool).await.unwrap(), 1);
}

#[sqlx::test(migrator = "hm_db::MIGRATOR")]
async fn console_tickets_are_single_use(pool: PgPool) {
    let t = root(&pool).await;
    let u = user(&pool, &t, "alice").await;
    let h = host(&pool).await;
    let v = vm(&pool, &t, &h, "a").await;
    console::create_ticket(&pool, "tk", u.id, v.id, "vnc", Duration::from_secs(30))
        .await
        .unwrap();
    assert_eq!(
        console::consume_ticket(&pool, "tk").await.unwrap(),
        Some((u.id, v.id, "vnc".into()))
    );
    assert_eq!(console::consume_ticket(&pool, "tk").await.unwrap(), None);
}

#[sqlx::test(migrator = "hm_db::MIGRATOR")]
async fn images_visible_to_owner_and_shared_down_the_tree(pool: PgPool) {
    let r = root(&pool).await;
    let a = tenants::create(&pool, Some(r.id), "acme", "Acme")
        .await
        .unwrap();
    let a1 = tenants::create(&pool, Some(a.id), "dev", "Dev")
        .await
        .unwrap();
    let b = tenants::create(&pool, Some(r.id), "globex", "Globex")
        .await
        .unwrap();
    let img = |tenant_id, name, shared| images::NewImage {
        tenant_id,
        name,
        source_url: "http://x",
        format: "qcow2",
        shared,
    };

    let public = images::create(&pool, img(r.id, "debian", true))
        .await
        .unwrap();
    let private_root = images::create(&pool, img(r.id, "internal", false))
        .await
        .unwrap();
    let acme = images::create(&pool, img(a.id, "acme-shared", true))
        .await
        .unwrap();

    let names = |v: Vec<images::Image>| v.into_iter().map(|i| i.name).collect::<Vec<_>>();
    assert_eq!(
        names(images::list_usable(&pool, a1.id).await.unwrap()),
        ["acme-shared", "debian"]
    );
    assert_eq!(
        names(images::list_usable(&pool, b.id).await.unwrap()),
        ["debian"]
    );
    assert_eq!(
        names(images::list_usable(&pool, r.id).await.unwrap()),
        ["debian", "internal"]
    );

    assert!(images::is_usable_by(&pool, acme.id, a1.id).await.unwrap());
    assert!(!images::is_usable_by(&pool, acme.id, b.id).await.unwrap());
    assert!(
        !images::is_usable_by(&pool, private_root.id, a.id)
            .await
            .unwrap()
    );
    assert!(images::is_usable_by(&pool, public.id, b.id).await.unwrap());
}

#[sqlx::test(migrator = "hm_db::MIGRATOR")]
async fn vm_listing_by_subtree_and_power_observation(pool: PgPool) {
    let r = root(&pool).await;
    let a = tenants::create(&pool, Some(r.id), "acme", "Acme")
        .await
        .unwrap();
    let h = host(&pool).await;
    let v1 = vm(&pool, &r, &h, "root-vm").await;
    let v2 = vm(&pool, &a, &h, "acme-vm").await;

    assert_eq!(vms::list(&pool, r.id, false).await.unwrap().len(), 1);
    assert_eq!(vms::list(&pool, r.id, true).await.unwrap().len(), 2);
    assert_eq!(vms::list(&pool, a.id, true).await.unwrap()[0].id, v2.id);

    assert!(
        vms::observe_power(&pool, v1.id, h.id, "running")
            .await
            .unwrap()
    );
    assert!(
        !vms::observe_power(&pool, v1.id, h.id, "running")
            .await
            .unwrap(),
        "no change"
    );
    assert!(
        !vms::observe_power(&pool, v1.id, HostId::new(), "stopped")
            .await
            .unwrap(),
        "wrong host"
    );
    assert_eq!(vms::get(&pool, v1.id).await.unwrap().power, "running");
}
