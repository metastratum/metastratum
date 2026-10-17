//! CPU models: a VM that may migrate gets a model every host of its cluster
//! runs (fitted again on request); one that may not, its host's CPU as is.
//! Hosts say which VMs of their cluster their CPU cannot run (none, in a
//! lab of identical hosts).

use std::time::{Duration, Instant};

use reqwest::Method;
use serde_json::json;

const MADE: Duration = Duration::from_secs(5 * 60);

#[tokio::test]
async fn vms_get_a_model_their_cluster_runs() -> anyhow::Result<()> {
    let Some(api) = mst_e2e::connect().await? else {
        return Ok(());
    };
    let run = mst_e2e::run_name("cpu");
    mst_e2e::cleaned_up(&api, &run, async {
        let image = api.image().await?;
        let make = |name: String, migratable: bool| {
            json!({"name": name, "vcpus": 1, "memory_mib": 256, "disk_gib": 3, "image_id": image,
                   "migratable": migratable, "start": false})
        };
        let vm = api.post("/vms", make(format!("{run}-m"), true)).await?;
        let id = vm["id"].as_str().unwrap().to_owned();
        api.wait_task(vm["task_id"].as_str().unwrap(), MADE).await?;
        let v = api.get(&format!("/vms/{id}")).await?;
        assert!(v["cpu_model"].is_string(), "the cluster's model: {v}");

        // Fitted again while stopped (to the hosts as they are now).
        let (st, v) = api
            .try_send(
                Method::PATCH,
                &format!("/vms/{id}"),
                Some(json!({"fit_cpu": true})),
            )
            .await?;
        assert_eq!((st, v["cpu_model"].is_string()), (200, true), "{v}");

        // One that does not migrate has its host's CPU, and nothing to fit.
        let pinned = api.post("/vms", make(format!("{run}-p"), false)).await?;
        let pid = pinned["id"].as_str().unwrap().to_owned();
        api.wait_task(pinned["task_id"].as_str().unwrap(), MADE)
            .await?;
        let v = api.get(&format!("/vms/{pid}")).await?;
        assert!(v["cpu_model"].is_null(), "{v}");
        let (st, b) = api
            .try_send(
                Method::PATCH,
                &format!("/vms/{pid}"),
                Some(json!({"fit_cpu": true})),
            )
            .await?;
        assert_eq!(st, 400, "{b}");

        // Identical hosts run every VM of the cluster, once their CPUs
        // are known (worked out in the background).
        let deadline = Instant::now() + Duration::from_secs(150);
        loop {
            let hosts = mst_e2e::in_service(&api.get("/hosts").await?);
            if hosts.iter().all(|h| {
                h["cpu_vendor"].is_string() && h["cannot_run"].as_array().is_some_and(Vec::is_empty)
            }) {
                break;
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "hosts that cannot run VMs: {hosts:?}"
            );
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        Ok(())
    })
    .await
}
