//! Disks of their own: made empty, attached to a running VM (the guest sees
//! it at once, under its ID), detached and attached again with what they
//! hold, grown, moved along when the VM migrates, kept when the VM goes,
//! and attached to another VM; on local storage and on NFS.

use std::time::{Duration, Instant};

use mst_e2e::{Api, Console};
use reqwest::Method;
use serde_json::{Value, json};

const BOOT: Duration = Duration::from_secs(5 * 60);
const PASSWORD: &str = "e2e-password";
const GIB: u64 = 1 << 30;

#[tokio::test]
async fn disks_attach_detach_and_outlive_their_vm() -> anyhow::Result<()> {
    let Some(api) = mst_e2e::connect().await? else {
        return Ok(());
    };
    let run = mst_e2e::run_name("disks");
    mst_e2e::cleaned_up(&api, &run, scenario(&api, &run)).await
}

async fn sh(api: &Api, vm: &str, cmd: &str) -> anyhow::Result<String> {
    let mut serial: Console = api.console(vm, "serial").await?;
    let (out, code) = serial.run("debian", PASSWORD, cmd).await?;
    anyhow::ensure!(code == 0, "{cmd} failed ({code}): {out}");
    Ok(out)
}

/// The word or number a shell expression prints. The marker is split by
/// quotes in the echoed command line, so only the output has it whole.
async fn value(api: &Api, vm: &str, expr: &str) -> anyhow::Result<String> {
    let out = sh(api, vm, &format!("echo \"V\"\"=$({expr})\"")).await?;
    out.rsplit_once("V=")
        .and_then(|(_, rest)| rest.split_whitespace().next().map(str::to_owned))
        .ok_or_else(|| anyhow::anyhow!("no value in {out:?}"))
}

async fn vm(api: &Api, name: &str) -> anyhow::Result<String> {
    let image = api.image().await?;
    let vm = api
        .post(
            "/vms",
            json!({"name": name, "vcpus": 1, "memory_mib": 512, "disk_gib": 4, "image_id": image,
                   "password": PASSWORD, "migratable": true}),
        )
        .await?;
    let id = vm["id"].as_str().unwrap().to_owned();
    api.wait_task(vm["task_id"].as_str().unwrap(), BOOT).await?;
    api.wait_vm(&id, BOOT, |v| {
        v["status"] == "ready" && v["power"] == "running"
    })
    .await?;
    Ok(id)
}

async fn disk(api: &Api, id: &str) -> anyhow::Result<Value> {
    api.get(&format!("/disks/{id}")).await
}

/// Whether the guest has a block device at `path`.
async fn present(api: &Api, vm: &str, path: &str) -> anyhow::Result<bool> {
    Ok(value(api, vm, &format!("test -b {path} && echo yes || echo no")).await? == "yes")
}

/// Waits for the task an answer names, if it names one.
async fn settle(api: &Api, answer: &Value) -> anyhow::Result<()> {
    if let Some(t) = answer["task_id"].as_str() {
        api.wait_task(t, BOOT).await?;
    }
    Ok(())
}

