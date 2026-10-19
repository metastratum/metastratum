//! Repository tests for hosts, tokens, sessions, images, networks and VMs.

use std::time::Duration;

use chrono::Utc;
use mst_core::ids::{HostId, NetworkId, NicId, VmId};
use mst_core::vm::VmSpec;
use mst_db::images::Copy;
use mst_db::{
    console, hosts, images, join_tokens, networks, sessions, tenants, two_factor, users, vms,
};
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
        name: "mst-host1",
        address: "10.77.0.11",
        cpu_count: 4,
        memory_mib: 6000,
        cpu_model: "x86_64",
        kvm: true,
        libvirt_version: "11.3.0",
        ovn_chassis: "mst-host1",
        agent_version: "0.1.0",
        physical_networks: &[],
        cpu_json: "",
        migration_ready: false,
    };
    hosts::create(pool, HostId::new(), &facts, "fp", Utc::now(), "01")
        .await
        .unwrap()
}

async fn vm(pool: &PgPool, tenant: &tenants::Tenant, host: &hosts::Host, name: &str) -> vms::Vm {
    let spec = VmSpec {
        cpu: None,
        migratable: true,
        extra: Default::default(),
        vcpus: 1,
        memory_mib: 512,
        max_memory_mib: None,
        max_vcpus: None,
        free_page_reporting: true,
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

#[sqlx::test(migrator = "mst_db::MIGRATOR")]
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

    // Reserved addresses: a load balancer's (lowest free) and the health
    // check address (highest), which NICs then skip.
    let vip = networks::reserve_address(&mut conn, net.id, "lb-vip")
        .await
        .unwrap();
    let monitor = networks::monitor_address(&mut conn, net.id).await.unwrap();
    assert_eq!(
        networks::monitor_address(&mut conn, net.id).await.unwrap(),
        monitor
    );
    assert_eq!(
        (vip.to_string(), monitor.to_string()),
        ("10.0.0.4".into(), "10.0.0.254".into())
    );
    let c = networks::create_nic(&mut conn, NicId::new(), v.id, net.id, "02:00:00:00:00:03")
        .await
        .unwrap();
    assert_eq!(c.ip, "10.0.0.5");
    networks::release_address(&mut *conn, net.id, vip)
        .await
        .unwrap();
    let d = networks::create_nic(&mut conn, NicId::new(), v.id, net.id, "02:00:00:00:00:04")
        .await
        .unwrap();
    assert_eq!(d.ip, "10.0.0.4", "released, so it is free again");

    // A /29 has 6 usable addresses; minus the gateway leaves 5.
    let small = NetworkId::new();
    let vpc = networks::get_vpc(&pool, net.vpc_id).await.unwrap();
    networks::create(&pool, small, &vpc, "small", "192.168.5.0/29", "192.168.5.1")
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
        matches!(err, mst_db::Error::Domain(mst_core::Error::Conflict(_))),
        "{err:?}"
    );
}

/// Allocations at once, on plain connections and in transactions of their
/// own (as the API makes them), all get an address, and never the same one.
#[sqlx::test(migrator = "mst_db::MIGRATOR")]
async fn concurrent_ip_allocation_never_duplicates(pool: PgPool) {
    let t = root(&pool).await;
    let h = host(&pool).await;
    let v = vm(&pool, &t, &h, "a").await;
    let net = networks::default_for(&mut pool.acquire().await.unwrap(), t.id)
        .await
        .unwrap();

    let tasks: Vec<_> = (0..40)
        .map(|i| {
            let pool = pool.clone();
            tokio::spawn(async move {
                let mac = format!("02:00:00:00:02:{i:02}");
                if i % 2 == 0 {
                    let mut conn = pool.acquire().await.unwrap();
                    networks::create_nic(&mut conn, NicId::new(), v.id, net.id, &mac)
                        .await
                        .unwrap()
                        .ip
                } else {
                    let mut tx = pool.begin().await.unwrap();
                    let ip = networks::create_nic(&mut tx, NicId::new(), v.id, net.id, &mac)
                        .await
                        .unwrap()
                        .ip;
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    tx.commit().await.unwrap();
                    ip
                }
            })
        })
        .collect();
    let mut ips = vec![];
    for t in tasks {
        ips.push(t.await.unwrap());
    }
    ips.sort();
    ips.dedup();
    assert_eq!(ips.len(), 40);
}

#[sqlx::test(migrator = "mst_db::MIGRATOR")]
async fn join_tokens_are_single_use_and_expire(pool: PgPool) {
    root(&pool).await;
    let cluster = mst_db::clusters::create(&pool, "c1", 4.0, 1.0, false)
        .await
        .unwrap();
    let hour = Utc::now() + chrono::Duration::hours(1);
    join_tokens::create(&pool, "t1", "hash", None, hour, Some(cluster.id))
        .await
        .unwrap();
    join_tokens::create(
        &pool,
        "t2",
        "hash",
        None,
        Utc::now() - chrono::Duration::seconds(1),
        None,
    )
    .await
    .unwrap();

    assert_eq!(
        join_tokens::consume(&pool, "t1", "wrong").await.unwrap(),
        None
    );
    assert_eq!(
        join_tokens::consume(&pool, "t1", "hash").await.unwrap(),
        Some(Some(cluster.id))
    );
    assert_eq!(
        join_tokens::consume(&pool, "t1", "hash").await.unwrap(),
        None,
        "reuse"
    );
    assert_eq!(
        join_tokens::consume(&pool, "t2", "hash").await.unwrap(),
        None,
        "expired"
    );
}

