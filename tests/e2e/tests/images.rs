//! Images: downloaded as they are added (or uploaded, in parts), onto one
//! host, and copied from there to the hosts VMs are made on. VMs' disks are
//! copies of their own: they migrate without the image, and keep working
//! once it is deleted.

use std::time::Duration;

use mst_e2e::Api;
use reqwest::Method;
use serde_json::{Value, json};

const PW: &str = "e2e-password";

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

#[tokio::test]
async fn images_are_copied_to_hosts_and_disks_stand_alone() -> anyhow::Result<()> {
    let Some(api) = mst_e2e::connect().await? else {
        return Ok(());
    };
    let run = mst_e2e::run_name("images");
    mst_e2e::cleaned_up(&api, &run, scenario(&api, &run)).await
}

async fn scenario(api: &Api, run: &str) -> anyhow::Result<()> {
    let url = std::env::var("MST_E2E_IMAGE_URL").unwrap_or_else(|_| {
        "https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2"
            .into()
    });
    let added = api
        .post(
            "/images",
            json!({"name": format!("{run}-debian"), "source_url": url, "format": "qcow2"}),
        )
        .await?;
    assert_eq!(added["state"], "downloading");
    let id = added["id"].as_str().unwrap().to_owned();

    // A VM made meanwhile waits for the download.
    let vm = api
        .post(
            "/vms",
            json!({"name": format!("{run}-a"), "vcpus": 1, "memory_mib": 768, "disk_gib": 4,
                   "image_id": id, "password": PW}),
        )
        .await?;
    let img = api.wait_image_ready(&id, Duration::from_secs(900)).await?;
    assert_eq!(img["origin"], "download", "{img}");
    assert_eq!(img["sha256"].as_str().map(str::len), Some(64), "{img}");
    assert_eq!(img["digest"].as_str().map(str::len), Some(64), "{img}");
    let virtual_bytes = img["virtual_bytes"].as_u64().unwrap();
    assert!(virtual_bytes > 1 << 30, "{img}");
    let first = img["hosts"].as_array().unwrap().clone();
    assert_eq!(first.len(), 1, "downloaded once: {img}");

    // A disk smaller than the image is refused at once.
    let (status, body) = api
        .try_send(
            Method::POST,
            "/vms",
            Some(
                json!({"name": format!("{run}-small"), "vcpus": 1, "memory_mib": 512,
                        "disk_gib": 1, "image_id": id}),
            ),
        )
        .await?;
    assert_eq!(status, 400, "{body}");

    api.wait_task(vm["task_id"].as_str().unwrap(), Duration::from_secs(600))
        .await?;
    let vm_id = vm["id"].as_str().unwrap();
    let v = api
        .wait_vm(vm_id, Duration::from_secs(120), |v| v["power"] == "running")
        .await?;
    let on = v["host_name"].as_str().unwrap().to_owned();
    // Another, which goes to another host (the least loaded): one of the
    // two is made where the image was not downloaded, which copies it.
    let other = api
        .post(
            "/vms",
            json!({"name": format!("{run}-b"), "vcpus": 1, "memory_mib": 512, "disk_gib": 4,
                   "image_id": id, "start": false}),
        )
        .await?;
    api.wait_task(other["task_id"].as_str().unwrap(), Duration::from_secs(600))
        .await?;
    let other = api
        .get(&format!("/vms/{}", other["id"].as_str().unwrap()))
        .await?;
    let img = image(api, &id).await?;
    let holders: Vec<&str> = img["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    eprintln!(
        "downloaded on {first:?}, VMs on {on} and {}; now on {holders:?}",
        other["host_name"]
    );
    for h in [on.as_str(), other["host_name"].as_str().unwrap()] {
        assert!(holders.contains(&h), "a VM's host keeps a copy: {img}");
    }

    let mut serial = api.console(vm_id, "serial").await?;
    let (out, rc) = serial
        .run("debian", PW, "echo e2e-marker > ~/marker && sync")
        .await?;
    assert_eq!(rc, 0, "{out}");
    drop(serial);

    // To a host without the image, where possible: the disk is copied
    // whole, and the image never needed there.
    let hosts = api.get("/hosts").await?;
    let candidates: Vec<&Value> = hosts
        .as_array()
        .unwrap()
        .iter()
        .filter(|h| h["state"] == "ready" && h["name"] != on.as_str())
        .collect();
    let dest = candidates
        .iter()
        .find(|h| !holders.contains(&h["name"].as_str().unwrap_or_default()))
        .or(candidates.first())
        .expect("another host to migrate to");
    let t = api
        .post(
            &format!("/vms/{vm_id}/migrate"),
            json!({"host_id": dest["id"]}),
        )
        .await?;
    api.wait_task(t["task_id"].as_str().unwrap(), Duration::from_secs(600))
        .await?;
    let v = api
        .wait_vm(vm_id, Duration::from_secs(60), |v| v["power"] == "running")
        .await?;
    assert_eq!(v["host_name"], dest["name"]);
    assert_eq!(
        image(api, &id).await?["hosts"],
        img["hosts"],
        "migrating copied the disk, not the image"
    );

    // The image goes; the VM's disk stands on its own, also through a
    // cold boot.
    let (status, body) = api
        .send(Method::DELETE, &format!("/images/{id}"), None)
        .await?;
    assert_eq!(status, 204, "{body}");
    let t = api
        .post(&format!("/vms/{vm_id}/stop?force=true"), json!({}))
        .await?;
    api.wait_task(t["task_id"].as_str().unwrap(), Duration::from_secs(60))
        .await?;
    api.wait_vm(vm_id, Duration::from_secs(30), |v| v["power"] == "stopped")
        .await?;
    let t = api.post(&format!("/vms/{vm_id}/start"), json!({})).await?;
    api.wait_task(t["task_id"].as_str().unwrap(), Duration::from_secs(60))
        .await?;
    let mut serial = api.console(vm_id, "serial").await?;
    let (out, rc) = serial.run("debian", PW, "cat ~/marker").await?;
    assert!(rc == 0 && out.contains("e2e-marker"), "{out}");
    Ok(())
}

