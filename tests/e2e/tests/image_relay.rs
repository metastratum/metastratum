//! An image kept in a shared pool, for a VM on a host that does not reach
//! that pool: a host that does copies it into its own store and hands it
//! over from there.
//!
//! The lab's hosts all reach its pools, so this moves one host without VMs
//! into a cluster of its own for the while (deploy/lab/e2e.sh runs it on its
//! own, before the others), and back afterwards.

use std::time::Duration;

use mst_e2e::Api;
use reqwest::Method;
use serde_json::{Value, json};

const PW: &str = "e2e-password";

#[tokio::test]
async fn images_reach_hosts_outside_their_pool() -> anyhow::Result<()> {
    let Some(api) = mst_e2e::connect().await? else {
        return Ok(());
    };
    let run = mst_e2e::run_name("image-relay");
    let pools = api.get("/storage-pools").await?;
    let Some(pool) = pools
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| {
            p["name"] == "e2e-nfs" && p["cluster_ids"].as_array().is_some_and(|c| c.len() == 1)
        })
        .cloned()
    else {
        eprintln!(
            "no e2e-nfs pool attached to one cluster; skipping (run the storage tests first)"
        );
        return Ok(());
    };
    let home = pool["cluster_ids"][0].clone();

    // A host of the pool's cluster without VMs, leaving others there.
    let vms = api.get("/vms?descendants=true").await?;
    let busy: Vec<&Value> = vms
        .as_array()
        .into_iter()
        .flatten()
        .map(|v| &v["host_id"])
        .collect();
    let hosts = api.get("/hosts").await?;
    let in_home: Vec<&Value> = hosts
        .as_array()
        .unwrap()
        .iter()
        .filter(|h| h["cluster_id"] == home && h["state"] == "ready" && h["connected"] == true)
        .collect();
    let Some(host) = in_home.iter().find(|h| !busy.contains(&&h["id"])).copied() else {
        eprintln!("no host without VMs in the pool's cluster; skipping");
        return Ok(());
    };
    if in_home.len() < 2 {
        eprintln!("the pool's cluster has a single host; skipping");
        return Ok(());
    }
    let host_id = host["id"].as_str().unwrap().to_owned();
    let host_name = host["name"].as_str().unwrap().to_owned();

    let cluster = api
        .post("/clusters", json!({"name": format!("{run}-apart")}))
        .await?;
    let cluster_id = cluster["id"].as_str().unwrap().to_owned();
    let moved = api
        .send(
            Method::PATCH,
            &format!("/hosts/{host_id}"),
            Some(json!({"cluster_id": cluster_id})),
        )
        .await;
    let outcome = match moved {
        Ok(_) => {
            mst_e2e::cleaned_up(
                &api,
                &run,
                scenario(&api, &run, &pool, &cluster_id, &host_name),
            )
            .await
        }
        Err(e) => Err(e),
    };
    // Back where it was, whatever happened (its VMs are gone by now).
    let back = api
        .send(
            Method::PATCH,
            &format!("/hosts/{host_id}"),
            Some(json!({"cluster_id": home})),
        )
        .await;
    let gone = api
        .send(Method::DELETE, &format!("/clusters/{cluster_id}"), None)
        .await;
    outcome?;
    back?;
    gone?;
    Ok(())
}

async fn image(api: &Api, id: &str) -> anyhow::Result<Value> {
    let images = api.get("/images").await?;
    images
        .as_array()
        .into_iter()
        .flatten()
        .find(|i| i["id"] == id)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no image {id}"))
}

async fn scenario(
    api: &Api,
    run: &str,
    pool: &Value,
    cluster: &str,
    host: &str,
) -> anyhow::Result<()> {
    let url = std::env::var("MST_E2E_IMAGE_URL").unwrap_or_else(|_| {
        "https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2"
            .into()
    });
    let added = api
        .post(
            "/images",
            json!({"name": format!("{run}-debian"), "source_url": url, "format": "qcow2",
                   "storage_pool_id": pool["id"]}),
        )
        .await?;
    let id = added["id"].as_str().unwrap().to_owned();
    api.wait_task(added["task_id"].as_str().unwrap(), Duration::from_secs(900))
        .await?;
    assert_eq!(image(api, &id).await?["hosts"], json!([]));

    // On local disk in the cluster apart, whose one host does not reach
    // the pool.
    let vm = api
        .post(
            "/vms",
            json!({"name": format!("{run}-vm"), "vcpus": 1, "memory_mib": 512, "disk_gib": 4,
                   "image_id": id, "password": PW, "cluster_id": cluster}),
        )
        .await?;
    api.wait_task(vm["task_id"].as_str().unwrap(), Duration::from_secs(900))
        .await?;
    let vm_id = vm["id"].as_str().unwrap();
    let v = api
        .wait_vm(vm_id, Duration::from_secs(120), |v| v["power"] == "running")
        .await?;
    assert_eq!(v["host_name"], host);
    // Handed over by a host that reaches the pool, which kept a copy too.
    let img = image(api, &id).await?;
    let holders: Vec<&str> = img["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    eprintln!("now on {holders:?}");
    assert!(holders.contains(&host), "{img}");
    assert_eq!(holders.len(), 2, "relayed through one host: {img}");

    let mut serial = api.console(vm_id, "serial").await?;
    let (out, rc) = serial
        .run("debian", PW, "grep -c trixie /etc/os-release")
        .await?;
    assert_eq!(rc, 0, "{out}");
    Ok(())
}