#[sqlx::test(migrator = "mst_db::MIGRATOR")]
async fn sessions_resolve_until_expiry_or_disable(pool: PgPool) {
    let t = root(&pool).await;
    let u = user(&pool, &t, "alice").await;
    sessions::create(&pool, "s1", u.id, Duration::from_secs(60))
        .await
        .unwrap();
    sessions::create(&pool, "s2", u.id, Duration::from_secs(0))
        .await
        .unwrap();
    let live = sessions::Resolved {
        user_id: u.id,
        enroll_only: false,
    };
    assert_eq!(sessions::resolve(&pool, "s1").await.unwrap(), Some(live));
    assert_eq!(sessions::resolve(&pool, "s2").await.unwrap(), None);

    sqlx::query("UPDATE users SET disabled = true WHERE id = $1")
        .bind(u.id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(sessions::resolve(&pool, "s1").await.unwrap(), None);

    assert_eq!(sessions::purge_expired(&pool).await.unwrap(), 1);
}

#[sqlx::test(migrator = "mst_db::MIGRATOR")]
async fn two_factor_requirement_backup_codes_and_challenges(pool: PgPool) {
    let t = root(&pool).await;
    let acme = tenants::create(&pool, Some(t.id), "acme", "Acme")
        .await
        .unwrap();
    let dev = tenants::create(&pool, Some(acme.id), "dev", "Dev")
        .await
        .unwrap();
    let u = user(&pool, &dev, "alice").await;
    sessions::create(&pool, "s1", u.id, Duration::from_secs(60))
        .await
        .unwrap();
    let enroll_only = |r: Option<sessions::Resolved>| r.unwrap().enroll_only;

    // Required above: a session without a factor can only add one.
    assert!(!enroll_only(sessions::resolve(&pool, "s1").await.unwrap()));
    two_factor::set_required(&pool, acme.id, true)
        .await
        .unwrap();
    assert!(two_factor::required(&pool, dev.id).await.unwrap());
    assert!(!two_factor::required(&pool, t.id).await.unwrap());
    assert_eq!(
        two_factor::policy(&pool, dev.id).await.unwrap(),
        (false, Some((acme.id, "Acme".to_owned())))
    );
    assert_eq!(
        two_factor::policy(&pool, acme.id).await.unwrap(),
        (true, None)
    );
    assert!(enroll_only(sessions::resolve(&pool, "s1").await.unwrap()));
    let f = two_factor::add(&pool, u.id, "totp", "Phone", &serde_json::json!({}))
        .await
        .unwrap();
    assert!(!enroll_only(sessions::resolve(&pool, "s1").await.unwrap()));
    assert!(two_factor::has_factor(&pool, u.id).await.unwrap());
    assert_eq!(
        two_factor::with_factor(&pool, &[u.id]).await.unwrap().len(),
        1
    );

    // Backup codes work once.
    let mut conn = pool.acquire().await.unwrap();
    two_factor::set_backup_codes(&mut conn, u.id, &["a".into(), "b".into()])
        .await
        .unwrap();
    assert_eq!(two_factor::backup_codes_left(&pool, u.id).await.unwrap(), 2);
    assert!(two_factor::use_backup_code(&pool, u.id, "a").await.unwrap());
    assert!(!two_factor::use_backup_code(&pool, u.id, "a").await.unwrap());
    assert!(!two_factor::use_backup_code(&pool, u.id, "c").await.unwrap());
    assert_eq!(two_factor::backup_codes_left(&pool, u.id).await.unwrap(), 1);

    // A challenge takes so many attempts, then is gone.
    let data = serde_json::json!({"x": 1});
    two_factor::create_challenge(&pool, "c1", u.id, "login", &data, Duration::from_secs(60))
        .await
        .unwrap();
    for n in 1..=3 {
        let c = two_factor::attempt_challenge(&pool, "c1", "login", 3)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((c.attempts, c.user_id), (n, u.id));
    }
    assert!(
        two_factor::attempt_challenge(&pool, "c1", "login", 3)
            .await
            .unwrap()
            .is_none()
    );
    two_factor::create_challenge(&pool, "c2", u.id, "totp", &data, Duration::from_secs(60))
        .await
        .unwrap();
    assert!(
        two_factor::attempt_challenge(&pool, "c2", "login", 3)
            .await
            .unwrap()
            .is_none()
    );
    two_factor::create_challenge(&pool, "c3", u.id, "login", &data, Duration::from_secs(0))
        .await
        .unwrap();
    assert!(
        two_factor::attempt_challenge(&pool, "c3", "login", 3)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        two_factor::purge_expired_challenges(&pool).await.unwrap(),
        0
    );

    // Removing the factor; a reset takes the codes too.
    assert!(two_factor::remove(&pool, u.id, f.id).await.unwrap());
    assert!(!two_factor::remove(&pool, u.id, f.id).await.unwrap());
    assert_eq!(two_factor::reset(&mut conn, u.id).await.unwrap(), 0);
    assert_eq!(two_factor::backup_codes_left(&pool, u.id).await.unwrap(), 0);
}

#[sqlx::test(migrator = "mst_db::MIGRATOR")]
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

#[sqlx::test(migrator = "mst_db::MIGRATOR")]
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
        storage_pool_id: mst_core::ids::LOCAL_POOL,
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

#[sqlx::test(migrator = "mst_db::MIGRATOR")]
async fn images_are_downloaded_then_copied_to_hosts(pool: PgPool) {
    let r = root(&pool).await;
    let a = tenants::create(&pool, Some(r.id), "acme", "Acme")
        .await
        .unwrap();
    let img = images::create(
        &pool,
        images::NewImage {
            tenant_id: a.id,
            name: "debian",
            source_url: "http://x",
            format: "qcow2",
            shared: false,
            storage_pool_id: mst_core::ids::LOCAL_POOL,
        },
    )
    .await
    .unwrap();
    assert_eq!(img.state, "downloading");
    let hex = |id: uuid::Uuid| id.simple().to_string();
    assert_eq!(
        images::store_path(&pool, a.id).await.unwrap(),
        format!("{}/{}", hex(r.id.0), hex(a.id.0))
    );

    images::failed(&pool, img.id, "HTTP 404").await.unwrap();
    let got = images::get(&pool, img.id).await.unwrap();
    assert_eq!(
        (got.state.as_str(), got.error.as_deref()),
        ("failed", Some("HTTP 404"))
    );
    images::downloading(&pool, img.id).await.unwrap();
    assert_eq!(images::get(&pool, img.id).await.unwrap().error, None);

    let h1 = host(&pool).await;
    sqlx::query("UPDATE hosts SET name = 'other', address = '10.77.0.12', ovn_chassis = 'other'")
        .execute(&pool)
        .await
        .unwrap();
    let h2 = host(&pool).await;
    let mut conn = pool.acquire().await.unwrap();
    let stored = |size_bytes: i64| images::Stored {
        size_bytes,
        virtual_bytes: 3 << 30,
        sha256: "abc".into(),
        digest: "def".into(),
    };
    images::set_download_host(&pool, img.id, h1.id)
        .await
        .unwrap();
    images::downloaded(&mut conn, img.id, Copy::local(h1.id), &stored(400 << 20))
        .await
        .unwrap();
    // Copies change nothing of what was downloaded; neither does a second
    // report of the download.
    images::add_copy(&mut conn, img.id, Copy::local(h2.id))
        .await
        .unwrap();
    images::add_copy(&mut conn, img.id, Copy::local(h2.id))
        .await
        .unwrap();
    images::downloaded(&mut conn, img.id, Copy::local(h1.id), &stored(9 << 30))
        .await
        .unwrap();
    let got = images::get(&pool, img.id).await.unwrap();
    assert_eq!(got.state, "ready");
    assert_eq!(
        (got.sha256.as_deref(), got.digest.as_deref()),
        (Some("abc"), Some("def"))
    );
    assert_eq!(
        (got.size_bytes, got.virtual_bytes),
        (Some(400 << 20), Some(3 << 30))
    );
    assert_eq!(got.download_host_id, Some(h1.id));
    // Downloading again starts on a host yet to pick.
    images::downloading(&pool, img.id).await.unwrap();
    assert_eq!(
        images::get(&pool, img.id).await.unwrap().download_host_id,
        None
    );
    assert_eq!(images::downloading_count(&pool, a.id).await.unwrap(), 1);
    images::downloaded(&mut conn, img.id, Copy::local(h1.id), &stored(400 << 20))
        .await
        .unwrap();
    assert_eq!(
        images::copies(&pool, img.id).await.unwrap(),
        [Copy::local(h1.id), Copy::local(h2.id)]
    );
    // Charged its stored size once, however many hosts hold it.
    let used = mst_db::quotas::usage(&pool, a.id).await.unwrap();
    assert_eq!(used["image_gib"], 1);
    assert_eq!(
        images::copy_hosts(&pool, &[img.id]).await.unwrap()[&img.id].len(),
        2
    );
    images::remove_copy(&pool, img.id, Copy::local(h1.id))
        .await
        .unwrap();
    assert_eq!(
        images::copies(&pool, img.id).await.unwrap(),
        [Copy::local(h2.id)]
    );
    // A copy in a shared pool is the pool's, not a host's: once.
    let nfs = mst_db::storage_pools::create(
        &mut conn,
        "nfs",
        "nfs",
        &serde_json::json!({"server": "10.0.0.1", "export": "/x"}),
        &[],
    )
    .await
    .unwrap();
    for _ in 0..2 {
        images::add_copy(&mut conn, img.id, Copy::shared(nfs))
            .await
            .unwrap();
    }
    assert_eq!(
        images::copies(&pool, img.id).await.unwrap(),
        [Copy::local(h2.id), Copy::shared(nfs)]
    );
    assert_eq!(images::count_in_pool(&pool, nfs).await.unwrap(), 1);
    assert_eq!(
        images::copy_hosts(&pool, &[img.id]).await.unwrap()[&img.id],
        ["mst-host1"]
    );

    // Deleted meanwhile: nothing to record a copy on.
    images::delete(&pool, img.id).await.unwrap();
    assert_eq!(images::count_in_pool(&pool, nfs).await.unwrap(), 0);
    assert!(
        images::add_copy(&mut conn, img.id, Copy::local(h1.id))
            .await
            .is_err()
    );
}

#[sqlx::test(migrator = "mst_db::MIGRATOR")]
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
        vms::observe_power(&pool, v1.id, h.id, "running", None)
            .await
            .unwrap()
    );
    assert!(
        !vms::observe_power(&pool, v1.id, h.id, "running", None)
            .await
            .unwrap(),
        "no change"
    );
    assert!(
        !vms::observe_power(&pool, v1.id, HostId::new(), "stopped", None)
            .await
            .unwrap(),
        "wrong host"
    );
    assert_eq!(vms::get(&pool, v1.id).await.unwrap().power, "running");
}