/// An image that cannot be downloaded says why, and makes no VMs. It has
/// failed for good: its URL is gone, and nothing downloads it again.
#[tokio::test]
async fn failed_downloads_say_why_and_are_final() -> anyhow::Result<()> {
    let Some(api) = mst_e2e::connect().await? else {
        return Ok(());
    };
    let run = mst_e2e::run_name("images-failed");
    mst_e2e::cleaned_up(&api, &run, async {
        let added = api
            .post(
                "/images",
                json!({"name": format!("{run}-missing"), "format": "qcow2",
                       "source_url": "https://deb.debian.org/debian/no-such-image.qcow2"}),
            )
            .await?;
        let id = added["id"].as_str().unwrap();
        let img = api.wait_image(id, Duration::from_secs(120)).await?;
        assert_eq!(img["state"], "failed", "{img}");
        assert!(img["error"].as_str().unwrap_or("").contains("404"), "{img}");
        assert!(img.get("source_url").is_none(), "no URL is kept: {img}");

        let (status, body) = api
            .try_send(
                Method::POST,
                "/vms",
                Some(
                    json!({"name": format!("{run}-vm"), "vcpus": 1, "memory_mib": 512,
                            "disk_gib": 4, "image_id": id}),
                ),
            )
            .await?;
        assert_eq!(status, 409, "{body}");

        // There is no downloading it again.
        let (status, body) = api
            .try_send(
                Method::POST,
                &format!("/images/{id}/download"),
                Some(json!({})),
            )
            .await?;
        assert!(matches!(status, 404 | 405), "{status} {body}");
        Ok(())
    })
    .await
}

/// Uploads a file in parts of `part` bytes, from `from`.
async fn upload(
    api: &Api,
    id: &str,
    file: &[u8],
    from: usize,
    part: usize,
) -> anyhow::Result<Value> {
    let mut at = from;
    let mut last = Value::Null;
    while at < file.len() {
        let end = (at + part).min(file.len());
        let (st, v) = api
            .put_bytes(
                &format!("/images/{id}/upload?offset={at}"),
                file[at..end].to_vec(),
            )
            .await?;
        anyhow::ensure!(st == 200, "part at {at}: {st} {v}");
        at = end;
        last = v;
    }
    Ok(last)
}

/// An uploaded image: in parts (one out of turn is refused, saying where to
/// go on), then checked and stored as a download is, and the same as the
/// download of the same file. VMs boot from it. A file that is no image
/// fails as it is checked, for good.
#[tokio::test]
async fn uploaded_images_are_checked_and_boot_like_downloaded_ones() -> anyhow::Result<()> {
    let Some(api) = mst_e2e::connect().await? else {
        return Ok(());
    };
    let run = mst_e2e::run_name("images-upload");
    mst_e2e::cleaned_up(&api, &run, upload_scenario(&api, &run)).await
}

