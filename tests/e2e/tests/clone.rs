//! Cloning and templates: a stopped VM made a template is never started,
//! and clones of it boot as new machines with what its disks held; on local
//! disk (the clone stays on the same host) and, when the storage tests have
//! set them up, on NFS and Ceph.

use std::time::Duration;

use mst_e2e::{Api, Console};
use reqwest::Method;
use serde_json::json;

const BOOT: Duration = Duration::from_secs(5 * 60);
const COPY: Duration = Duration::from_secs(10 * 60);
const PASSWORD: &str = "e2e-password";

#[tokio::test]
async fn templates_and_clones() -> anyhow::Result<()> {
    let Some(api) = mst_e2e::connect().await? else {
        return Ok(());
    };
    let run = mst_e2e::run_name("clone");
    let pools = api.get("/storage-pools").await?;
    let pool = |name: &str| {
        pools
            .as_array()
            .into_iter()
            .flatten()
            .find(|p| p["name"] == name && !p["cluster_ids"].as_array().is_none_or(Vec::is_empty))
            .map(|p| p["id"].as_str().unwrap().to_owned())
    };
    let mut variants = vec![("local", None)];
    for (label, name) in [("nfs", "e2e-nfs"), ("rbd", "e2e-rbd")] {
        match pool(name) {
            Some(id) => variants.push((label, Some(id))),
            None => eprintln!("no {name} pool; skipping {label} (run the storage tests first)"),
        }
    }
    mst_e2e::cleaned_up(&api, &run, async {
        let results = futures::future::join_all(
            variants
                .iter()
                .map(|(label, pool)| scenario(&api, &run, label, pool.as_deref())),
        )
        .await;
        for ((label, _), r) in variants.iter().zip(results) {
            r.map_err(|e| e.context(format!("on {label} storage")))?;
        }
        Ok(())
    })
    .await
}

async fn sh(api: &Api, vm: &str, cmd: &str) -> anyhow::Result<String> {
    let mut serial: Console = api.console(vm, "serial").await?;
    let (out, code) = serial.run("debian", PASSWORD, cmd).await?;
    anyhow::ensure!(code == 0, "{cmd} failed ({code}): {out}");
    Ok(out)
}

async fn scenario(api: &Api, run: &str, label: &str, pool: Option<&str>) -> anyhow::Result<()> {
    let image = api.image().await?;
    let src = api
        .post(
            "/vms",
            json!({"name": format!("{run}-{label}-tpl"), "vcpus": 1, "memory_mib": 768, "disk_gib": 4,
                   "image_id": image, "password": PASSWORD, "storage_pool_id": pool}),
        )
        .await?;
    api.wait_task(src["task_id"].as_str().unwrap(), BOOT)
        .await?;
    let src = src["id"].as_str().unwrap().to_owned();
    api.wait_vm(&src, BOOT, |v| v["power"] == "running").await?;
    sh(api, &src, "echo golden | sudo tee /var/tmp/marker && sync").await?;

    // Only a stopped VM is cloned, or made a template.
    let clone_req = |name: String| json!({"name": name, "password": PASSWORD});
    let (st, _) = api
        .try_send(
            Method::POST,
            &format!("/vms/{src}/clone"),
            Some(clone_req(format!("{run}-{label}-early"))),
        )
        .await?;
    assert_eq!(st, 409, "a running VM is not cloned");
    let t = api
        .post(&format!("/vms/{src}/stop?force=true"), json!({}))
        .await?;
    api.wait_task(t["task_id"].as_str().unwrap(), BOOT).await?;
    api.wait_vm(&src, BOOT, |v| v["power"] == "stopped").await?;
    let (st, v) = api
        .try_send(
            Method::PATCH,
            &format!("/vms/{src}"),
            Some(json!({"template": true})),
        )
        .await?;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["template"], true);
    let (st, _) = api
        .try_send(Method::POST, &format!("/vms/{src}/start"), None)
        .await?;
    assert_eq!(st, 409, "a template is never started");

    let c = api
        .post(
            &format!("/vms/{src}/clone"),
            clone_req(format!("{run}-{label}-1")),
        )
        .await?;
    api.wait_task(c["task_id"].as_str().unwrap(), COPY).await?;
    let clone = c["id"].as_str().unwrap().to_owned();
    let v = api
        .wait_vm(&clone, BOOT, |v| v["power"] == "running")
        .await?;
    let tpl = api.get(&format!("/vms/{src}")).await?;
    assert_eq!(v["template"], false);
    assert_eq!(v["disks"][0]["size_bytes"], tpl["disks"][0]["size_bytes"]);
    assert_ne!(v["disks"][0]["volume_id"], tpl["disks"][0]["volume_id"]);
    assert_ne!(v["nics"][0]["mac"], tpl["nics"][0]["mac"]);
    if label == "local" {
        assert_eq!(
            v["host_id"], tpl["host_id"],
            "local disks are copied on their host"
        );
    }
    // What the template's disk held, under the clone's own name.
    let out = sh(api, &clone, "cat /var/tmp/marker; hostname").await?;
    assert!(
        out.contains("golden") && out.contains(&format!("{run}-{label}-1")),
        "{out}"
    );
    // The template stays as it was.
    let tpl = api.get(&format!("/vms/{src}")).await?;
    assert_eq!(
        (tpl["power"].as_str(), tpl["template"].as_bool()),
        (Some("stopped"), Some(true))
    );
    Ok(())
}
