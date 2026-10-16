//! M5 end to end: hardening.

use std::time::Duration;

use reqwest::Method;
use serde_json::json;

/// A tenant's image URL cannot make a host fetch from the management
/// network (and hand the response back as a disk).
#[tokio::test]
async fn tenant_images_are_not_fetched_from_internal_addresses() -> anyhow::Result<()> {
    let Some(admin) = hm_e2e::connect().await? else {
        return Ok(());
    };
    let run = format!(
        "e2e{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs()
    );
    let root = admin.get("/auth/me").await?["tenant_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let t = admin
        .post(
            "/tenants",
            json!({"parent_id": root, "slug": format!("{run}-ssrf"), "name": "SSRF"}),
        )
        .await?["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let result = async {
        let clusters: Vec<_> = admin.get("/clusters").await?.as_array().unwrap().iter().map(|c| c["id"].clone()).collect();
        admin
            .send(Method::PUT, &format!("/tenants/{t}/grants"), Some(json!({"clusters": clusters, "storage_pools": []})))
            .await?;
        // The controller's own API, on the management network.
        let img = admin
            .post(
                "/images",
                json!({"tenant_id": t, "name": "internal", "source_url": "http://10.77.0.1:18080/healthz", "format": "raw"}),
            )
            .await?;
        let vm = admin
            .post(
                "/vms",
                json!({"tenant_id": t, "name": "ssrf", "vcpus": 1, "memory_mib": 256, "disk_gib": 1,
                       "image_id": img["id"], "migratable": false}),
            )
            .await?;
        let id = vm["id"].as_str().unwrap();
        let vm = admin.wait_vm(id, Duration::from_secs(120), |v| v["status"] == "error").await?;
        let err = vm["error"].as_str().unwrap_or("");
        assert!(err.contains("not a public address"), "{err}");
        let (_, task) = admin.try_send(Method::DELETE, &format!("/vms/{id}"), None).await?;
        admin.wait_task(task["task_id"].as_str().unwrap(), Duration::from_secs(60)).await?;
        anyhow::Ok(())
    }
    .await;
    if let Ok(images) = admin.get(&format!("/images?tenant_id={t}")).await {
        for i in images
            .as_array()
            .into_iter()
            .flatten()
            .filter(|i| i["tenant_id"] == t.as_str())
        {
            let _ = admin
                .try_send(
                    Method::DELETE,
                    &format!("/images/{}", i["id"].as_str().unwrap()),
                    None,
                )
                .await;
        }
    }
    let (st, body) = admin
        .try_send(Method::DELETE, &format!("/tenants/{t}"), None)
        .await?;
    result?;
    assert_eq!(st, 204, "{body}");
    Ok(())
}

/// With two controller replicas (`HM_E2E_URL2`), each serves every host:
/// consoles and VM operations work through either, whichever holds the
/// host's agent session.
#[tokio::test]
async fn every_replica_reaches_every_host() -> anyhow::Result<()> {
    let Some(a) = hm_e2e::connect().await? else {
        return Ok(());
    };
    let Ok(url2) = std::env::var("HM_E2E_URL2") else {
        eprintln!("HM_E2E_URL2 not set; skipping");
        return Ok(());
    };
    let password = std::env::var("HM_E2E_PASSWORD")?;
    let b = hm_e2e::Api::login(&url2, "admin", &password, None).await?;
    // Both see the same sessions.
    let hosts_a = a.get("/hosts").await?;
    let hosts_b = b.get("/hosts").await?;
    for h in hosts_a.as_array().unwrap() {
        let other = hosts_b
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == h["id"])
            .unwrap();
        assert_eq!(h["controller"], other["controller"]);
    }
    let image = a.image().await?;
    let name = format!(
        "e2e-{:x}-ha",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs()
    );
    let vm = a
        .post(
            "/vms",
            json!({"name": name, "vcpus": 1, "memory_mib": 512, "disk_gib": 3, "image_id": image,
                   "migratable": false}),
        )
        .await?;
    let id = vm["id"].as_str().unwrap().to_owned();
    let result = async {
        a.wait_vm(&id, Duration::from_secs(120), |v| {
            v["status"] == "ready" && v["power"] == "running"
        })
        .await?;
        for api in [&a, &b] {
            let mut c = api.console(&id, "vnc").await?;
            c.expect("RFB 003.", Duration::from_secs(10)).await?;
        }
        // Operations through the other replica.
        let t = b
            .post(&format!("/vms/{id}/stop?force=true"), json!({}))
            .await?;
        b.wait_task(t["task_id"].as_str().unwrap(), Duration::from_secs(60))
            .await?;
        anyhow::Ok(())
    }
    .await;
    let (_, t) = a
        .try_send(Method::DELETE, &format!("/vms/{id}"), None)
        .await?;
    if let Some(t) = t["task_id"].as_str() {
        a.wait_task(t, Duration::from_secs(120)).await?;
    }
    result
}
