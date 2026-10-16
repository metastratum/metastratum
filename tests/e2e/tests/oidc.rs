//! Per-tenant OIDC sign-in against Dex (deploy/dev/compose.yaml), whose mock
//! connector signs everyone in as Kilgore Trout, member of `authors`.
//!
//! Runs when HM_E2E_DEX holds Dex's issuer URL, e.g.
//! `HM_E2E_DEX=http://127.0.0.1:5556/dex`. The controller's public URL must
//! be one of the redirect URIs registered in deploy/dev/dex.yaml, and as
//! Dex is on an internal address, the controller must trust it for
//! sub-tenants (`--trusted-oidc-issuers`).

use hm_e2e::Api;
use reqwest::Method;
use serde_json::{Value, json};

/// Walks a browser through the sign-in redirects. Returns where it lands
/// and, if signed in, `/auth/me`.
async fn sign_in(
    base: &str,
    tenant: &str,
    return_to: &str,
) -> anyhow::Result<(String, Option<Value>)> {
    let http = reqwest::Client::builder()
        .cookie_store(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut url = format!("{base}/api/v1/auth/oidc/start?tenant={tenant}&return_to={return_to}");
    for _ in 0..10 {
        let resp = http.get(&url).send().await?;
        let Some(next) = resp.headers().get("location") else {
            anyhow::bail!("{url} answered {} without a redirect", resp.status());
        };
        let next = reqwest::Url::parse(&url)?.join(next.to_str()?)?.to_string();
        // Through Dex and our callback; anything else is the destination.
        if next.starts_with(&format!("{base}/api/")) || !next.starts_with(base) {
            url = next;
            continue;
        }
        let me = http.get(format!("{base}/api/v1/auth/me")).send().await?;
        let me = if me.status().is_success() {
            Some(me.json().await?)
        } else {
            None
        };
        return Ok((next.trim_start_matches(base).to_owned(), me));
    }
    anyhow::bail!("too many redirects")
}

fn roles(me: &Value) -> Vec<String> {
    let mut r: Vec<String> = me["roles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["role"].as_str().unwrap().to_owned())
        .collect();
    r.sort();
    r
}

#[tokio::test]
async fn tenant_users_sign_in_with_their_identity_provider() -> anyhow::Result<()> {
    let Ok(issuer) = std::env::var("HM_E2E_DEX") else {
        eprintln!("HM_E2E_DEX not set; skipping");
        return Ok(());
    };
    let Some(admin) = hm_e2e::connect().await? else {
        return Ok(());
    };
    let base = admin.base.clone();
    let slug = format!(
        "e2e{:x}-oidc",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs()
    );
    let root = admin.get("/auth/me").await?["tenant_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let tenant = admin
        .post(
            "/tenants",
            json!({"parent_id": root, "slug": slug, "name": "OIDC e2e"}),
        )
        .await?["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let result = tokio::spawn({
        let (admin, base, slug, tenant) =
            (admin.clone(), base.clone(), slug.clone(), tenant.clone());
        async move { scenario(&admin, &base, &issuer, &slug, &tenant).await }
    })
    .await;

    let (st, body) = admin
        .try_send(Method::DELETE, &format!("/tenants/{tenant}"), None)
        .await?;
    assert_eq!(st, 204, "deleting tenant: {body}");
    match result {
        Ok(r) => r,
        Err(panic) => std::panic::resume_unwind(panic.into_panic()),
    }
}

async fn scenario(
    admin: &Api,
    base: &str,
    issuer: &str,
    slug: &str,
    tenant: &str,
) -> anyhow::Result<()> {
    let idp = format!("/tenants/{tenant}/identity-provider");
    let config = |mappings: Value| {
        json!({
            "name": "Dex",
            "issuer": issuer,
            "client_id": "hypermon",
            "scopes": "openid profile email groups",
            "role_mappings": mappings,
        })
    };

    // Without a provider there is nothing to sign in with.
    assert_eq!(
        admin.get(&format!("/auth/methods?tenant={slug}")).await?["oidc"],
        Value::Null
    );
    let (landed, me) = sign_in(base, slug, "/").await?;
    assert!(landed.starts_with("/login?error="), "{landed}");
    assert!(me.is_none());

    // The secret is required on creation, and never shown.
    let (st, _) = admin
        .try_send(Method::PUT, &idp, Some(config(json!([]))))
        .await?;
    assert_eq!(st, 400, "a provider without a client secret");
    // Other internal addresses are off limits for a sub-tenant's provider:
    // the controller would be probing its own network for the tenant.
    for internal in ["http://127.0.0.1:5432/", "http://localhost:8080/realm"] {
        let mut c = config(json!([]));
        c["issuer"] = json!(internal);
        c["client_secret"] = json!("x");
        let (st, body) = admin.try_send(Method::PUT, &idp, Some(c)).await?;
        assert_eq!(st, 400, "{internal}: {body}");
        assert!(body.to_string().contains("public"), "{internal}: {body}");
    }
    let mut first = config(json!([{"group": "authors", "role": "viewer"}]));
    first["client_secret"] = json!("hypermon-dev-secret");
    let (st, view) = admin.try_send(Method::PUT, &idp, Some(first)).await?;
    assert_eq!(st, 200, "{view}");
    assert!(view.get("client_secret").is_none(), "{view}");
    assert!(
        view["redirect_uri"]
            .as_str()
            .unwrap()
            .ends_with("/api/v1/auth/oidc/callback")
    );
    assert_eq!(
        admin.get(&format!("/auth/methods?tenant={slug}")).await?["oidc"],
        "Dex"
    );

    // First sign-in creates the user, with the mapped role, and returns to
    // where they started. Paths off the site are not followed.
    let (landed, me) = sign_in(base, slug, "/vms").await?;
    assert_eq!(landed, "/vms");
    let me = me.expect("signed in");
    assert_eq!(me["tenant_id"], tenant);
    assert_eq!(me["display_name"], "Kilgore Trout");
    assert_eq!(roles(&me), ["viewer"]);
    let user = me["user_id"].as_str().unwrap().to_owned();
    let (landed, _) = sign_in(base, slug, "https://evil.example/").await?;
    assert_eq!(landed, "/");

    // Roles follow the mappings at each sign-in; the user stays the same.
    // An update without a secret keeps the stored one.
    admin
        .send(
            Method::PUT,
            &idp,
            Some(config(json!([{"group": "authors", "role": "operator"}]))),
        )
        .await?;
    let (_, me) = sign_in(base, slug, "/").await?;
    let me = me.expect("signed in");
    assert_eq!(me["user_id"], user.as_str());
    assert_eq!(roles(&me), ["operator"]);

    // No mapped role: refused, and the OIDC role is gone...
    admin
        .send(
            Method::PUT,
            &idp,
            Some(config(json!([{"group": "editors", "role": "admin"}]))),
        )
        .await?;
    let (landed, me) = sign_in(base, slug, "/").await?;
    assert!(landed.starts_with("/login?error="), "{landed}");
    assert!(me.is_none());
    let bindings = admin.get(&format!("/users/{user}/role-bindings")).await?;
    assert_eq!(bindings.as_array().unwrap().len(), 0, "{bindings}");

    // ...but a role granted by hand still lets them in, and survives sync.
    admin
        .post(
            "/role-bindings",
            json!({"user_id": user, "tenant_id": tenant, "role": "viewer"}),
        )
        .await?;
    let (_, me) = sign_in(base, slug, "/").await?;
    assert_eq!(roles(&me.expect("signed in")), ["viewer"]);

    // Removing the provider stops OIDC sign-in.
    admin.send(Method::DELETE, &idp, None).await?;
    let (landed, me) = sign_in(base, slug, "/").await?;
    assert!(landed.starts_with("/login?error="), "{landed}");
    assert!(me.is_none());
    Ok(())
}
