//! Repository tests for tenancy: role bindings, tokens, quotas, grants, audit.

use chrono::Utc;
use hm_core::ids::{HostId, TenantId, VmId, VolumeId};
use hm_core::vm::VmSpec;
use hm_db::quotas::Amounts;
use hm_db::{audit, grants, hosts, quotas, rbac, tenants, users, vms};
use sqlx::PgPool;
use uuid::Uuid;

struct Tree {
    root: tenants::Tenant,
    acme: tenants::Tenant,
    dev: tenants::Tenant,
    globex: tenants::Tenant,
}

async fn tree(pool: &PgPool) -> Tree {
    let root = tenants::create(pool, None, "provider", "Provider")
        .await
        .unwrap();
    let acme = tenants::create(pool, Some(root.id), "acme", "Acme")
        .await
        .unwrap();
    let dev = tenants::create(pool, Some(acme.id), "dev", "Acme Dev")
        .await
        .unwrap();
    let globex = tenants::create(pool, Some(root.id), "globex", "Globex")
        .await
        .unwrap();
    Tree {
        root,
        acme,
        dev,
        globex,
    }
}

async fn user(pool: &PgPool, tenant: TenantId, name: &str) -> users::User {
    let new = users::NewUser {
        tenant_id: tenant,
        username: name,
        display_name: name,
        email: None,
        password_hash: Some("x"),
    };
    users::create(pool, new).await.unwrap()
}

async fn host(pool: &PgPool) -> HostId {
    let f = hosts::Facts {
        name: "h",
        address: "10.0.0.1",
        cpu_count: 64,
        memory_mib: 1 << 20,
        cpu_model: "",
        kvm: true,
        libvirt_version: "",
        ovn_chassis: "",
        agent_version: "",
        physical_networks: &[],
        cpu_json: "",
        migration_ready: false,
    };
    hosts::create(pool, HostId::new(), &f, "fp", Utc::now())
        .await
        .unwrap()
        .id
}

