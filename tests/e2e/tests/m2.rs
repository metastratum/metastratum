//! M2 end to end: nested tenants, inherited roles, hierarchical quotas, the
//! audit log and API tokens, through the public API.

use std::time::Duration;

use hm_e2e::Api;
use reqwest::Method;
use serde_json::{Value, json};

const PASSWORD: &str = "e2e-password-1";

#[derive(Clone)]
struct World {
    a: String,
    a1: String,
    b: String,
}

async fn status(api: &Api, method: Method, path: &str, body: Option<Value>) -> u16 {
    api.try_send(method, path, body).await.unwrap().0
}

async fn create_vm(
    api: &Api,
    tenant: &str,
    image: &str,
    name: &str,
) -> anyhow::Result<(u16, Value)> {
    api.try_send(
        Method::POST,
        "/vms",
        Some(json!({"tenant_id": tenant, "name": name, "vcpus": 1, "memory_mib": 512, "disk_gib": 3, "image_id": image})),
    )
    .await
}

#[tokio::test]
async fn nested_tenancy_roles_quotas_audit_and_tokens() -> anyhow::Result<()> {
    let Some(admin) = hm_e2e::connect().await? else {
        return Ok(());
    };
    let base = admin.base.clone();
    let run = format!(
        "e2e{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs()
    );
    let me = admin.get("/auth/me").await?;
    let root = me["tenant_id"].as_str().unwrap().to_owned();

    let mk = |parent: String, slug: String| {
        let admin = &admin;
        async move {
            let t = admin
                .post(
                    "/tenants",
                    json!({"parent_id": parent, "slug": slug, "name": slug}),
                )
                .await?;
            anyhow::Ok(t["id"].as_str().unwrap().to_owned())
        }
    };
    let a = mk(root.clone(), format!("{run}-a")).await?;
    let a1 = mk(a.clone(), "a1".into()).await?;
    let b = mk(root.clone(), format!("{run}-b")).await?;
    let w = World { a, a1, b };

    // Run in a task so a failed assertion still reaches the cleanup below.
    let result = tokio::spawn({
        let (admin, base, run, w) = (admin.clone(), base.clone(), run.clone(), w.clone());
        async move { scenario(&admin, &base, &run, &w).await }
    })
    .await;

    // Clean up: VMs first (tenants must be empty), then tenants inside out.
    for t in [&w.a1, &w.a, &w.b] {
        for vm in admin
            .get(&format!("/vms?tenant_id={t}"))
            .await?
            .as_array()
            .cloned()
            .unwrap_or_default()
        {
            let (_, task) = admin
                .try_send(
                    Method::DELETE,
                    &format!("/vms/{}", vm["id"].as_str().unwrap()),
                    None,
                )
                .await?;
            if let Some(id) = task["task_id"].as_str() {
                let _ = admin.wait_task(id, Duration::from_secs(120)).await;
            }
        }
    }
    for t in [&w.a1, &w.a, &w.b] {
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

async fn scenario(admin: &Api, base: &str, run: &str, w: &World) -> anyhow::Result<()> {
    let image = admin.image().await?;
    // The image belongs to the provider; share it so tenants can use it.
    let img = admin
        .get("/images")
        .await?
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == image.as_str())
        .cloned()
        .unwrap();
    assert_eq!(
        img["shared"], true,
        "the e2e image must be shared with tenants"
    );

    let user = |tenant: &str, name: &str, role: &str| json!({"tenant_id": tenant, "username": name, "display_name": name, "password": PASSWORD, "role": role});
    admin.post("/users", user(&w.a, "alice", "admin")).await?;
    admin
        .post("/users", user(&w.a1, "otto", "operator"))
        .await?;
    admin.post("/users", user(&w.b, "bob", "operator")).await?;
    let alice = Api::login(base, "alice", PASSWORD, Some(&format!("{run}-a"))).await?;
    let otto = Api::login(base, "otto", PASSWORD, Some(&format!("{run}-a/a1"))).await?;
    let bob = Api::login(base, "bob", PASSWORD, Some(&format!("{run}-b"))).await?;

    // Tenant visibility follows role bindings.
    let names = |v: Value| {
        v.as_array()
            .unwrap()
            .iter()
            .map(|t| t["slug_path"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names(alice.get("/tenants").await?),
        [format!("{run}-a"), format!("{run}-a/a1")]
    );
    assert_eq!(names(bob.get("/tenants").await?), [format!("{run}-b")]);

    // A quota on A covers A1 too. Only the provider (A's parent) may set it.
    assert_eq!(
        status(
            &alice,
            Method::PUT,
            &format!("/tenants/{}/quotas", w.a),
            Some(json!({"vcpus": 100}))
        )
        .await,
        403
    );
    admin
        .send(
            Method::PUT,
            &format!("/tenants/{}/quotas", w.a),
            Some(json!({"vcpus": 2})),
        )
        .await?;
    // A's admin manages A1's quota, within A's.
    assert_eq!(
        status(
            &alice,
            Method::PUT,
            &format!("/tenants/{}/quotas", w.a1),
            Some(json!({"vcpus": 3}))
        )
        .await,
        400
    );
    alice
        .send(
            Method::PUT,
            &format!("/tenants/{}/quotas", w.a1),
            Some(json!({"vcpus": 2})),
        )
        .await?;

    let (st, vm1) = create_vm(&otto, &w.a1, &image, "otto-1").await?;
    assert_eq!(st, 202, "{vm1}");
    let (st, vm2) = create_vm(&alice, &w.a, &image, "alice-1").await?;
    assert_eq!(st, 202, "{vm2}");
    let (st, over) = create_vm(&otto, &w.a1, &image, "otto-2").await?;
    assert_eq!(
        (st, over["error"].as_str()),
        (409, Some("quota_exceeded")),
        "{over}"
    );
    let q = alice.get(&format!("/tenants/{}/quotas", w.a)).await?;
    assert_eq!(
        (q["usage"]["vms"].as_i64(), q["usage"]["vcpus"].as_i64()),
        (Some(2), Some(2)),
        "{q}"
    );

    // Otto (A1) cannot see A's VMs; A's VM even looks nonexistent to him.
    let alice_vm = vm2["id"].as_str().unwrap();
    assert_eq!(
        status(&otto, Method::GET, &format!("/vms?tenant_id={}", w.a), None).await,
        403
    );
    assert_eq!(
        status(&otto, Method::GET, &format!("/vms/{alice_vm}"), None).await,
        404
    );
    assert_eq!(
        status(
            &bob,
            Method::GET,
            &format!("/vms/{}", vm1["id"].as_str().unwrap()),
            None
        )
        .await,
        404
    );
    // Alice (A) sees and manages A1's VMs and users.
    let a_vms = alice
        .get(&format!("/vms?tenant_id={}&descendants=true", w.a))
        .await?;
    assert_eq!(a_vms.as_array().unwrap().len(), 2);
    alice
        .post(
            &format!("/vms/{}/stop?force=true", vm1["id"].as_str().unwrap()),
            json!({}),
        )
        .await?;
    alice
        .post(
            "/users",
            json!({"tenant_id": w.a1, "username": "olga", "password": PASSWORD, "role": "viewer"}),
        )
        .await?;
    assert_eq!(
        status(
            &otto,
            Method::POST,
            "/users",
            Some(json!({"tenant_id": w.a1, "username": "x", "password": PASSWORD}))
        )
        .await,
        403
    );
    // Infrastructure stays with the provider.
    assert_eq!(status(&alice, Method::GET, "/hosts", None).await, 403);

    // The audit log records it all, for those allowed to read it.
    let audit = alice.get(&format!("/audit?tenant_id={}", w.a)).await?;
    let actions: Vec<_> = audit
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["action"].as_str().unwrap().to_owned())
        .collect();
    for want in ["vm.create", "vm.stop", "user.create", "tenant.quotas"] {
        assert!(
            actions.iter().any(|a| a == want),
            "{want} missing from {actions:?}"
        );
    }
    assert_eq!(
        status(
            &bob,
            Method::GET,
            &format!("/audit?tenant_id={}", w.a),
            None
        )
        .await,
        403
    );
    assert_eq!(
        status(
            &otto,
            Method::GET,
            &format!("/audit?tenant_id={}", w.a1),
            None
        )
        .await,
        403,
        "operators cannot read the audit log"
    );

    // API tokens act as their user until deleted.
    let tok = otto.post("/tokens", json!({"name": "e2e"})).await?;
    let bot = Api::with_token(base, tok["token"].as_str().unwrap())?;
    assert_eq!(
        bot.get(&format!("/vms?tenant_id={}", w.a1))
            .await?
            .as_array()
            .unwrap()
            .len(),
        1
    );
    otto.send(
        Method::DELETE,
        &format!("/tokens/{}", tok["id"].as_str().unwrap()),
        None,
    )
    .await?;
    assert_eq!(status(&bot, Method::GET, "/vms", None).await, 401);

    // A tenant with VMs cannot be deleted.
    assert_eq!(
        status(admin, Method::DELETE, &format!("/tenants/{}", w.a1), None).await,
        409
    );
    Ok(())
}
