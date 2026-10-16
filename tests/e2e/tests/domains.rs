//! Domain names bound to tenants: signing in at one starts from its tenant.

use mst_e2e::Api;
use reqwest::Method;
use serde_json::json;

const PASSWORD: &str = "e2e-password-1";

#[tokio::test]
async fn sign_in_starts_from_the_domains_tenant() -> anyhow::Result<()> {
    let Some(admin) = mst_e2e::connect().await? else {
        return Ok(());
    };
    let run = format!(
        "e2e{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis()
    );
    let root = admin.get("/auth/me").await?["tenant_id"]
        .as_str()
        .unwrap()
        .to_owned();
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
    let acme = mk(root, run.clone()).await?;
    let dev = mk(acme.clone(), "dev".into()).await?;

    let result = tokio::spawn({
        let (admin, run, acme, dev) = (admin.clone(), run.clone(), acme.clone(), dev.clone());
        async move { scenario(&admin, &run, &acme, &dev).await }
    })
    .await;

    for t in [&dev, &acme] {
        let (st, body) = admin
            .try_send(Method::DELETE, &format!("/tenants/{t}"), None)
            .await?;
        assert_eq!(st, 204, "deleting tenant: {body}");
    }
    match result {
        Ok(r) => r,
        Err(panic) => std::panic::resume_unwind(panic.into_panic()),
    }
}

async fn sign_in(api: &Api, user: &str, tenant: Option<&str>) -> anyhow::Result<u16> {
    let body = json!({"username": user, "password": PASSWORD, "tenant": tenant});
    Ok(api
        .try_send(Method::POST, "/auth/login", Some(body))
        .await?
        .0)
}

async fn scenario(admin: &Api, run: &str, acme: &str, dev: &str) -> anyhow::Result<()> {
    let user = |tenant: &str, name: &str, role: &str| json!({"tenant_id": tenant, "username": name, "display_name": name, "password": PASSWORD, "role": role});
    admin.post("/users", user(acme, "alice", "admin")).await?;
    admin.post("/users", user(dev, "bob", "operator")).await?;
    let domain = format!("{run}.example.test");
    let at = || Api::anonymous_at(&admin.base, &domain);
    let port = reqwest::Url::parse(&admin.base)?.port().unwrap_or(80);
    // Where a tenant's people sign in, as the Sign-in tab shows it.
    let sign_in_urls = |tenant: &str| {
        let path = format!("/tenants/{tenant}/sign-in");
        async move {
            let v = admin.get(&path).await?;
            anyhow::Ok((
                v["page"].as_str().unwrap().to_owned(),
                v["redirect_uri"].as_str().unwrap().to_owned(),
            ))
        }
    };
    let base = &admin.base;
    assert_eq!(
        sign_in_urls(dev).await?,
        (
            format!("{base}/login?tenant={run}/dev"),
            format!("{base}/api/v1/auth/oidc/callback")
        )
    );

    // Not bound yet: the name leads to the provider's sign-in.
    let site = at()?.get("/auth/site").await?;
    assert_eq!(site["tenant_name"], serde_json::Value::Null, "{site}");
    assert_eq!(sign_in(&at()?, "alice", None).await?, 401);

    // Only the provider binds domains, and only plain host names.
    let alice = Api::login(&admin.base, "alice", PASSWORD, Some(run)).await?;
    let path = format!("/tenants/{acme}/domains");
    let (st, _) = alice
        .try_send(Method::POST, &path, Some(json!({"domain": domain})))
        .await?;
    assert_eq!(st, 403, "a tenant administrator bound a domain");
    for bad in ["https://x.example.test", "x.example.test:443", "x/y"] {
        let (st, _) = admin
            .try_send(Method::POST, &path, Some(json!({"domain": bad})))
            .await?;
        assert_eq!(st, 400, "{bad}");
    }
    let t = admin
        .post(
            &path,
            json!({"domain": format!("{}.", domain.to_uppercase())}),
        )
        .await?;
    assert_eq!(t["domains"], json!([domain]), "stored in lower case: {t}");
    let (st, _) = admin
        .try_send(
            Method::POST,
            &format!("/tenants/{dev}/domains"),
            Some(json!({"domain": domain})),
        )
        .await?;
    assert_eq!(st, 409, "a domain names one tenant");

    // Sign-in is at the domain now, with paths below its tenant.
    let there = format!("http://{domain}:{port}");
    assert_eq!(
        sign_in_urls(dev).await?,
        (
            format!("{there}/login?tenant=dev"),
            format!("{there}/api/v1/auth/oidc/callback")
        )
    );
    assert_eq!(sign_in_urls(acme).await?.0, format!("{there}/login"));

    // Bound: the sign-in page knows the tenant, and that it has sub-tenants.
    let site = at()?.get("/auth/site").await?;
    assert_eq!(site["tenant_name"], run, "{site}");
    assert_eq!(site["sub_tenants"], true, "{site}");
    // Paths start from the bound tenant.
    let alice_there = at()?;
    assert_eq!(sign_in(&alice_there, "alice", None).await?, 204);
    assert_eq!(alice_there.get("/auth/me").await?["tenant_id"], acme);
    assert_eq!(sign_in(&at()?, "bob", Some("dev")).await?, 204);
    assert_eq!(
        sign_in(&at()?, "bob", Some(&format!("{run}/dev"))).await?,
        401
    );
    // Nothing above it is reachable there, and elsewhere nothing changed.
    assert_eq!(sign_in(&at()?, "admin", None).await?, 401);
    let plain = Api::anonymous_at(&admin.base, "127.0.0.1")?;
    assert_eq!(sign_in(&plain, "alice", None).await?, 401);
    assert_eq!(sign_in(&plain, "alice", Some(run)).await?, 204);

    // A tenant without sub-tenants needs no tenant field.
    let dev_domain = format!("dev-{run}.example.test");
    admin
        .post(
            &format!("/tenants/{dev}/domains"),
            json!({"domain": dev_domain}),
        )
        .await?;
    let site = Api::anonymous_at(&admin.base, &dev_domain)?
        .get("/auth/site")
        .await?;
    assert_eq!(site["tenant_name"], "dev", "{site}");
    assert_eq!(site["sub_tenants"], false, "{site}");
    // The nearest domain wins.
    assert_eq!(
        sign_in_urls(dev).await?.0,
        format!("http://{dev_domain}:{port}/login")
    );

    // Unbound, the name leads to the provider again.
    let (st, _) = admin
        .try_send(Method::DELETE, &format!("{path}/{domain}"), None)
        .await?;
    assert_eq!(st, 204);
    let (st, _) = admin
        .try_send(Method::DELETE, &format!("{path}/{domain}"), None)
        .await?;
    assert_eq!(st, 404);
    assert_eq!(
        at()?.get("/auth/site").await?["tenant_name"],
        serde_json::Value::Null
    );
    assert_eq!(sign_in(&at()?, "alice", None).await?, 401);
    assert_eq!(
        sign_in_urls(acme).await?.0,
        format!("{base}/login?tenant={run}")
    );
    Ok(())
}