/// Allocates a VM (with a disk) under quota, as the API does.
async fn allocate(
    pool: &PgPool,
    tenant: TenantId,
    host: HostId,
    vcpus: u32,
    mem: u64,
    disk_gib: i64,
) -> hm_db::Result<VmId> {
    let mut tx = pool.begin().await.unwrap();
    let delta: Amounts = [
        ("vms", 1),
        ("vcpus", vcpus as i64),
        ("memory_mib", mem as i64),
        ("disk_gib", disk_gib),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    quotas::reserve(&mut tx, tenant, &delta).await?;
    let id = VmId::new();
    let spec = VmSpec {
        cpu: None,
        vcpus,
        memory_mib: mem,
        firmware: Default::default(),
        disks: vec![],
        nics: vec![],
        cloud_init: None,
    };
    vms::create(
        &mut *tx,
        vms::NewVm {
            id,
            tenant_id: tenant,
            name: &id.to_string()[28..],
            host_id: host,
            spec: &spec,
            desired_power: "running",
            created_by: None,
        },
    )
    .await?;
    let vol = vms::Volume {
        id: VolumeId::new(),
        tenant_id: tenant,
        vm_id: Some(id),
        host_id: Some(host),
        pool_id: hm_core::ids::LOCAL_POOL,
        size_bytes: disk_gib << 30,
        image_id: None,
    };
    vms::create_volume(&mut *tx, &vol).await?;
    tx.commit().await.unwrap();
    Ok(id)
}

#[sqlx::test(migrator = "hm_db::MIGRATOR")]
async fn bindings_resolve_to_tenant_paths(pool: PgPool) {
    let t = tree(&pool).await;
    let u = user(&pool, t.acme.id, "alice").await;
    rbac::create_binding(&pool, u.id, t.acme.id, "operator", None)
        .await
        .unwrap();
    rbac::create_binding(&pool, u.id, t.globex.id, "viewer", None)
        .await
        .unwrap();
    assert!(
        rbac::create_binding(&pool, u.id, t.acme.id, "operator", None)
            .await
            .is_err(),
        "duplicate"
    );

    let mut grants = rbac::grants_of(&pool, u.id).await.unwrap();
    grants.sort_by_key(|(p, _)| p.to_string());
    assert!(grants.contains(&(t.acme.path.clone(), "operator".into())));
    assert!(grants.contains(&(t.globex.path.clone(), "viewer".into())));
    assert_eq!(
        rbac::bindings_at(&pool, t.acme.id).await.unwrap()[0].username,
        "alice"
    );
}

#[sqlx::test(migrator = "hm_db::MIGRATOR")]
async fn root_admin_count_ignores_disabled_users_and_other_tenants(pool: PgPool) {
    let t = tree(&pool).await;
    let a = user(&pool, t.root.id, "a").await;
    let b = user(&pool, t.root.id, "b").await;
    let c = user(&pool, t.acme.id, "c").await;
    rbac::create_binding(&pool, a.id, t.root.id, "admin", None)
        .await
        .unwrap();
    rbac::create_binding(&pool, b.id, t.root.id, "admin", None)
        .await
        .unwrap();
    rbac::create_binding(&pool, c.id, t.acme.id, "admin", None)
        .await
        .unwrap();
    assert_eq!(rbac::root_admin_count(&pool).await.unwrap(), 2);
    users::update(
        &pool,
        b.id,
        users::Changes {
            display_name: None,
            email: None,
            password_hash: None,
            disabled: Some(true),
        },
    )
    .await
    .unwrap();
    assert_eq!(rbac::root_admin_count(&pool).await.unwrap(), 1);
}

#[sqlx::test(migrator = "hm_db::MIGRATOR")]
async fn api_tokens_resolve_until_expired_or_user_disabled(pool: PgPool) {
    let t = tree(&pool).await;
    let u = user(&pool, t.acme.id, "bot").await;
    rbac::create_token(&pool, u.id, "ci", "h1", None)
        .await
        .unwrap();
    rbac::create_token(
        &pool,
        u.id,
        "old",
        "h2",
        Some(Utc::now() - chrono::Duration::seconds(1)),
    )
    .await
    .unwrap();
    assert_eq!(rbac::resolve_token(&pool, "h1").await.unwrap(), Some(u.id));
    assert_eq!(rbac::resolve_token(&pool, "h2").await.unwrap(), None);
    assert!(
        rbac::tokens_of(&pool, u.id).await.unwrap()[1]
            .last_used_at
            .is_some()
            || rbac::tokens_of(&pool, u.id).await.unwrap()[0]
                .last_used_at
                .is_some()
    );

    let other = user(&pool, t.acme.id, "mallory").await;
    let tok = rbac::tokens_of(&pool, u.id).await.unwrap()[0].id;
    assert!(
        !rbac::delete_token(&pool, tok, other.id).await.unwrap(),
        "only the owner may delete"
    );
    users::update(
        &pool,
        u.id,
        users::Changes {
            display_name: None,
            email: None,
            password_hash: None,
            disabled: Some(true),
        },
    )
    .await
    .unwrap();
    assert_eq!(rbac::resolve_token(&pool, "h1").await.unwrap(), None);
}

#[sqlx::test(migrator = "hm_db::MIGRATOR")]
async fn quotas_apply_to_whole_subtrees(pool: PgPool) {
    let t = tree(&pool).await;
    let h = host(&pool).await;
    let mut tx = pool.begin().await.unwrap();
    quotas::set_limit(&mut tx, t.acme.id, "vcpus", Some(4))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // dev has no limit of its own, but acme's covers it.
    allocate(&pool, t.dev.id, h, 2, 512, 1).await.unwrap();
    allocate(&pool, t.acme.id, h, 2, 512, 1).await.unwrap();
    let err = allocate(&pool, t.dev.id, h, 1, 512, 1).await.unwrap_err();
    assert!(
        matches!(err, hm_db::Error::Domain(hm_core::Error::QuotaExceeded { ref tenant, .. }) if tenant == "Acme"),
        "{err:?}"
    );
    // Other subtrees are unaffected.
    allocate(&pool, t.globex.id, h, 8, 512, 1).await.unwrap();

    let used = quotas::usage(&pool, t.acme.id).await.unwrap();
    assert_eq!((used["vms"], used["vcpus"], used["disk_gib"]), (2, 4, 2));
}

#[sqlx::test(migrator = "hm_db::MIGRATOR")]
async fn concurrent_allocations_cannot_overrun_a_quota(pool: PgPool) {
    let t = tree(&pool).await;
    let h = host(&pool).await;
    let mut tx = pool.begin().await.unwrap();
    quotas::set_limit(&mut tx, t.acme.id, "vms", Some(3))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let attempts: Vec<_> = (0..10)
        .map(|i| {
            let pool = pool.clone();
            let tenant = if i % 2 == 0 { t.acme.id } else { t.dev.id };
            tokio::spawn(async move {
                match allocate(&pool, tenant, h, 1, 256, 1).await {
                    Ok(_) => true,
                    Err(hm_db::Error::Domain(hm_core::Error::QuotaExceeded { .. })) => false,
                    Err(e) => panic!("unexpected error: {e:?}"),
                }
            })
        })
        .collect();
    let mut ok = 0;
    for a in attempts {
        ok += usize::from(a.await.unwrap());
    }
    assert_eq!(ok, 3);
    assert_eq!(quotas::usage(&pool, t.acme.id).await.unwrap()["vms"], 3);
}

#[sqlx::test(migrator = "hm_db::MIGRATOR")]
async fn child_limits_stay_within_parent_limits(pool: PgPool) {
    let t = tree(&pool).await;
    let mut tx = pool.begin().await.unwrap();
    quotas::set_limit(&mut tx, t.acme.id, "memory_mib", Some(8192))
        .await
        .unwrap();
    quotas::set_limit(&mut tx, t.dev.id, "memory_mib", Some(4096))
        .await
        .unwrap();
    let too_high = quotas::set_limit(&mut tx, t.dev.id, "memory_mib", Some(9000))
        .await
        .unwrap_err();
    assert!(
        too_high.to_string().contains("Acme's limit of 8192"),
        "{too_high}"
    );
    let below_child = quotas::set_limit(&mut tx, t.acme.id, "memory_mib", Some(2048))
        .await
        .unwrap_err();
    assert!(
        below_child
            .to_string()
            .contains("Acme Dev has a memory_mib limit of 4096"),
        "{below_child}"
    );
    assert!(
        quotas::set_limit(&mut tx, t.acme.id, "gpus", Some(1))
            .await
            .is_err()
    );

    quotas::set_limit(&mut tx, t.dev.id, "memory_mib", None)
        .await
        .unwrap();
    quotas::set_limit(&mut tx, t.acme.id, "memory_mib", Some(2048))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        quotas::limits(&pool, t.acme.id).await.unwrap()["memory_mib"],
        2048
    );
}

