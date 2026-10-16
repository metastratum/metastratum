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