async fn upload_scenario(api: &Api, run: &str) -> anyhow::Result<()> {
    // The file the lab's test image comes from, fetched here.
    let url = std::env::var("MST_E2E_IMAGE_URL").unwrap_or_else(|_| {
        "https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2"
            .into()
    });
    let file = reqwest::get(&url)
        .await?
        .error_for_status()?
        .bytes()
        .await?
        .to_vec();
    let sha256: String = ring::digest::digest(&ring::digest::SHA256, &file)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

    // Either a URL or an upload, not both.
    let (st, v) = api
        .try_send(
            Method::POST,
            "/images",
            Some(json!({"name": format!("{run}-both"), "format": "qcow2",
                        "source_url": url, "upload_bytes": file.len()})),
        )
        .await?;
    assert_eq!(st, 400, "{v}");
    let added = api
        .post(
            "/images",
            json!({"name": format!("{run}-debian"), "format": "qcow2", "upload_bytes": file.len()}),
        )
        .await?;
    let id = added["id"].as_str().unwrap().to_owned();
    assert_eq!(
        (&added["state"], &added["received_bytes"], &added["origin"]),
        (&json!("uploading"), &json!(0), &json!("upload")),
        "{added}"
    );
    // Not before it is all there.
    let (st, v) = api
        .try_send(
            Method::POST,
            "/vms",
            Some(
                json!({"name": format!("{run}-early"), "vcpus": 1, "memory_mib": 512,
                        "disk_gib": 4, "image_id": id}),
            ),
        )
        .await?;
    assert_eq!(st, 409, "{v}");

    let part = 8 << 20;
    upload(api, &id, &file[..2 * part], 0, part).await?;
    // Out of turn (ahead, or sent again): refused, saying where to go on.
    for wrong in [3 * part, part] {
        let (st, v) = api
            .put_bytes(
                &format!("/images/{id}/upload?offset={wrong}"),
                file[wrong..wrong + 16].to_vec(),
            )
            .await?;
        assert_eq!(st, 409, "{v}");
        assert!(
            v["message"]
                .as_str()
                .unwrap_or("")
                .contains(&format!("offset {}", 2 * part)),
            "{v}"
        );
    }
    assert_eq!(image(api, &id).await?["received_bytes"], 2 * part);
    let last = upload(api, &id, &file, 2 * part, 32 << 20).await?;
    assert_eq!(last["state"], "storing", "{last}");
    let img = api.wait_image(&id, Duration::from_secs(900)).await?;
    assert_eq!(img["state"], "ready", "{img}");
    assert_eq!(img["sha256"], sha256, "the file as uploaded: {img}");
    // The same disk the download of the same file holds.
    let downloaded = image(api, &api.image().await?).await?;
    if downloaded["sha256"] == sha256 {
        assert_eq!(img["digest"], downloaded["digest"], "{img} {downloaded}");
    }

    let vm = api
        .post(
            "/vms",
            json!({"name": format!("{run}-vm"), "vcpus": 1, "memory_mib": 768, "disk_gib": 4,
                   "image_id": id, "password": PW}),
        )
        .await?;
    let vm_id = vm["id"].as_str().unwrap().to_owned();
    api.wait_task(vm["task_id"].as_str().unwrap(), Duration::from_secs(600))
        .await?;
    let mut serial = api.console(&vm_id, "serial").await?;
    let (out, code) = serial.run("debian", PW, "true").await?;
    assert_eq!(code, 0, "it boots: {out}");

    // Not an image: fails as it is checked, for good.
    let junk = vec![b'x'; 3 << 20];
    let bad = api
        .post(
            "/images",
            json!({"name": format!("{run}-junk"), "format": "qcow2", "upload_bytes": junk.len()}),
        )
        .await?;
    let bad_id = bad["id"].as_str().unwrap().to_owned();
    upload(api, &bad_id, &junk, 0, part).await?;
    let img = api.wait_image(&bad_id, Duration::from_secs(900)).await?;
    assert_eq!(img["state"], "failed", "{img}");
    let (st, v) = api
        .put_bytes(
            &format!("/images/{bad_id}/upload?offset=0"),
            junk[..16].to_vec(),
        )
        .await?;
    assert_eq!(st, 409, "not uploaded again: {v}");
    Ok(())
}

