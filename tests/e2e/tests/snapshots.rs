//! Snapshots: taken of a running and a stopped VM, reverted to while it is
//! stopped (what was written since is gone), deleted while it runs; on
//! local disk (which then cannot migrate) and, when the storage tests have
//! set them up, on NFS (which migrates, snapshots and all) and Ceph.

use std::time::Duration;

use mst_e2e::{Api, Console};
use reqwest::Method;
use serde_json::{Value, json};

const BOOT: Duration = Duration::from_secs(5 * 60);
const WORK: Duration = Duration::from_secs(5 * 60);
const PASSWORD: &str = "e2e-password";

#[tokio::test]
async fn snapshots_take_revert_and_delete() -> anyhow::Result<()> {
    let Some(api) = mst_e2e::connect().await? else {
        return Ok(());
    };
    let run = format!(
        "e2e-snap-{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs()
    );
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

    let results = futures::future::join_all(
        variants
            .iter()
            .map(|(label, pool)| scenario(&api, &run, label, pool.as_deref())),
    )
    .await;

    // Clean up: the VMs, and their snapshots with them.
    let vms = api.get("/vms").await?;
    for v in vms.as_array().into_iter().flatten() {
        if !v["name"].as_str().is_some_and(|n| n.starts_with(&run)) {
            continue;
        }
        let id = v["id"].as_str().unwrap();
        let (_, t) = api
            .try_send(Method::DELETE, &format!("/vms/{id}"), None)
            .await?;
        if let Some(task) = t["task_id"].as_str() {
            // Deleting a Ceph volume with snapshots left must work too.
            api.wait_task(task, BOOT).await?;
        }
    }
    for ((label, _), r) in variants.iter().zip(results) {
        r.map_err(|e| e.context(format!("on {label} storage")))?;
    }
    Ok(())
}

async fn sh(api: &Api, vm: &str, cmd: &str) -> anyhow::Result<String> {
    let mut serial: Console = api.console(vm, "serial").await?;
    let (out, code) = serial.run("debian", PASSWORD, cmd).await?;
    anyhow::ensure!(code == 0, "{cmd} failed ({code}): {out}");
    Ok(out)
}

async fn snapshots(api: &Api, vm: &str) -> anyhow::Result<Vec<Value>> {
    Ok(api
        .get(&format!("/vms/{vm}/snapshots"))
        .await?
        .as_array()
        .cloned()
        .unwrap_or_default())
}

/// Takes a snapshot and returns it once taken.
async fn take(api: &Api, vm: &str, name: &str) -> anyhow::Result<Value> {
    let s = api
        .post(&format!("/vms/{vm}/snapshots"), json!({"name": name}))
        .await?;
    assert_eq!(s["state"], "creating", "{s}");
    api.wait_task(s["task_id"].as_str().unwrap(), WORK).await?;
    let id = s["id"].as_str().unwrap();
    let all = snapshots(api, vm).await?;
    let s = all.iter().find(|x| x["id"] == id).cloned().unwrap();
    assert_eq!(s["state"], "ready", "{s}");
    Ok(s)
}

async fn power(api: &Api, vm: &str, action: &str, want: &str) -> anyhow::Result<()> {
    let t = api.post(&format!("/vms/{vm}/{action}"), json!({})).await?;
    api.wait_task(t["task_id"].as_str().unwrap(), BOOT).await?;
    api.wait_vm(vm, BOOT, |v| v["status"] == "ready" && v["power"] == want)
        .await?;
    Ok(())
}

async fn scenario(api: &Api, run: &str, label: &str, pool: Option<&str>) -> anyhow::Result<()> {
    let image = api.image().await?;
    let vm = api
        .post(
            "/vms",
            json!({"name": format!("{run}-{label}"), "vcpus": 1, "memory_mib": 768, "disk_gib": 4,
                   "image_id": image, "password": PASSWORD, "storage_pool_id": pool}),
        )
        .await?;
    api.wait_task(vm["task_id"].as_str().unwrap(), BOOT).await?;
    let vm = vm["id"].as_str().unwrap().to_owned();
    api.wait_vm(&vm, BOOT, |v| v["power"] == "running").await?;

    sh(api, &vm, "echo before | sudo tee /var/tmp/marker && sync").await?;

    // Taken while it runs; without a guest agent, crash-consistent.
    let first = take(api, &vm, "before").await?;
    assert_eq!(first["consistent"], false, "{first}");
    let v = api.get(&format!("/vms/{vm}")).await?;
    assert_eq!(v["power"], "running", "the snapshot left the VM running");
    let (st, _) = api
        .try_send(
            Method::POST,
            &format!("/vms/{vm}/snapshots"),
            Some(json!({"name": "before"})),
        )
        .await?;
    assert_eq!(st, 409, "names are unique per VM");

    sh(api, &vm, "echo after | sudo tee /var/tmp/marker && sync").await?;

    // Only a stopped VM is reverted.
    let first_id = first["id"].as_str().unwrap().to_owned();
    let (st, body) = api
        .try_send(Method::POST, &format!("/snapshots/{first_id}/revert"), None)
        .await?;
    assert_eq!(st, 409, "{body}");

    // Local disks would lose their snapshots in a block migration; shared
    // ones keep them.
    let (st, body) = api
        .try_send(Method::POST, &format!("/vms/{vm}/migrate"), Some(json!({})))
        .await?;
    if label == "local" {
        assert_eq!(st, 409, "{body}");
        assert!(body.to_string().contains("snapshots"), "{body}");
    } else if st == 202 {
        api.wait_task(body["task_id"].as_str().unwrap(), WORK)
            .await?;
    } else {
        eprintln!("{label}: not migrated ({st}: {body})");
    }

    // Taken while stopped: consistent.
    power(api, &vm, "stop?force=true", "stopped").await?;
    let second = take(api, &vm, "stopped").await?;
    assert_eq!(second["consistent"], true, "{second}");

    let t = api
        .post(&format!("/snapshots/{first_id}/revert"), json!({}))
        .await?;
    // No starting it meanwhile.
    let (st, _) = api
        .try_send(Method::POST, &format!("/vms/{vm}/start"), None)
        .await?;
    assert!(st == 409 || st == 202, "{st}");
    api.wait_task(t["task_id"].as_str().unwrap(), WORK).await?;
    if st == 409 {
        power(api, &vm, "start", "running").await?;
    } else {
        api.wait_vm(&vm, BOOT, |v| v["power"] == "running").await?;
    }
    let out = sh(api, &vm, "cat /var/tmp/marker").await?;
    assert!(
        out.contains("before") && !out.contains("after"),
        "reverted: {out}"
    );

    // Deleted while it runs; the other stays.
    let second_id = second["id"].as_str().unwrap();
    let (st, t) = api
        .try_send(Method::DELETE, &format!("/snapshots/{second_id}"), None)
        .await?;
    assert_eq!(st, 202, "{t}");
    api.wait_task(t["task_id"].as_str().unwrap(), WORK).await?;
    let left: Vec<String> = snapshots(api, &vm)
        .await?
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(left, ["before"]);
    Ok(())
}