#[sqlx::test(migrator = "hm_db::MIGRATOR")]
async fn grants_flow_down_and_revocations_cascade(pool: PgPool) {
    let t = tree(&pool).await;
    let (c1, c2) = (Uuid::now_v7(), Uuid::now_v7());
    assert!(
        grants::may_use(&pool, t.root.id, "cluster", c1)
            .await
            .unwrap(),
        "the root may use anything"
    );

    let mut tx = pool.begin().await.unwrap();
    grants::set(&mut tx, t.acme.id, "cluster", &[c1, c2])
        .await
        .unwrap();
    grants::set(&mut tx, t.dev.id, "cluster", &[c1])
        .await
        .unwrap();
    // dev cannot be given what acme does not have; globex was given nothing.
    assert!(
        grants::set(&mut tx, t.dev.id, "cluster", &[Uuid::now_v7()])
            .await
            .is_err()
    );
    assert!(
        grants::set(&mut tx, t.root.id, "cluster", &[c1])
            .await
            .is_err()
    );
    tx.commit().await.unwrap();
    assert!(
        grants::may_use(&pool, t.dev.id, "cluster", c1)
            .await
            .unwrap()
    );
    assert!(
        !grants::may_use(&pool, t.globex.id, "cluster", c1)
            .await
            .unwrap()
    );

    // Taking c1 away from acme takes it from dev as well.
    let mut tx = pool.begin().await.unwrap();
    grants::set(&mut tx, t.acme.id, "cluster", &[c2])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(
        !grants::may_use(&pool, t.dev.id, "cluster", c1)
            .await
            .unwrap()
    );
    assert_eq!(
        grants::list(&pool, t.acme.id, "cluster").await.unwrap(),
        vec![c2]
    );
}

