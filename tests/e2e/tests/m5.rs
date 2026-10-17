//! M5 end to end: hardening.

use std::time::Duration;

use reqwest::Method;
use serde_json::json;

/// Held by tests that disturb agent sessions (renewal reconnects every agent,
/// fencing kills a host) or need them steady, so they never overlap.
static SESSIONS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A tenant's image URL cannot make a host fetch from the management
/// network (and hand the response back as a disk).
#[tokio::test]
async fn tenant_images_are_not_fetched_from_internal_addresses() -> anyhow::Result<()> {
    let Some(admin) = mst_e2e::connect().await? else {
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
    mst_e2e::cleaned_up(&admin, &run, async {
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
        Ok(())
    })
    .await
}

/// With two controller replicas (`MST_E2E_URL2`), each serves every host:
/// consoles and VM operations work through either, whichever holds the
/// host's agent session.
#[tokio::test]
async fn every_replica_reaches_every_host() -> anyhow::Result<()> {
    let _sessions = SESSIONS.lock().await;
    let Some(a) = mst_e2e::connect().await? else {
        return Ok(());
    };
    let Ok(url2) = std::env::var("MST_E2E_URL2") else {
        eprintln!("MST_E2E_URL2 not set; skipping");
        return Ok(());
    };
    let password = std::env::var("MST_E2E_PASSWORD")?;
    let b = mst_e2e::Api::login(&url2, "admin", &password, None).await?;
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
    mst_e2e::cleaned_up(&a, &name, async {
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
        Ok(())
    })
    .await
}

fn virsh(args: &[&str]) -> anyhow::Result<String> {
    let out = std::process::Command::new("virsh")
        .arg("-c")
        .arg("qemu:///system")
        .args(args)
        .output()?;
    anyhow::ensure!(
        out.status.success(),
        "virsh {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A host that stops responding is fenced and its VMs on shared storage
/// restart elsewhere. Destructive: freezes a lab host. Runs with
/// `MST_E2E_FENCE=1`, on the machine running the lab (hosts are fenced with
/// libvirt there).
#[tokio::test]
async fn failed_hosts_are_fenced_and_their_vms_restart_elsewhere() -> anyhow::Result<()> {
    let _sessions = SESSIONS.lock().await;
    let Some(api) = mst_e2e::connect().await? else {
        return Ok(());
    };
    if std::env::var_os("MST_E2E_FENCE").is_none() {
        eprintln!("MST_E2E_FENCE not set; skipping");
        return Ok(());
    }
    let hosts = mst_e2e::in_service(&api.get("/hosts").await?);
    let cluster = hosts[0]["cluster_id"].as_str().unwrap().to_owned();
    let c = api
        .get("/clusters")
        .await?
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == cluster.as_str())
        .unwrap()
        .clone();
    let cluster_body = |ha: bool| json!({"name": c["name"], "cpu_overcommit": c["cpu_overcommit"], "memory_overcommit": c["memory_overcommit"], "ha_enabled": ha});
    for h in &hosts {
        let id = h["id"].as_str().unwrap();
        api.send(
            Method::PUT,
            &format!("/hosts/{id}/fencing"),
            Some(json!({"kind": "libvirt", "config": {"domain": h["name"]}})),
        )
        .await?;
        let t = api
            .post(&format!("/hosts/{id}/fencing/test"), json!({}))
            .await?;
        assert_eq!(t["power"], "on", "{t}");
    }
    api.send(
        Method::PUT,
        &format!("/clusters/{cluster}"),
        Some(cluster_body(true)),
    )
    .await?;

    let image = api.image().await?;
    let pools = api.get("/storage-pools").await?;
    let nfs = pools
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["kind"] == "nfs" && !p["cluster_ids"].as_array().unwrap().is_empty());
    let name = format!(
        "e2e-{:x}-ha",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs()
    );
    let vm = api
        .post(
            "/vms",
            json!({"name": name, "vcpus": 1, "memory_mib": 512, "disk_gib": 3, "image_id": image,
                   "storage_pool_id": nfs.map(|p| p["id"].clone()), "migratable": true}),
        )
        .await?;
    let id = vm["id"].as_str().unwrap().to_owned();
    let outcome = mst_e2e::attempt(async {
        anyhow::ensure!(
            nfs.is_some(),
            "needs an NFS pool (run the storage tests first)"
        );
        api.wait_vm(&id, Duration::from_secs(120), |v| {
            v["status"] == "ready" && v["power"] == "running"
        })
        .await?;
        // Onto the least busy host, to disturb the lab least.
        let quiet = hosts
            .iter()
            .min_by_key(|h| h["vm_count"].as_u64().unwrap_or(0))
            .unwrap();
        let victim = quiet["id"].as_str().unwrap().to_owned();
        let at = api.get(&format!("/vms/{id}")).await?;
        if at["host_id"] != victim.as_str() {
            let t = api
                .post(&format!("/vms/{id}/migrate"), json!({"host_id": victim}))
                .await?;
            api.wait_task(t["task_id"].as_str().unwrap(), Duration::from_secs(300))
                .await?;
        }
        let domain = quiet["name"].as_str().unwrap().to_owned();
        eprintln!("freezing {domain}");
        virsh(&["suspend", &domain])?;
        let moved = api
            .wait_vm(&id, Duration::from_secs(600), |v| {
                v["host_id"] != victim.as_str() && v["power"] == "running"
            })
            .await;
        let host = api.get("/hosts").await?;
        let state = host
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["id"] == victim.as_str())
            .unwrap()["state"]
            .clone();
        // Bring the host back whatever happened.
        let _ = virsh(&["start", &domain]);
        let _ = virsh(&["resume", &domain]);
        let moved = moved?;
        eprintln!("restarted on {}", moved["host_name"]);
        assert_eq!(state, "fenced");
        // Once its agent is back, it can return to service.
        for _ in 0..120 {
            let h = api.get("/hosts").await?;
            if h.as_array()
                .unwrap()
                .iter()
                .any(|h| h["id"] == victim.as_str() && h["connected"] == true)
            {
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        api.send(
            Method::PATCH,
            &format!("/hosts/{victim}"),
            Some(json!({"state": "ready"})),
        )
        .await?;
        Ok(())
    })
    .await;
    let cleanup = async {
        api.send(
            Method::PUT,
            &format!("/clusters/{cluster}"),
            Some(cluster_body(false)),
        )
        .await?;
        for h in &hosts {
            let hid = h["id"].as_str().unwrap();
            let _ = api
                .send(
                    Method::PUT,
                    &format!("/hosts/{hid}/fencing"),
                    Some(json!({"kind": null})),
                )
                .await;
        }
        mst_e2e::clean(&api, &name).await
    }
    .await;
    outcome.finish(cleanup)
}

/// Hosts renew their certificates (here on request) and carry on: they
/// reconnect with the new one, and libvirt's TLS between hosts (live
/// migration) uses it.
#[tokio::test]
async fn hosts_renew_their_certificates() -> anyhow::Result<()> {
    let _sessions = SESSIONS.lock().await;
    let Some(api) = mst_e2e::connect().await? else {
        return Ok(());
    };
    let before = mst_e2e::in_service(&api.get("/hosts").await?);
    let asked = chrono::Utc::now();
    for h in &before {
        let id = h["id"].as_str().unwrap();
        api.send(
            Method::POST,
            &format!("/hosts/{id}/renew-certificate"),
            None,
        )
        .await?;
    }
    // Each comes back with a later expiry.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let now = api.get("/hosts").await?;
        let done = before.iter().all(|b| {
            now.as_array().unwrap().iter().any(|h| {
                h["id"] == b["id"]
                    && h["connected"] == true
                    && h["cert_expires_at"].as_str() > b["cert_expires_at"].as_str()
                    // On a session begun with the new certificate.
                    && h["connected_since"]
                        .as_str()
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                        .is_some_and(|t| t > asked)
            })
        });
        if done {
            break;
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "hosts did not come back renewed: {now}"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    // Migration uses the hosts' TLS certificates.
    let image = api.image().await?;
    let name = format!(
        "e2e-{:x}-renew",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs()
    );
    let vm = api
        .post(
            "/vms",
            json!({"name": name, "vcpus": 1, "memory_mib": 512, "disk_gib": 3, "image_id": image, "migratable": true}),
        )
        .await?;
    let id = vm["id"].as_str().unwrap().to_owned();
    mst_e2e::cleaned_up(&api, &name, async {
        api.wait_vm(&id, Duration::from_secs(120), |v| {
            v["status"] == "ready" && v["power"] == "running"
        })
        .await?;
        let t = api.post(&format!("/vms/{id}/migrate"), json!({})).await?;
        api.wait_task(t["task_id"].as_str().unwrap(), Duration::from_secs(300))
            .await?;
        Ok(())
    })
    .await
}
