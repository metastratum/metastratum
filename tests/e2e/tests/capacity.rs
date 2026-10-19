//! Stopped VMs keep nothing on their host: starting one needs room there
//! again. One on shared storage whose host has none starts on another host
//! of its cluster; one on its host's own storage cannot.
//!
//! It fills hosts on purpose, so deploy/lab/e2e.sh runs it on its own,
//! before the others.

use std::time::Duration;

use mst_e2e::Api;
use reqwest::Method;
use serde_json::{Value, json};

const PW: &str = "e2e-password";
const BOOT: Duration = Duration::from_secs(600);

#[tokio::test]
async fn stopped_vms_start_where_there_is_room() -> anyhow::Result<()> {
    let Some(api) = mst_e2e::connect().await? else {
        return Ok(());
    };
    let pools = api.get("/storage-pools").await?;
    let Some(nfs) = pools
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["name"] == "e2e-nfs" && !p["cluster_ids"].as_array().is_none_or(Vec::is_empty))
        .cloned()
    else {
        eprintln!("no e2e-nfs pool; skipping (run the storage tests first)");
        return Ok(());
    };
    let run = mst_e2e::run_name("capacity");
    mst_e2e::cleaned_up(&api, &run, scenario(&api, &run, &nfs)).await
}

async fn scenario(api: &Api, run: &str, nfs: &Value) -> anyhow::Result<()> {
    let image = api.image().await?;
    let create = |name: &str, pool: &Value| {
        api.post(
            "/vms",
            json!({"name": format!("{run}-{name}"), "vcpus": 1, "memory_mib": 512, "disk_gib": 4,
                   "image_id": image, "password": PW, "storage_pool_id": pool}),
        )
    };
    let local = create("local", &Value::Null).await?;
    let shared = create("shared", &nfs["id"]).await?;
    let (local, shared) = (id(&local), id(&shared));
    for vm in [&local, &shared] {
        api.wait_vm(vm, BOOT, |v| v["power"] == "running").await?;
    }

    // On local storage, too large for any host once stopped: it may be made
    // so (it keeps nothing), but not started.
    power(api, &local, "stop?force=true", "stopped").await?;
    let largest = api
        .get("/hosts")
        .await?
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|h| h["memory_mib"].as_u64())
        .max()
        .unwrap();
    let (st, v) = resize(api, &local, largest + 1024).await?;
    assert_eq!(st, 200, "{v}");
    let (st, body) = api
        .try_send(
            Method::POST,
            &format!("/vms/{local}/start"),
            Some(json!({})),
        )
        .await?;
    assert_eq!(st, 409, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("own storage"),
        "{body}"
    );
    let (st, v) = resize(api, &local, 512).await?;
    assert_eq!(st, 200, "{v}");
    power(api, &local, "start", "running").await?;

    // On shared storage, with its host too full for it: it starts on
    // another host.
    let at = api.get(&format!("/vms/{shared}")).await?["host_id"].clone();
    let local_at = api.get(&format!("/vms/{local}")).await?["host_id"].clone();
    if local_at != at {
        // The other VM moves in, to fill the host.
        let t = api
            .post(&format!("/vms/{local}/migrate"), json!({"host_id": at}))
            .await?;
        api.wait_task(t["task_id"].as_str().unwrap(), BOOT).await?;
    }
    let hosts = api.get("/hosts").await?;
    let host = hosts
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["id"] == at)
        .unwrap()
        .clone();
    // All its host has, but what the other VM takes, and a bit more: as
    // much as it may grow to while it runs (what is kept for it), not what
    // it boots with.
    let big = host["memory_mib"].as_u64().unwrap() - 256;
    power(api, &shared, "stop?force=true", "stopped").await?;
    let (st, v) = resize(api, &shared, big).await?;
    assert_eq!(st, 200, "{v}");
    let others_room = hosts
        .as_array()
        .unwrap()
        .iter()
        .filter(|h| h["id"] != at && h["connected"] == true)
        .any(|h| {
            h["memory_mib"].as_u64().unwrap() >= big + h["allocated_memory_mib"].as_u64().unwrap()
        });
    if !others_room {
        eprintln!("no other host has {big} MiB to spare; skipping the move");
        return Ok(());
    }
    power(api, &shared, "start", "running").await?;
    let v = api.get(&format!("/vms/{shared}")).await?;
    assert_ne!(v["host_id"], at, "started on another host: {v}");
    let mut serial = api.console(&shared, "serial").await?;
    let (out, rc) = serial.run("debian", PW, "true").await?;
    assert_eq!(rc, 0, "it runs there: {out}");
    Ok(())
}

fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_owned()
}

/// Lets the VM grow to `max_memory_mib` while it runs (512 MiB to boot).
async fn resize(api: &Api, vm: &str, max_memory_mib: u64) -> anyhow::Result<(u16, Value)> {
    api.try_send(
        Method::PATCH,
        &format!("/vms/{vm}"),
        Some(json!({"memory_mib": 512, "max_memory_mib": max_memory_mib})),
    )
    .await
}

async fn power(api: &Api, vm: &str, action: &str, want: &str) -> anyhow::Result<()> {
    let t = api.post(&format!("/vms/{vm}/{action}"), json!({})).await?;
    api.wait_task(t["task_id"].as_str().unwrap(), BOOT).await?;
    api.wait_vm(vm, BOOT, |v| v["status"] == "ready" && v["power"] == want)
        .await?;
    Ok(())
}
