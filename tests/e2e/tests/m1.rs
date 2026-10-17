//! M1 end to end: VM lifecycle on real hosts, both consoles, and the tenant
//! network across hypervisors.

use std::time::Duration;

use mst_e2e::{Api, Console};
use serde_json::{Value, json};

const BOOT: Duration = Duration::from_secs(5 * 60);

async fn create(api: &Api, image: &str, name: &str) -> anyhow::Result<Value> {
    let vm = api
        .post(
            "/vms",
            json!({"name": name, "vcpus": 1, "memory_mib": 1024, "disk_gib": 5, "image_id": image, "password": "e2e-password"}),
        )
        .await?;
    api.wait_task(vm["task_id"].as_str().unwrap(), BOOT).await?;
    Ok(vm)
}

#[tokio::test]
async fn vm_lifecycle_consoles_and_network() -> anyhow::Result<()> {
    let Some(api) = mst_e2e::connect().await? else {
        return Ok(());
    };
    let run = mst_e2e::run_name("m1");
    mst_e2e::cleaned_up(&api, &run, scenario(&api, &run)).await
}

async fn scenario(api: &Api, run: &str) -> anyhow::Result<()> {
    let image = api.image().await?;
    let a = create(api, &image, &format!("{run}-a")).await?;
    let b = create(api, &image, &format!("{run}-b")).await?;
    let (a_id, b_id) = (a["id"].as_str().unwrap(), b["id"].as_str().unwrap());
    let b_ip = b["nics"][0]["ip"].as_str().unwrap().to_owned();
    for id in [a_id, b_id] {
        api.wait_vm(id, Duration::from_secs(30), |v| {
            v["status"] == "ready" && v["power"] == "running"
        })
        .await?;
    }
    eprintln!("VMs on {} and {}", a["host_name"], b["host_name"]);

    // Graphical console speaks RFB; tickets are single use.
    let t = api
        .post(&format!("/vms/{a_id}/console"), json!({"kind": "vnc"}))
        .await?;
    let url = api.ws_url(t["path"].as_str().unwrap());
    let mut vnc = Console::open(&url).await?;
    vnc.expect("RFB 003.", Duration::from_secs(10)).await?;
    drop(vnc);
    assert!(
        Console::open(&url).await.is_err(),
        "a used ticket must be refused"
    );

    // Serial console: log in and reach the other VM over the tenant network.
    let mut serial = api.console(a_id, "serial").await?;
    let (out, rc) = serial
        .run("debian", "e2e-password", &format!("ping -c3 -W2 {b_ip}"))
        .await?;
    assert_eq!(rc, 0, "ping {b_ip} failed:\n{out}");
    assert!(out.contains(" 0% packet loss"), "{out}");
    drop(serial);

    // Power cycle.
    let t = api
        .post(&format!("/vms/{b_id}/stop?force=true"), json!({}))
        .await?;
    api.wait_task(t["task_id"].as_str().unwrap(), Duration::from_secs(60))
        .await?;
    api.wait_vm(b_id, Duration::from_secs(30), |v| v["power"] == "stopped")
        .await?;
    let t = api.post(&format!("/vms/{b_id}/start"), json!({})).await?;
    api.wait_task(t["task_id"].as_str().unwrap(), Duration::from_secs(60))
        .await?;
    api.wait_vm(b_id, Duration::from_secs(30), |v| v["power"] == "running")
        .await?;

    for id in [a_id, b_id] {
        let (_, t) = api
            .send(reqwest::Method::DELETE, &format!("/vms/{id}"), None)
            .await?;
        api.wait_task(t["task_id"].as_str().unwrap(), Duration::from_secs(120))
            .await?;
        assert_eq!(api.status_of(&format!("/vms/{id}")).await?, 404);
    }
    Ok(())
}

#[tokio::test]
async fn unauthenticated_requests_are_rejected() -> anyhow::Result<()> {
    let Ok(base) = std::env::var("MST_E2E_URL") else {
        return Ok(());
    };
    let _ = rustls::crypto::ring::default_provider().install_default();
    let anon = reqwest::Client::new();
    for path in ["/vms", "/hosts", "/images", "/tasks"] {
        let status = anon
            .get(format!("{base}/api/v1{path}"))
            .send()
            .await?
            .status()
            .as_u16();
        assert_eq!(status, 401, "{path}");
    }
    Ok(())
}