#[sqlx::test(migrator = "mst_db::MIGRATOR")]
async fn scheduling_snapshot_reports_allocations_and_liveness(pool: PgPool) {
    let t = root(&pool).await;
    let h = host(&pool).await;
    let cluster = mst_db::clusters::create(&pool, "c1", 2.0, 1.5, false)
        .await
        .unwrap();
    hosts::set_cluster(&pool, h.id, Some(cluster.id))
        .await
        .unwrap();
    hosts::set_connected(&pool, h.id, Some("replica"))
        .await
        .unwrap();
    let spec = VmSpec {
        cpu: None,
        migratable: true,
        extra: Default::default(),
        vcpus: 3,
        memory_mib: 1024,
        max_memory_mib: None,
        max_vcpus: None,
        free_page_reporting: true,
        firmware: Default::default(),
        disks: vec![],
        nics: vec![],
        cloud_init: None,
    };
    vms::create(
        &pool,
        vms::NewVm {
            id: VmId::new(),
            tenant_id: t.id,
            name: "a",
            host_id: h.id,
            spec: &spec,
            desired_power: "running",
            created_by: None,
        },
    )
    .await
    .unwrap();

    let mut tx = pool.begin().await.unwrap();
    let c = mst_db::clusters::candidates_locked(&mut tx).await.unwrap();
    assert_eq!(c.len(), 1);
    assert_eq!(
        (
            c[0].cluster_id,
            c[0].live,
            c[0].used_vcpus,
            c[0].used_memory_mib
        ),
        (Some(cluster.id), true, 3, 1024)
    );
    assert_eq!((c[0].cpu_overcommit, c[0].memory_overcommit), (2.0, 1.5));
    tx.commit().await.unwrap();

    // A cluster with hosts cannot be deleted.
    let mut tx = pool.begin().await.unwrap();
    assert!(mst_db::clusters::delete(&mut tx, cluster.id).await.is_err());
}

