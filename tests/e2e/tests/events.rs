//! Live updates: changes reach the event streams of those who may see them,
//! and only those.

use std::time::Duration;

use mst_e2e::Api;
use reqwest::Method;
use serde_json::json;

const PASSWORD: &str = "e2e-password-1";

#[tokio::test]
async fn changes_reach_only_those_who_may_see_them() -> anyhow::Result<()> {
    let Some(admin) = mst_e2e::connect().await? else {
        return Ok(());
    };
    let base = admin.base.clone();
    let run = mst_e2e::run_name("events");
    let root = admin.get("/auth/me").await?["tenant_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut tenants = vec![];
    for side in ["a", "b"] {
        let t = admin
            .post(
                "/tenants",
                json!({"parent_id": root, "slug": format!("{run}-{side}"), "name": side}),
            )
            .await?;
        tenants.push(t["id"].as_str().unwrap().to_owned());
    }

    let result = tokio::spawn({
        let (admin, base, run, tenants) =
            (admin.clone(), base.clone(), run.clone(), tenants.clone());
        async move { scenario(&admin, &base, &run, &tenants[0], &tenants[1]).await }
    })
    .await;

    for t in tenants.iter().rev() {
        let (st, body) = admin
            .try_send(Method::DELETE, &format!("/tenants/{t}"), None)
            .await?;
        assert_eq!(st, 204, "deleting tenant {t}: {body}");
    }
    match result {
        Ok(r) => r,
        Err(panic) => std::panic::resume_unwind(panic.into_panic()),
    }
}

async fn scenario(admin: &Api, base: &str, run: &str, a: &str, b: &str) -> anyhow::Result<()> {
    for (t, name) in [(a, "ann"), (b, "ben")] {
        admin
            .post(
                "/users",
                json!({"tenant_id": t, "username": name, "display_name": name, "password": PASSWORD, "role": "admin"}),
            )
            .await?;
    }
    let ann = Api::login(base, "ann", PASSWORD, Some(&format!("{run}-a"))).await?;
    let ben = Api::login(base, "ben", PASSWORD, Some(&format!("{run}-b"))).await?;
    let mut ann_events = ann.events().await?;
    let mut ben_events = ben.events().await?;
    let mut admin_events = admin.events().await?;
    // Streams open by telling the client to (re)load everything.
    for s in [&mut ann_events, &mut ben_events, &mut admin_events] {
        let (name, _) = s.next(Duration::from_secs(5)).await?.expect("first event");
        assert_eq!(name, "resync");
    }

    // A change inside A reaches A's admin and the provider's, not B's.
    let sub = ann
        .post(
            "/tenants",
            json!({"parent_id": a, "slug": "sub", "name": "Sub"}),
        )
        .await?;
    let sub = sub["id"].as_str().unwrap();
    let wait = Duration::from_secs(5);
    let seen = ann_events
        .wait_for("tenant", sub, wait)
        .await?
        .expect("ann sees her sub-tenant");
    assert_eq!(seen["op"], "insert");
    assert!(
        admin_events.wait_for("tenant", sub, wait).await?.is_some(),
        "the provider admin sees it"
    );
    assert!(
        ben_events
            .wait_for("tenant", sub, Duration::from_secs(2))
            .await?
            .is_none(),
        "ben must not see it"
    );

    ann.send(Method::DELETE, &format!("/tenants/{sub}"), None)
        .await?;
    let gone = ann_events
        .wait_for("tenant", sub, wait)
        .await?
        .expect("ann sees the deletion");
    assert_eq!(gone["op"], "delete");

    // A stream needs credentials.
    let anon = reqwest::get(format!("{base}/api/v1/events")).await?;
    assert_eq!(anon.status(), 401);
    Ok(())
}
