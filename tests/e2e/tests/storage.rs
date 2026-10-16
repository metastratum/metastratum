//! M2 end to end: VMs on a shared NFS pool, and pool grants.
//!
//! Uses the NFS export at HM_E2E_NFS (default the lab's
//! 10.77.0.10:/srv/nfs/hypermon), attached to the `default` cluster.

use std::time::Duration;

use hm_e2e::Api;
use reqwest::Method;
use serde_json::{Value, json};

async fn nfs_pool(admin: &Api) -> anyhow::Result<String> {
    let find = || async {
        let pools = admin.get("/storage-pools").await?;
        anyhow::Ok(
            pools
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["name"] == "e2e-nfs")
                .map(|p| p["id"].as_str().unwrap().to_owned()),
        )
    };
    if let Some(id) = find().await? {
        return Ok(id);
    }
    let nfs = std::env::var("HM_E2E_NFS").unwrap_or_else(|_| "10.77.0.10:/srv/nfs/hypermon".into());
    let (server, export) = nfs.split_once(':').expect("HM_E2E_NFS is server:/export");
    let clusters = admin.get("/clusters").await?;
    let default = clusters
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "default")
        .unwrap()["id"]
        .clone();
    let body = json!({"name": "e2e-nfs", "kind": "nfs", "config": {"server": server, "export": export}, "cluster_ids": [default]});
    // Tests run concurrently; whoever creates it, use it.
    let (status, p) = admin
        .try_send(Method::POST, "/storage-pools", Some(body))
        .await?;
    match status {
        201 => Ok(p["id"].as_str().unwrap().to_owned()),
        409 => Ok(find().await?.expect("created concurrently")),
        _ => anyhow::bail!("creating the NFS pool: HTTP {status}: {p}"),
    }
}

async fn volumes_in(admin: &Api, pool: &str) -> anyhow::Result<i64> {
    let pools = admin.get("/storage-pools").await?;
    Ok(pools
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == pool)
        .unwrap()["volumes"]
        .as_i64()
        .unwrap())
}

#[tokio::test]
async fn vm_on_nfs_pool() -> anyhow::Result<()> {
    let Some(admin) = hm_e2e::connect().await? else {
        return Ok(());
    };
    let pool = nfs_pool(&admin).await?;
    let image = admin.image().await?;
    let before = volumes_in(&admin, &pool).await?;
    let name = format!(
        "e2e-nfs-{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs()
    );

    let vm = admin
        .post(
            "/vms",
            json!({"name": name, "vcpus": 1, "memory_mib": 1024, "disk_gib": 4, "image_id": image,
                   "password": "e2e-password", "storage_pool_id": pool}),
        )
        .await?;
    let id = vm["id"].as_str().unwrap().to_owned();
    let result = async {
        assert_eq!(vm["disks"][0]["pool_id"], pool.as_str());
        admin
            .wait_task(vm["task_id"].as_str().unwrap(), Duration::from_secs(600))
            .await?;
        assert_eq!(volumes_in(&admin, &pool).await?, before + 1);
        admin
            .wait_vm(&id, Duration::from_secs(30), |v| v["power"] == "running")
            .await?;

        let mut serial = admin.console(&id, "serial").await?;
        let (out, rc) = serial
            .run("debian", "e2e-password", "lsblk -dn -o SIZE /dev/vda")
            .await?;
        assert_eq!(rc, 0, "{out}");
        assert!(out.contains("4G"), "{out}");
        anyhow::Ok(())
    }
    .await;

    let (_, t) = admin
        .send(Method::DELETE, &format!("/vms/{id}"), None)
        .await?;
    admin
        .wait_task(t["task_id"].as_str().unwrap(), Duration::from_secs(120))
        .await?;
    assert_eq!(
        volumes_in(&admin, &pool).await?,
        before,
        "the volume is removed with the VM"
    );
    result
}

#[tokio::test]
async fn shared_pools_need_a_grant() -> anyhow::Result<()> {
    let Some(admin) = hm_e2e::connect().await? else {
        return Ok(());
    };
    let pool = nfs_pool(&admin).await?;
    let image = admin.image().await?;
    let root = admin.get("/auth/me").await?["tenant_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let slug = format!(
        "e2e-s{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs()
    );
    let t = admin
        .post(
            "/tenants",
            json!({"parent_id": root, "slug": slug, "name": slug}),
        )
        .await?;
    let tenant = t["id"].as_str().unwrap().to_owned();

    let result = async {
        let usable = |v: Value| v.as_array().unwrap().iter().map(|p| p["kind"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
        assert_eq!(usable(admin.get(&format!("/tenants/{tenant}/storage-pools")).await?), ["local"]);
        let (st, body) = admin
            .try_send(
                Method::POST,
                "/vms",
                Some(json!({"tenant_id": tenant, "name": "x", "vcpus": 1, "memory_mib": 512, "disk_gib": 4,
                            "image_id": image, "storage_pool_id": pool})),
            )
            .await?;
        assert_eq!(st, 404, "{body}");
        let grant = json!({"clusters": [], "storage_pools": [pool]});
        admin.send(Method::PUT, &format!("/tenants/{tenant}/grants"), Some(grant)).await?;
        let mut kinds = usable(admin.get(&format!("/tenants/{tenant}/storage-pools")).await?);
        kinds.sort();
        assert_eq!(kinds, ["local", "nfs"]);
        // Local storage needs no grant and cannot be granted.
        let local = json!({"clusters": [], "storage_pools": ["00000000-0000-0000-0000-000000000001"]});
        let (st, _) = admin.try_send(Method::PUT, &format!("/tenants/{tenant}/grants"), Some(local)).await?;
        assert_eq!(st, 400);
        anyhow::Ok(())
    }
    .await;
    admin
        .send(Method::DELETE, &format!("/tenants/{tenant}"), None)
        .await?;
    result
}