/// Images kept in a shared pool (the storage tests' NFS and Ceph pools):
/// downloaded into it, and VMs on hosts that reach it made from it there,
/// with no copy on any host, whichever pool their disks are in (local, the
/// same shared pool, or the other).
#[tokio::test]
async fn images_kept_in_shared_pools() -> anyhow::Result<()> {
    let Some(api) = mst_e2e::connect().await? else {
        return Ok(());
    };
    let run = mst_e2e::run_name("images-pools");
    mst_e2e::cleaned_up(&api, &run, async {
        let url = std::env::var("MST_E2E_IMAGE_URL").unwrap_or_else(|_| {
            "https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2"
                .into()
        });
        let pools = api.get("/storage-pools").await?;
        let shared: Vec<(&str, Value)> = ["e2e-nfs", "e2e-rbd"]
            .into_iter()
            .filter_map(|name| {
                let pool = pools.as_array().into_iter().flatten().find(|p| {
                    p["name"] == name && !p["cluster_ids"].as_array().is_none_or(Vec::is_empty)
                });
                if pool.is_none() {
                    eprintln!("no {name} pool; skipping it (run the storage tests first)");
                }
                Some((name, pool?.clone()))
            })
            .collect();
        for (name, pool) in &shared {
            let added = api
                .post(
                    "/images",
                    json!({"name": format!("{run}-{name}"), "source_url": url, "format": "qcow2",
                           "storage_pool_id": pool["id"]}),
                )
                .await?;
            let id = added["id"].as_str().unwrap().to_owned();
            assert_eq!(added["storage_pool_name"], *name);
            let img = api.wait_image_ready(&id, Duration::from_secs(900)).await?;
            assert_eq!(img["hosts"], json!([]), "kept in {name} alone: {img}");

            // On hosts that reach the pool, a disk in each pool: made from
            // the pool's copy.
            let mut targets = vec![("local", Value::Null)];
            targets.extend(shared.iter().map(|(n, p)| (*n, p["id"].clone())));
            let mut vms = vec![];
            for (target, pool_id) in &targets {
                let vm = api
                    .post(
                        "/vms",
                        json!({"name": format!("{run}-{name}-to-{target}"), "vcpus": 1,
                               "memory_mib": 512, "disk_gib": 4, "image_id": id,
                               "password": PW, "storage_pool_id": pool_id}),
                    )
                    .await?;
                vms.push((*target, vm));
            }
            for (target, vm) in &vms {
                api.wait_task(vm["task_id"].as_str().unwrap(), Duration::from_secs(600))
                    .await
                    .map_err(|e| e.context(format!("from {name} to {target}")))?;
                let vm_id = vm["id"].as_str().unwrap();
                api.wait_vm(vm_id, Duration::from_secs(120), |v| v["power"] == "running")
                    .await?;
                let mut serial = api.console(vm_id, "serial").await?;
                let (out, rc) = serial
                    .run("debian", PW, "grep -c trixie /etc/os-release")
                    .await?;
                assert_eq!(rc, 0, "from {name} to {target}: {out}");
                eprintln!("from {name} to {target}: boots");
            }
            assert_eq!(
                image(&api, &id).await?["hosts"],
                json!([]),
                "no host needed a copy"
            );

            // The pool cannot go while it keeps the image; the image can,
            // and the VMs keep their disks.
            let (status, _) = api
                .try_send(
                    Method::DELETE,
                    &format!("/storage-pools/{}", pool["id"].as_str().unwrap()),
                    None,
                )
                .await?;
            assert_eq!(status, 409);
            let (status, body) = api
                .send(Method::DELETE, &format!("/images/{id}"), None)
                .await?;
            assert_eq!(status, 204, "{body}");
            for (_, vm) in &vms {
                let vm_id = vm["id"].as_str().unwrap();
                let (_, t) = api
                    .send(Method::DELETE, &format!("/vms/{vm_id}"), None)
                    .await?;
                api.wait_task(t["task_id"].as_str().unwrap(), Duration::from_secs(120))
                    .await?;
            }
        }
        if shared.is_empty() {
            eprintln!("no shared pools; nothing tested");
        }
        Ok(())
    })
    .await
}
