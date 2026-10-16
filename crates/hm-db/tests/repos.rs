//! Repository tests. They need a Postgres server: `deploy/dev/compose.yaml`
//! provides one, and `DATABASE_URL` must point at it. `sqlx::test` creates a
//! fresh database per test.

use std::time::Duration;

use hm_core::task::TaskState;
use hm_db::{tasks, tenants, users};
use sqlx::PgPool;

#[sqlx::test(migrator = "hm_db::MIGRATOR")]
async fn tenant_tree_paths_and_queries(pool: PgPool) {
    let root = tenants::create(&pool, None, "provider", "Provider")
        .await
        .unwrap();
    let a = tenants::create(&pool, Some(root.id), "acme", "Acme")
        .await
        .unwrap();
    let a1 = tenants::create(&pool, Some(a.id), "dev", "Acme Dev")
        .await
        .unwrap();
    let b = tenants::create(&pool, Some(root.id), "globex", "Globex")
        .await
        .unwrap();

    assert_eq!(a1.path, root.path.child(a.id).child(a1.id));
    assert_eq!(tenants::root(&pool).await.unwrap().unwrap().id, root.id);

    let lineage: Vec<_> = tenants::lineage(&pool, a1.id)
        .await
        .unwrap()
        .iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(lineage, vec![root.id, a.id, a1.id]);

    let sub: Vec<_> = tenants::subtree(&pool, a.id)
        .await
        .unwrap()
        .iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(sub, vec![a.id, a1.id]);
    assert!(!sub.contains(&b.id));
}

#[sqlx::test(migrator = "hm_db::MIGRATOR")]
async fn tenant_constraints(pool: PgPool) {
    let root = tenants::create(&pool, None, "provider", "Provider")
        .await
        .unwrap();
    let err = tenants::create(&pool, None, "other-root", "Second root")
        .await
        .unwrap_err();
    assert!(
        matches!(err, hm_db::Error::Domain(hm_core::Error::Conflict(_))),
        "{err:?}"
    );

    tenants::create(&pool, Some(root.id), "acme", "Acme")
        .await
        .unwrap();
    let err = tenants::create(&pool, Some(root.id), "acme", "Acme again")
        .await
        .unwrap_err();
    assert!(
        matches!(err, hm_db::Error::Domain(hm_core::Error::Conflict(_))),
        "{err:?}"
    );

    assert!(
        tenants::create(&pool, Some(root.id), "Bad Slug", "x")
            .await
            .is_err()
    );
}

#[sqlx::test(migrator = "hm_db::MIGRATOR")]
async fn users_are_scoped_to_tenants(pool: PgPool) {
    let root = tenants::create(&pool, None, "provider", "Provider")
        .await
        .unwrap();
    let a = tenants::create(&pool, Some(root.id), "acme", "Acme")
        .await
        .unwrap();
    let new = |tenant_id| users::NewUser {
        tenant_id,
        username: "alice",
        display_name: "Alice",
        email: None,
        password_hash: Some("x"),
    };
    let u = users::create(&pool, new(root.id)).await.unwrap();
    users::create(&pool, new(a.id)).await.unwrap();
    assert!(users::create(&pool, new(a.id)).await.is_err());

    let found = users::find_by_username(&pool, root.id, "alice")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.id, u.id);
}

#[sqlx::test(migrator = "hm_db::MIGRATOR")]
async fn tasks_claim_lease_and_finish(pool: PgPool) {
    let root = tenants::create(&pool, None, "provider", "Provider")
        .await
        .unwrap();
    let new = || tasks::NewTask {
        tenant_id: root.id,
        kind: "noop",
        target: None,
        params: serde_json::json!({}),
        created_by: None,
    };
    let t1 = tasks::enqueue(&pool, new()).await.unwrap();
    let t2 = tasks::enqueue(&pool, new()).await.unwrap();

    let lease = Duration::from_secs(30);
    let c1 = tasks::claim(&pool, "w1", lease).await.unwrap().unwrap();
    let c2 = tasks::claim(&pool, "w2", lease).await.unwrap().unwrap();
    assert_eq!((c1.id, c2.id), (t1.id, t2.id));
    assert!(tasks::claim(&pool, "w3", lease).await.unwrap().is_none());

    // Only the lease owner may checkpoint or finish.
    let state = serde_json::json!({"n": 1});
    assert!(
        !tasks::checkpoint(&pool, t1.id, "w2", lease, "s", &state, None)
            .await
            .unwrap()
    );
    assert!(
        tasks::checkpoint(&pool, t1.id, "w1", lease, "s", &state, Some(0.5))
            .await
            .unwrap()
    );
    assert!(tasks::finish(&pool, t1.id, "w1", Ok(())).await.unwrap());
    let done = tasks::get(&pool, t1.id).await.unwrap();
    assert_eq!(done.state, TaskState::Succeeded);
    assert_eq!(done.progress, Some(1.0));

    // An expired lease lets another worker take over.
    sqlx::query("UPDATE tasks SET lease_expires_at = now() - interval '1 second' WHERE id = $1")
        .bind(t2.id)
        .execute(&pool)
        .await
        .unwrap();
    let taken = tasks::claim(&pool, "w3", lease).await.unwrap().unwrap();
    assert_eq!((taken.id, taken.attempts), (t2.id, 2));
    assert!(
        !tasks::finish(&pool, t2.id, "w2", Err("late".into()))
            .await
            .unwrap()
    );
    assert!(
        tasks::finish(&pool, t2.id, "w3", Err("boom".into()))
            .await
            .unwrap()
    );
    let failed = tasks::get(&pool, t2.id).await.unwrap();
    assert_eq!(
        (failed.state, failed.error.as_deref()),
        (TaskState::Failed, Some("boom"))
    );
}

#[sqlx::test(migrator = "hm_db::MIGRATOR")]
async fn tasks_for_one_target_run_in_order(pool: PgPool) {
    let root = tenants::create(&pool, None, "provider", "Provider")
        .await
        .unwrap();
    let target = uuid::Uuid::now_v7();
    let new = |target: Option<uuid::Uuid>| tasks::NewTask {
        tenant_id: root.id,
        kind: "noop",
        target: target.map(|t| ("vm", t)),
        params: serde_json::json!({}),
        created_by: None,
    };
    let first = tasks::enqueue(&pool, new(Some(target))).await.unwrap();
    let second = tasks::enqueue(&pool, new(Some(target))).await.unwrap();
    let other = tasks::enqueue(&pool, new(None)).await.unwrap();

    let lease = Duration::from_secs(30);
    assert_eq!(
        tasks::claim(&pool, "w", lease).await.unwrap().unwrap().id,
        first.id
    );
    // `second` waits for `first`; untargeted work is not held up.
    assert_eq!(
        tasks::claim(&pool, "w", lease).await.unwrap().unwrap().id,
        other.id
    );
    assert!(tasks::claim(&pool, "w", lease).await.unwrap().is_none());
    tasks::finish(&pool, first.id, "w", Ok(())).await.unwrap();
    assert_eq!(
        tasks::claim(&pool, "w", lease).await.unwrap().unwrap().id,
        second.id
    );
}