#[sqlx::test(migrator = "hm_db::MIGRATOR")]
async fn audit_entries_are_scoped_to_subtrees_and_outlive_tenants(pool: PgPool) {
    let t = tree(&pool).await;
    for (tenant, action) in [
        (t.root.id, "host.enroll"),
        (t.acme.id, "vm.create"),
        (t.dev.id, "vm.delete"),
        (t.globex.id, "vm.create"),
    ] {
        audit::record(
            &pool,
            audit::Entry {
                user_id: None,
                username: Some("x"),
                tenant_id: Some(tenant),
                action,
                target: None,
                success: true,
                details: serde_json::json!({}),
            },
        )
        .await
        .unwrap();
    }
    let acme: Vec<_> = audit::list(&pool, t.acme.id, None, 50)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.action)
        .collect();
    assert_eq!(acme, ["vm.delete", "vm.create"]);
    assert_eq!(
        audit::list(&pool, t.root.id, None, 50).await.unwrap().len(),
        4
    );
    let page = audit::list(&pool, t.root.id, None, 2).await.unwrap();
    assert_eq!(
        audit::list(&pool, t.root.id, Some(page[1].id), 50)
            .await
            .unwrap()
            .len(),
        2
    );

    tenants::delete(&mut pool.acquire().await.unwrap(), t.dev.id)
        .await
        .unwrap();
    assert_eq!(
        audit::list(&pool, t.acme.id, None, 50).await.unwrap().len(),
        2,
        "history survives deletion"
    );
}

#[sqlx::test(migrator = "hm_db::MIGRATOR")]
async fn tenant_contents_and_subtree_listing(pool: PgPool) {
    let t = tree(&pool).await;
    assert_eq!(
        tenants::contents(&pool, t.acme.id).await.unwrap(),
        vec![("sub-tenants".to_owned(), 1)]
    );
    assert!(tenants::contents(&pool, t.dev.id).await.unwrap().is_empty());
    let names: Vec<_> = tenants::in_subtrees(&pool, &[t.acme.id, t.globex.id])
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.slug)
        .collect();
    assert_eq!(names, ["acme", "globex", "dev"]);
}

#[sqlx::test(migrator = "hm_db::MIGRATOR")]
async fn storage_pools_attach_to_clusters(pool: PgPool) {
    tree(&pool).await;
    let c1 = hm_db::clusters::create(&pool, "c1", 4.0, 1.0)
        .await
        .unwrap();
    let c2 = hm_db::clusters::create(&pool, "c2", 4.0, 1.0)
        .await
        .unwrap();
    let local = hm_db::storage_pools::get(&pool, hm_core::ids::LOCAL_POOL)
        .await
        .unwrap();
    assert_eq!((local.kind.as_str(), local.clusters.len()), ("local", 0));

    let mut tx = pool.begin().await.unwrap();
    let cfg = serde_json::json!({"server": "10.77.0.10", "export": "/srv/nfs/hypermon"});
    let id = hm_db::storage_pools::create(&mut tx, "nfs1", "nfs", &cfg, &[c1.id])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert!(
        hm_db::storage_pools::create(&mut tx, "nfs1", "nfs", &cfg, &[])
            .await
            .is_err(),
        "duplicate name"
    );
    tx.rollback().await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    hm_db::storage_pools::update(&mut tx, id, "nfs1", &cfg, &[c1.id, c2.id])
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    let err =
        hm_db::storage_pools::update(&mut tx, id, "nfs1", &cfg, &[hm_core::ids::ClusterId::new()])
            .await
            .unwrap_err();
    assert!(err.to_string().contains("unknown cluster"), "{err}");
    tx.rollback().await.unwrap();

    let p = hm_db::storage_pools::get(&pool, id).await.unwrap();
    assert_eq!(p.clusters, vec![c1.id]);
    assert_eq!(
        hm_db::storage_pools::volume_count(&pool, id).await.unwrap(),
        0
    );
    let mut tx = pool.begin().await.unwrap();
    hm_db::storage_pools::delete(&mut tx, id).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        hm_db::storage_pools::list(&pool).await.unwrap().len(),
        1,
        "only local is left"
    );
}