/// Waits until the disk is attached to the VM (a move attaches it with a
/// task of its own once there).
async fn attached(api: &Api, id: &str, vm: &str) -> anyhow::Result<Value> {
    let deadline = Instant::now() + BOOT;
    loop {
        let d = disk(api, id).await?;
        if d["state"] == "ready" && d["vm_id"] == vm {
            return Ok(d);
        }
        anyhow::ensure!(
            d["state"] != "ready" || d["error"].is_null(),
            "not attached: {d}"
        );
        anyhow::ensure!(Instant::now() < deadline, "not attached in time: {d}");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn attach(api: &Api, vm: &str, id: &str) -> anyhow::Result<Value> {
    let (st, d) = api
        .try_send(
            Method::POST,
            &format!("/vms/{vm}/disks"),
            Some(json!({"disk_id": id})),
        )
        .await?;
    anyhow::ensure!(st == 202, "attaching: {st} {d}");
    settle(api, &d).await?;
    attached(api, id, vm).await
}

async fn detach(api: &Api, vm: &str, id: &str) -> anyhow::Result<Value> {
    let (st, d) = api
        .try_send(Method::DELETE, &format!("/vms/{vm}/disks/{id}"), None)
        .await?;
    anyhow::ensure!(st == 202, "detaching: {st} {d}");
    settle(api, &d).await?;
    disk(api, id).await
}

/// Moves the disk (a task) and returns it where it went.
async fn relocate(api: &Api, id: &str, to: Value) -> anyhow::Result<Value> {
    let (st, m) = api
        .try_send(Method::POST, &format!("/disks/{id}/move"), Some(to))
        .await?;
    anyhow::ensure!(st == 202, "moving: {st} {m}");
    settle(api, &m).await?;
    let d = disk(api, id).await?;
    anyhow::ensure!(d["state"] == "ready" && d["error"].is_null(), "moved: {d}");
    Ok(d)
}

async fn scenario(api: &Api, run: &str) -> anyhow::Result<()> {
    let a = vm(api, &format!("{run}-a")).await?;

    // Made and plugged into the running VM.
    let (st, d) = api
        .try_send(
            Method::POST,
            "/disks",
            Some(json!({"name": format!("{run}-data"), "size_gib": 2, "vm_id": a})),
        )
        .await?;
    assert_eq!(st, 201, "{d}");
    let data = d["id"].as_str().unwrap().to_owned();
    settle(api, &d).await?;
    let d = attached(api, &data, &a).await?;
    let path = d["guest_path"]
        .as_str()
        .expect("where the guest finds it")
        .to_owned();
    assert_eq!(d["boot"], false, "{d}");
    assert!(present(api, &a, &path).await?, "the guest sees {path}");
    sh(
        api,
        &a,
        &format!(
            "sudo mkfs.ext4 -q {path} && sudo mkdir -p /mnt/d && sudo mount {path} /mnt/d && \
             echo kept | sudo tee /mnt/d/marker && sudo umount /mnt/d"
        ),
    )
    .await?;
    let marker = |p: &str| {
        format!(
            "sudo mkdir -p /mnt/d && sudo mount {p} /mnt/d && cat /mnt/d/marker && sudo umount /mnt/d"
        )
    };

    // Not deleted while attached; the boot disk stays with its VM.
    let (st, b) = api
        .try_send(Method::DELETE, &format!("/disks/{data}"), None)
        .await?;
    assert_eq!(st, 409, "{b}");
    let disks = api.get(&format!("/vms/{a}/disks")).await?;
    let boot = disks[0]["id"].as_str().unwrap().to_owned();
    assert_eq!(disks[0]["boot"], true, "{disks}");
    let (st, b) = api
        .try_send(Method::DELETE, &format!("/vms/{a}/disks/{boot}"), None)
        .await?;
    assert_eq!(st, 409, "{b}");

    // Unplugged while it runs, and plugged in again with what it holds.
    let d = detach(api, &a, &data).await?;
    assert!(d["vm_id"].is_null() && d["error"].is_null(), "{d}");
    assert!(!present(api, &a, &path).await?, "gone from the guest");
    attach(api, &a, &data).await?;
    assert_eq!(value(api, &a, &marker(&path)).await?, "kept");

    // Grown under the running guest.
    let (st, d) = api
        .try_send(
            Method::PATCH,
            &format!("/disks/{data}"),
            Some(json!({"size_gib": 3})),
        )
        .await?;
    assert_eq!((st, d["size_gib"].as_u64()), (200, Some(3)), "{d}");
    let bytes: u64 = value(api, &a, &format!("sudo blockdev --getsize64 {path}"))
        .await?
        .parse()?;
    assert_eq!(bytes, 3 * GIB, "the guest sees it grown");

    // A VM with snapshots keeps the disks it has.
    let s = api.post(&format!("/vms/{a}/snapshots"), json!({})).await?;
    api.wait_task(s["task_id"].as_str().unwrap(), BOOT).await?;
    let (st, b) = api
        .try_send(Method::DELETE, &format!("/vms/{a}/disks/{data}"), None)
        .await?;
    assert_eq!(st, 409, "{b}");
    let (_, t) = api
        .try_send(
            Method::DELETE,
            &format!("/snapshots/{}", s["id"].as_str().unwrap()),
            None,
        )
        .await?;
    api.wait_task(t["task_id"].as_str().unwrap(), BOOT).await?;

    // Migrating, the VM takes the disk along (copied, on local storage).
    let before = disk(api, &data).await?;
    let t = api.post(&format!("/vms/{a}/migrate"), json!({})).await?;
    api.wait_task(t["task_id"].as_str().unwrap(), BOOT).await?;
    let v = api.get(&format!("/vms/{a}")).await?;
    let after = disk(api, &data).await?;
    assert_ne!(after["host_id"], before["host_id"], "{after}");
    assert_eq!(after["host_id"], v["host_id"], "with its VM: {after}");
    assert_eq!(value(api, &a, &marker(&path)).await?, "kept");

    // On NFS (when the storage tests made a pool): attached to a stopped
    // VM, it is there from its next start.
    let pools = api.get("/storage-pools").await?;
    let shared_pool = |kind: &str| {
        pools
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["kind"] == kind && !p["cluster_ids"].as_array().unwrap().is_empty())
            .map(|p| p["id"].clone())
    };
    let (nfs, rbd) = (shared_pool("nfs"), shared_pool("rbd"));
    // One storage under two names would make a moved disk its own copy:
    // refused.
    if let Some(p) = pools
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["kind"] == "nfs")
    {
        let (st, b) = api
            .try_send(
                Method::POST,
                "/storage-pools",
                Some(json!({"name": format!("{run}-alias"), "kind": "nfs",
                            "config": {"server": p["config"]["server"],
                                       "export": format!("{}/", p["config"]["export"].as_str().unwrap())},
                            "cluster_ids": []})),
            )
            .await?;
        assert_eq!(st, 409, "a second pool on {}: {b}", p["name"]);
    }
    let shared = match &nfs {
        Some(nfs) => {
            let (st, d) = api
                .try_send(
                    Method::POST,
                    "/disks",
                    Some(json!({"name": format!("{run}-shared"), "size_gib": 1, "storage_pool_id": nfs})),
                )
                .await?;
            assert_eq!((st, d["vm_id"].is_null()), (201, true), "{d}");
            let shared = d["id"].as_str().unwrap().to_owned();
            // Shut down, not powered off: what the guest wrote is on its disks.
            let t = api.post(&format!("/vms/{a}/stop"), json!({})).await?;
            api.wait_task(t["task_id"].as_str().unwrap(), BOOT).await?;
            api.wait_vm(&a, BOOT, |v| {
                v["status"] == "ready" && v["power"] == "stopped"
            })
            .await?;
            let d = attach(api, &a, &shared).await?;
            let p = d["guest_path"].as_str().unwrap().to_owned();
            let t = api.post(&format!("/vms/{a}/start"), json!({})).await?;
            api.wait_task(t["task_id"].as_str().unwrap(), BOOT).await?;
            api.wait_vm(&a, BOOT, |v| v["power"] == "running").await?;
            sh(
                api,
                &a,
                &format!(
                    "sudo mkfs.ext4 -q {p} && sudo mount {p} /mnt/d && \
                     echo shared | sudo tee /mnt/d/marker && sudo umount /mnt/d && sync"
                ),
            )
            .await?;
            Some(shared)
        }
        None => {
            eprintln!("no NFS pool (run the storage tests first): no shared disk");
            None
        }
    };

    // The VM goes; its disks of their own stay, attached to nothing.
    let (_, t) = api
        .try_send(Method::DELETE, &format!("/vms/{a}"), None)
        .await?;
    api.wait_task(t["task_id"].as_str().unwrap(), BOOT).await?;
    for id in std::iter::once(&data).chain(shared.as_ref()) {
        let d = disk(api, id).await?;
        assert!(d["vm_id"].is_null() && d["state"] == "ready", "{d}");
    }
    let (st, _) = api
        .try_send(Method::GET, &format!("/disks/{boot}"), None)
        .await?;
    assert_eq!(st, 404, "the boot disk went with it");

    // Moved to another host, copied there with what it holds.
    let here = disk(api, &data).await?["host_id"].clone();
    let hosts = mst_e2e::in_service(&api.get("/hosts").await?);
    let other = hosts
        .iter()
        .find(|h| h["id"] != here)
        .expect("another host")["id"]
        .clone();
    let d = relocate(api, &data, json!({"host_id": other})).await?;
    assert_eq!(d["host_id"], other, "{d}");

    // To another VM: moved where it is, and attached there.
    let b = vm(api, &format!("{run}-b")).await?;
    let (st, m) = api
        .try_send(
            Method::POST,
            &format!("/disks/{data}/move"),
            Some(json!({"vm_id": b})),
        )
        .await?;
    match st {
        202 => {
            settle(api, &m).await?;
            attached(api, &data, &b).await?;
        }
        // On its host already: attached as it is.
        400 => {
            attach(api, &b, &data).await?;
        }
        _ => panic!("moving to {b}: {st} {m}"),
    }
    let d = disk(api, &data).await?;
    let vb = api.get(&format!("/vms/{b}")).await?;
    assert_eq!(
        (d["vm_id"].as_str(), &d["host_id"]),
        (Some(b.as_str()), &vb["host_id"]),
        "{d}"
    );
    assert_eq!(value(api, &b, &marker(&path)).await?, "kept");

    if let (Some(shared), Some(nfs)) = (&shared, &nfs) {
        // Attached to a VM on any host.
        let d = attach(api, &b, shared).await?;
        assert_eq!(
            value(api, &b, &marker(d["guest_path"].as_str().unwrap())).await?,
            "shared"
        );

        // Into another pool: from the host's own storage to NFS.
        detach(api, &b, &data).await?;
        let d = relocate(api, &data, json!({"storage_pool_id": nfs})).await?;
        assert_eq!(
            (d["storage_pool_kind"].as_str(), d["host_id"].is_null()),
            (Some("nfs"), true),
            "{d}"
        );
        attach(api, &b, &data).await?;
        assert_eq!(value(api, &b, &marker(&path)).await?, "kept");
    }
    if let Some(rbd) = &rbd {
        // Into Ceph and back to the host: copied straight in and out.
        detach(api, &b, &data).await?;
        let d = relocate(api, &data, json!({"storage_pool_id": rbd})).await?;
        assert_eq!(d["storage_pool_kind"], "rbd", "{d}");
        attach(api, &b, &data).await?;
        assert_eq!(value(api, &b, &marker(&path)).await?, "kept");
        detach(api, &b, &data).await?;
        let local = disks[0]["storage_pool_id"].clone();
        let d = relocate(
            api,
            &data,
            json!({"storage_pool_id": local, "host_id": vb["host_id"]}),
        )
        .await?;
        assert_eq!(d["storage_pool_kind"], "local", "{d}");
        attach(api, &b, &data).await?;
        assert_eq!(value(api, &b, &marker(&path)).await?, "kept");
    }

    // Cloned, each disk is copied in its own pool (the clone's copies go
    // with it).
    if shared.is_some() {
        let t = api.post(&format!("/vms/{b}/stop"), json!({})).await?;
        api.wait_task(t["task_id"].as_str().unwrap(), BOOT).await?;
        api.wait_vm(&b, BOOT, |v| {
            v["status"] == "ready" && v["power"] == "stopped"
        })
        .await?;
        let pools_of = |disks: &Value| {
            disks
                .as_array()
                .unwrap()
                .iter()
                .map(|d| d["storage_pool_id"].clone())
                .collect::<Vec<_>>()
        };
        let original = pools_of(&api.get(&format!("/vms/{b}/disks")).await?);
        let c = api
            .post(
                &format!("/vms/{b}/clone"),
                json!({"name": format!("{run}-c"), "password": PASSWORD, "start": false}),
            )
            .await?;
        let cid = c["id"].as_str().unwrap().to_owned();
        api.wait_task(c["task_id"].as_str().unwrap(), BOOT).await?;
        assert_eq!(
            pools_of(&api.get(&format!("/vms/{cid}/disks")).await?),
            original
        );
        let (_, t) = api
            .try_send(Method::DELETE, &format!("/vms/{cid}"), None)
            .await?;
        api.wait_task(t["task_id"].as_str().unwrap(), BOOT).await?;
    }

    // Deleted with the VM when asked.
    let (_, t) = api
        .try_send(Method::DELETE, &format!("/vms/{b}?delete_disks=true"), None)
        .await?;
    api.wait_task(t["task_id"].as_str().unwrap(), BOOT).await?;
    for id in std::iter::once(&data).chain(shared.as_ref()) {
        let (st, _) = api
            .try_send(Method::GET, &format!("/disks/{id}"), None)
            .await?;
        assert_eq!(st, 404, "deleted with its VM");
    }
    Ok(())
}