#[sqlx::test(migrator = "mst_db::MIGRATOR")]
async fn power_observations_follow_stops_but_not_crashes(pool: PgPool) {
    let t = root(&pool).await;
    let h = host(&pool).await;
    let v = vm(&pool, &t, &h, "a").await;

    assert!(
        vms::observe_power(&pool, v.id, h.id, "running", None)
            .await
            .unwrap()
    );
    assert!(
        !vms::observe_power(&pool, v.id, h.id, "running", None)
            .await
            .unwrap(),
        "no change"
    );
    let before = vms::get(&pool, v.id)
        .await
        .unwrap()
        .power_observed_at
        .unwrap();

    // A crash: still meant to run.
    assert!(
        vms::observe_power(&pool, v.id, h.id, "crashed", Some("crashed"))
            .await
            .unwrap()
    );
    assert_eq!(
        vms::get(&pool, v.id).await.unwrap().desired_power,
        "running"
    );
    vms::observe_power(&pool, v.id, h.id, "running", None)
        .await
        .unwrap();

    // The guest shuts itself down: recorded with its reason, and it is now
    // meant to be stopped.
    assert!(
        vms::observe_power(&pool, v.id, h.id, "stopped", Some("shutdown"))
            .await
            .unwrap()
    );
    let vm = vms::get(&pool, v.id).await.unwrap();
    assert_eq!(
        (vm.power.as_str(), vm.power_reason.as_deref()),
        ("stopped", Some("shutdown"))
    );
    assert_eq!(vm.desired_power, "stopped");
    assert!(vm.power_observed_at.unwrap() >= before);

    // Started through the API: a stale report of the stop (no change)
    // leaves the new intent alone.
    vms::set_desired_power(&pool, v.id, "running")
        .await
        .unwrap();
    assert!(
        !vms::observe_power(&pool, v.id, h.id, "stopped", Some("shutdown"))
            .await
            .unwrap()
    );
    assert_eq!(
        vms::get(&pool, v.id).await.unwrap().desired_power,
        "running"
    );

    // A changed reason alone is a change; reports for the wrong host are not.
    assert!(
        vms::observe_power(&pool, v.id, h.id, "stopped", None)
            .await
            .unwrap()
    );
    assert!(
        !vms::observe_power(&pool, v.id, HostId::new(), "running", None)
            .await
            .unwrap()
    );
}

#[sqlx::test(migrator = "mst_db::MIGRATOR")]
async fn active_tasks_by_target(pool: PgPool) {
    let t = root(&pool).await;
    let h = host(&pool).await;
    let v = vm(&pool, &t, &h, "a").await;
    let new = |kind| mst_db::tasks::NewTask {
        tenant_id: t.id,
        kind,
        target: Some(("vm", v.id.0)),
        params: serde_json::json!({}),
        created_by: None,
    };
    let first = mst_db::tasks::enqueue(&pool, new("vm.stop")).await.unwrap();
    mst_db::tasks::enqueue(&pool, new("vm.start"))
        .await
        .unwrap();
    assert_eq!(
        mst_db::tasks::active_by_target(&pool).await.unwrap(),
        vec![(v.id.0, "vm.stop".to_owned())]
    );

    let claimed = mst_db::tasks::claim(&pool, "w", std::time::Duration::from_secs(30), &[], false)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, first.id);
    mst_db::tasks::finish(&pool, first.id, "w", Ok(()))
        .await
        .unwrap();
    assert_eq!(
        mst_db::tasks::active_by_target(&pool).await.unwrap(),
        vec![(v.id.0, "vm.start".to_owned())]
    );
}

/// A renewed certificate's predecessor is retired once the host connects
/// with the new one; decommissioning hands back what is left to revoke.
#[sqlx::test(migrator = "mst_db::MIGRATOR")]
async fn host_certificates_are_retired_and_revoked(pool: PgPool) {
    let h = host(&pool).await;
    let first_expiry = h.cert_expires_at;
    let later = Utc::now() + chrono::Duration::days(30);
    let mut conn = pool.acquire().await.unwrap();
    // A host that renews but never uses the new certificate, and renews
    // again with the old one: the unused one is revoked, so no more than
    // two stay valid.
    let unused = hosts::renew_cert(&mut conn, h.id, "fp", "fpx", later, "0x")
        .await
        .unwrap();
    assert!(unused.is_empty());
    let unused = hosts::renew_cert(&mut conn, h.id, "fp", "fp2", later, "02")
        .await
        .unwrap();
    assert_eq!(
        unused.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>(),
        ["0x"]
    );
    assert!(!hosts::accepts_cert(&pool, h.id, "fpx").await.unwrap());
    // Still on the old certificate: nothing to retire yet.
    assert!(
        hosts::retire_previous_cert(&pool, h.id, "fp")
            .await
            .unwrap()
            .is_none()
    );
    assert!(hosts::accepts_cert(&pool, h.id, "fp").await.unwrap());
    let old = hosts::retire_previous_cert(&pool, h.id, "fp2")
        .await
        .unwrap();
    assert_eq!(
        old.map(|(s, e)| (s, e.timestamp())),
        Some(("01".into(), first_expiry.timestamp()))
    );
    assert!(!hosts::accepts_cert(&pool, h.id, "fp").await.unwrap());
    assert!(
        hosts::retire_previous_cert(&pool, h.id, "fp2")
            .await
            .unwrap()
            .is_none()
    );

    let left = hosts::decommission(&pool, h.id).await.unwrap();
    assert_eq!(
        left.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>(),
        ["02"]
    );
    for (serial, expires) in &left {
        mst_db::pki::revoke(&pool, serial, Some(h.id), "decommissioned", *expires)
            .await
            .unwrap();
    }
    // Twice is fine; expired ones drop out of the list.
    mst_db::pki::revoke(&pool, "02", Some(h.id), "again", later)
        .await
        .unwrap();
    mst_db::pki::revoke(
        &pool,
        "03",
        None,
        "old",
        Utc::now() - chrono::Duration::days(1),
    )
    .await
    .unwrap();
    let listed: Vec<String> = mst_db::pki::revoked(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|(s, _)| s)
        .collect();
    assert_eq!(listed, ["02"]);
    assert_eq!(
        hosts::get(&pool, h.id).await.unwrap().state,
        "decommissioned"
    );
}

#[sqlx::test(migrator = "mst_db::MIGRATOR")]
async fn backup_schedules_are_due_and_pruned(pool: PgPool) {
    use mst_db::backups;
    let t = root(&pool).await;
    let h = host(&pool).await;
    let v = vm(&pool, &t, &h, "web-1").await;
    let config = serde_json::json!({"server": "nfs1", "export": "/backups"});
    let repo = backups::create_repository(&pool, "nfs1", "nfs", &config)
        .await
        .unwrap();
    let due = |pool: PgPool| async move {
        backups::due(&pool)
            .await
            .unwrap()
            .iter()
            .map(|s| s.vm_id)
            .collect::<Vec<_>>()
    };
    assert!(due(pool.clone()).await.is_empty(), "no schedule yet");
    let schedule = backups::NewSchedule {
        every: "daily",
        keep: 2,
        repository_id: repo,
        full_every: 1,
    };
    backups::set_schedule(&pool, v.id, Some(schedule))
        .await
        .unwrap();
    assert_eq!(due(pool.clone()).await, [v.id]);

    // Not while one runs, nor within a day of the last.
    let take = |scheduled| {
        let (pool, spec) = (pool.clone(), v.spec.0.clone());
        async move {
            let full = backups::Taking::Full { reason: None };
            backups::create(
                &pool,
                t.id,
                v.id,
                "web-1",
                repo,
                scheduled,
                &spec,
                1 << 30,
                full,
            )
            .await
            .unwrap()
        }
    };
    let mut conn = pool.acquire().await.unwrap();
    let first = take(true).await;
    assert!(backups::running_for(&pool, v.id).await.unwrap());
    assert!(due(pool.clone()).await.is_empty(), "one is running");
    backups::finish(&mut conn, first, 1 << 20, true, None)
        .await
        .unwrap();
    assert!(due(pool.clone()).await.is_empty(), "taken today");
    let age = |id: mst_core::ids::BackupId, days: i32| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "UPDATE backups SET created_at = now() - make_interval(days => $2) WHERE id = $1",
            )
            .bind(id)
            .bind(days)
            .execute(&pool)
            .await
            .unwrap();
        }
    };
    age(first, 3).await;
    assert_eq!(due(pool.clone()).await, [v.id], "a day has passed");

    // Beyond the newest two scheduled ones, the oldest go; manual ones stay.
    let second = take(true).await;
    backups::finish(&mut conn, second, 1, true, None)
        .await
        .unwrap();
    age(second, 2).await;
    let third = take(true).await;
    backups::finish(&mut conn, third, 1, false, None)
        .await
        .unwrap();
    let manual = take(false).await;
    backups::finish(&mut conn, manual, 1, true, None)
        .await
        .unwrap();
    age(manual, 9).await;
    assert_eq!(backups::beyond_keep(&pool, v.id, 2).await.unwrap(), [first]);

    // A repository with backups stays; its last one gone, so do schedules.
    assert!(!backups::delete_repository(&mut conn, repo).await.unwrap());
    for id in [first, second, third, manual] {
        backups::delete(&pool, id).await.unwrap();
    }
    assert!(backups::delete_repository(&mut conn, repo).await.unwrap());
    assert!(backups::schedule_of(&pool, v.id).await.unwrap().is_none());
}

#[sqlx::test(migrator = "mst_db::MIGRATOR")]
async fn incremental_backups_form_chains(pool: PgPool) {
    use mst_db::backups::{self, Taking};
    let t = root(&pool).await;
    let h = host(&pool).await;
    let v = vm(&pool, &t, &h, "web-1").await;
    let config = serde_json::json!({"server": "nfs1", "export": "/backups"});
    let repo = backups::create_repository(&pool, "nfs1", "nfs", &config)
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let schedule = backups::NewSchedule {
        every: "daily",
        keep: 7,
        repository_id: repo,
        full_every: 3,
    };
    backups::set_schedule(&pool, v.id, Some(schedule))
        .await
        .unwrap();
    let full_due = |pool: PgPool| async move {
        backups::schedule_of(&pool, v.id)
            .await
            .unwrap()
            .unwrap()
            .full_due()
    };
    // Builds on the newest complete one; none yet.
    assert!(
        backups::newest_ready(&pool, v.id, repo)
            .await
            .unwrap()
            .is_none()
    );
    let spec = v.spec.0.clone();
    macro_rules! new {
        ($taking:expr) => {
            backups::create(
                &pool,
                t.id,
                v.id,
                "web-1",
                repo,
                true,
                &spec,
                1 << 30,
                $taking,
            )
        };
    }
    let a = new!(Taking::Full { reason: None }).await.unwrap();
    backups::finish(&mut conn, a, 100, true, None)
        .await
        .unwrap();
    let a = backups::get(&pool, a).await.unwrap();
    assert_eq!(
        (a.kind.as_str(), a.parent_id, a.chain_id),
        ("full", None, a.id)
    );
    assert!(!full_due(pool.clone()).await);

    let parent = backups::newest_ready(&pool, v.id, repo)
        .await
        .unwrap()
        .unwrap();
    let b = new!(Taking::Incremental { parent: &parent }).await.unwrap();
    assert!(backups::child_running(&pool, a.id).await.unwrap());
    backups::finish(&mut conn, b, 10, true, None).await.unwrap();
    let b = backups::get(&pool, b).await.unwrap();
    assert_eq!(
        (b.kind.as_str(), b.parent_id, b.chain_id),
        ("incremental", Some(a.id), a.id)
    );
    assert!(
        !full_due(pool.clone()).await,
        "one incremental since the full one"
    );
    assert_eq!(
        backups::taken_before(&pool, v.id, &b).await.unwrap(),
        [b.id, a.id]
    );

    // Asked to be incremental, taken in full: it stands alone.
    let c = new!(Taking::Incremental { parent: &b }).await.unwrap();
    backups::finish(&mut conn, c, 100, true, Some("no tracking"))
        .await
        .unwrap();
    let c = backups::get(&pool, c).await.unwrap();
    assert_eq!(
        (c.kind.as_str(), c.parent_id, c.chain_id),
        ("full", None, c.id)
    );
    assert_eq!(c.full_reason.as_deref(), Some("no tracking"));
    let d = new!(Taking::Incremental { parent: &c }).await.unwrap();
    backups::finish(&mut conn, d, 5, true, None).await.unwrap();
    let e = new!(Taking::Incremental {
        parent: &backups::get(&pool, d).await.unwrap()
    })
    .await
    .unwrap();
    backups::finish(&mut conn, e, 5, true, None).await.unwrap();
    assert!(
        full_due(pool.clone()).await,
        "two incremental ones since the last full one"
    );

    // Quotas count what complete backups take up: 100 + 10 + 100 + 5 + 5.
    let used = mst_db::quotas::usage(&mut *conn, t.id).await.unwrap();
    assert_eq!(used["backup_gib"], 1);

    // Restoring from e reads d and c: neither may be merged away meanwhile,
    // and a merge under way holds restores off.
    sqlx::query(
        "UPDATE vms SET status = 'creating', spec = jsonb_set(spec, '{disks}', $1) WHERE id = $2",
    )
    .bind(serde_json::json!([{"restored_from": {"backup_id": e.to_string(), "index": 0}}]))
    .bind(v.id)
    .execute(&pool)
    .await
    .unwrap();
    assert!(backups::being_restored(&pool, c.id).await.unwrap());
    assert!(
        !backups::being_restored(&pool, a.id).await.unwrap(),
        "another chain"
    );
    sqlx::query("UPDATE vms SET status = 'ready' WHERE id = $1")
        .bind(v.id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(!backups::deleting_below(&pool, e).await.unwrap());
    backups::set_deleting(&pool, c.id).await.unwrap();
    assert!(backups::deleting_below(&pool, e).await.unwrap());
    assert!(
        backups::newest_ready(&pool, v.id, repo)
            .await
            .unwrap()
            .is_some_and(|n| n.id == e)
    );

    // c merged into d: d stands alone, takes what it now holds.
    let children = backups::children(&pool, c.id).await.unwrap();
    assert_eq!(children.iter().map(|b| b.id).collect::<Vec<_>>(), [d]);
    backups::merged(&mut conn, &c, &[(d, 104)]).await.unwrap();
    let d = backups::get(&pool, d).await.unwrap();
    assert_eq!(
        (d.parent_id, d.stored_bytes, d.chain_id),
        (None, Some(104), c.id)
    );
    assert!(backups::get(&pool, c.id).await.is_err());
    assert!(!backups::deleting_below(&pool, e).await.unwrap());

    // A failed one builds on nothing, and holds nothing up.
    let f = new!(Taking::Incremental { parent: &d }).await.unwrap();
    backups::fail(&pool, f, "broke").await.unwrap();
    assert_eq!(backups::get(&pool, f).await.unwrap().parent_id, None);
    assert!(
        backups::children(&pool, d.id)
            .await
            .unwrap()
            .iter()
            .all(|b| b.id == e)
    );
}

#[sqlx::test(migrator = "mst_db::MIGRATOR")]
async fn snapshots_are_unique_per_vm_and_mark_it_busy(pool: PgPool) {
    use mst_db::snapshots;
    let t = root(&pool).await;
    let h = host(&pool).await;
    let v = vm(&pool, &t, &h, "web-1").await;
    let id = snapshots::create(&pool, t.id, v.id, "before", &v.spec.0)
        .await
        .unwrap();
    assert!(
        snapshots::create(&pool, t.id, v.id, "before", &v.spec.0)
            .await
            .is_err(),
        "names are unique per VM"
    );
    assert_eq!(
        snapshots::busy(&pool, v.id).await.unwrap().as_deref(),
        Some("creating")
    );
    snapshots::set_state(&pool, id, "ready", Some(false), None)
        .await
        .unwrap();
    assert_eq!(snapshots::busy(&pool, v.id).await.unwrap(), None);
    // A later state change keeps what was found when it was taken.
    snapshots::set_state(&pool, id, "ready", None, Some("revert failed"))
        .await
        .unwrap();
    let s = snapshots::get(&pool, id).await.unwrap();
    assert_eq!(
        (s.consistent, s.error.as_deref()),
        (Some(false), Some("revert failed"))
    );
    assert_eq!(snapshots::count(&pool, v.id).await.unwrap(), 1);

    // They go with their VM.
    sqlx::query("DELETE FROM vms WHERE id = $1")
        .bind(v.id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(snapshots::get(&pool, id).await.is_err());
}

#[sqlx::test(migrator = "mst_db::MIGRATOR")]
async fn migrations_under_way_are_listed(pool: PgPool) {
    use mst_db::{migrations, tasks};
    let t = root(&pool).await;
    let h = host(&pool).await;
    let v = vm(&pool, &t, &h, "web-1").await;
    let task = tasks::enqueue(
        &pool,
        tasks::NewTask {
            tenant_id: t.id,
            kind: "vm.migrate",
            target: Some(("vm", v.id.0)),
            params: serde_json::json!({}),
            created_by: None,
        },
    )
    .await
    .unwrap();
    let facts = hosts::Facts {
        name: "mst-host2",
        address: "10.77.0.12",
        ovn_chassis: "mst-host2",
        cpu_count: 4,
        memory_mib: 6000,
        cpu_model: "x86_64",
        kvm: true,
        libvirt_version: "11.3.0",
        agent_version: "0.1.0",
        physical_networks: &[],
        cpu_json: "",
        migration_ready: false,
    };
    let dest = hosts::create(&pool, HostId::new(), &facts, "fp2", Utc::now(), "01")
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    migrations::begin(&mut conn, v.id, task.id, h.id, dest.id, 1, 512)
        .await
        .unwrap();
    let all = migrations::list(&pool).await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(
        (all[0].vm_id, all[0].handoff.0.clone()),
        (v.id, serde_json::json!({}))
    );
}

#[sqlx::test(migrator = "mst_db::MIGRATOR")]
async fn a_source_is_being_cloned_while_its_clone_is_created(pool: PgPool) {
    use mst_core::ids::VolumeId;
    use mst_core::vm::DiskSpec;
    let t = root(&pool).await;
    let h = host(&pool).await;
    let disk = |volume_id, cloned_from| DiskSpec {
        volume_id,
        pool_id: mst_core::ids::LOCAL_POOL,
        size_bytes: 1 << 30,
        image_id: None,
        restored_from: None,
        cloned_from,
        unit: None,
        extra: Default::default(),
    };
    let src = vm(&pool, &t, &h, "tpl").await;
    let src_volume = VolumeId::new();
    let mut spec = src.spec.0.clone();
    spec.disks = vec![disk(src_volume, None)];
    vms::set_spec(&pool, src.id, &spec).await.unwrap();
    let other = vm(&pool, &t, &h, "other").await;
    assert!(!vms::being_cloned(&pool, src.id).await.unwrap());

    let copy = vm(&pool, &t, &h, "copy").await;
    let mut spec = copy.spec.0.clone();
    spec.disks = vec![disk(VolumeId::new(), Some(src_volume))];
    vms::set_spec(&pool, copy.id, &spec).await.unwrap();
    // `vm` leaves it creating.
    assert!(vms::being_cloned(&pool, src.id).await.unwrap());
    assert!(!vms::being_cloned(&pool, other.id).await.unwrap());
    vms::set_status(&pool, copy.id, "ready", None)
        .await
        .unwrap();
    assert!(!vms::being_cloned(&pool, src.id).await.unwrap());

    vms::set_template(&pool, src.id, true).await.unwrap();
    assert!(vms::get(&pool, src.id).await.unwrap().template);
}

#[sqlx::test(migrator = "mst_db::MIGRATOR")]
async fn hosts_changes_wait_until_accepted(pool: PgPool) {
    let h = host(&pool).await;
    let facts = hosts::Facts {
        name: &h.name,
        address: &h.address,
        cpu_count: h.cpu_count,
        memory_mib: h.memory_mib,
        cpu_model: "",
        kvm: true,
        libvirt_version: "",
        ovn_chassis: &h.ovn_chassis,
        agent_version: "",
        physical_networks: &h.physical_networks,
        cpu_json: "",
        migration_ready: true,
    };
    let pending =
        serde_json::json!({"memory_mib": 1 << 20, "physical_networks": ["external", "other"]});
    hosts::update_facts(&pool, h.id, &facts, Some(&pending))
        .await
        .unwrap();
    let waiting = hosts::get(&pool, h.id).await.unwrap();
    assert_eq!(waiting.memory_mib, h.memory_mib);
    assert_eq!(waiting.pending_facts, Some(pending));
    assert!(hosts::accept_pending_facts(&pool, h.id).await.unwrap());
    let accepted = hosts::get(&pool, h.id).await.unwrap();
    assert_eq!(accepted.memory_mib, 1 << 20);
    assert_eq!(accepted.physical_networks, ["external", "other"]);
    assert_eq!((accepted.name, accepted.cpu_count), (h.name, h.cpu_count));
    assert!(accepted.pending_facts.is_none());
    assert!(!hosts::accept_pending_facts(&pool, h.id).await.unwrap());
}
