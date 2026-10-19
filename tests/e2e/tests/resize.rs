//! Resizing: a running VM's disk grows and the guest sees it at once; a
//! stopped VM gets more CPUs and memory, and its file system grows at boot.
//! Disks never shrink, CPUs and memory change only while stopped, and only
//! within what the host has. On local disk, and on Ceph when set up.

use std::time::Duration;

use mst_e2e::{Api, Console};
use reqwest::Method;
use serde_json::{Value, json};

const BOOT: Duration = Duration::from_secs(5 * 60);
const PASSWORD: &str = "e2e-password";
const GIB: u64 = 1 << 30;

#[tokio::test]
async fn resize_cpus_memory_and_disks() -> anyhow::Result<()> {
    let Some(api) = mst_e2e::connect().await? else {
        return Ok(());
    };
    let run = mst_e2e::run_name("resize");
    // Local disk, and Ceph when the storage tests have set it up.
    let pools = api.get("/storage-pools").await?;
    let rbd = pools
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["name"] == "e2e-rbd")
        .map(|p| p["id"].as_str().unwrap().to_owned());
    if rbd.is_none() {
        eprintln!("no e2e-rbd pool; skipping Ceph (run the storage tests first)");
    }
    let variants: Vec<(&str, Option<String>)> = [("local", None)]
        .into_iter()
        .chain(rbd.map(|id| ("rbd", Some(id))))
        .collect();
    mst_e2e::cleaned_up(&api, &run, async {
        let results = futures::future::join_all(
            variants
                .iter()
                .map(|(label, pool)| variant(&api, &run, label, pool.as_deref())),
        )
        .await;
        for ((label, _), r) in variants.iter().zip(results) {
            r.map_err(|e| e.context(format!("on {label} storage")))?;
        }
        Ok(())
    })
    .await
}

async fn variant(api: &Api, run: &str, label: &str, pool: Option<&str>) -> anyhow::Result<()> {
    let image = api.image().await?;
    let vm = api
        .post(
            "/vms",
            json!({"name": format!("{run}-{label}"), "vcpus": 1, "memory_mib": 768, "disk_gib": 4,
                   "image_id": image, "password": PASSWORD, "storage_pool_id": pool}),
        )
        .await?;
    let id = vm["id"].as_str().unwrap().to_owned();
    api.wait_task(vm["task_id"].as_str().unwrap(), BOOT).await?;
    scenario(api, &id, pool.is_some()).await
}

async fn sh(api: &Api, vm: &str, cmd: &str) -> anyhow::Result<String> {
    let mut serial: Console = api.console(vm, "serial").await?;
    let (out, code) = serial.run("debian", PASSWORD, cmd).await?;
    anyhow::ensure!(code == 0, "{cmd} failed ({code}): {out}");
    Ok(out)
}

/// The number a shell expression prints. The marker is split by quotes in
/// the echoed command line, so only the output has it whole.
async fn number(api: &Api, vm: &str, expr: &str) -> anyhow::Result<u64> {
    let out = sh(api, vm, &format!("echo \"N\"\"=$({expr})\"")).await?;
    out.rsplit_once("N=")
        .and_then(|(_, rest)| {
            rest.trim_start()
                .split(|c: char| !c.is_ascii_digit())
                .next()?
                .parse()
                .ok()
        })
        .ok_or_else(|| anyhow::anyhow!("no number in {out:?}"))
}

async fn resize(api: &Api, vm: &str, gib: u64) -> anyhow::Result<(u16, Value)> {
    api.try_send(
        Method::POST,
        &format!("/vms/{vm}/disks/0/resize"),
        Some(json!({"size_gib": gib})),
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

async fn scenario(api: &Api, vm: &str, ceph: bool) -> anyhow::Result<()> {
    api.wait_vm(vm, BOOT, |v| v["power"] == "running").await?;

    // Running: the disk grows under the guest.
    let (st, v) = resize(api, vm, 6).await?;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["disks"][0]["size_bytes"], 6 * GIB, "{v}");
    let size = number(api, vm, "lsblk -bdno SIZE /dev/vda").await?;
    assert_eq!(size, 6 * GIB, "the guest sees the new size");
    let (st, _) = resize(api, vm, 5).await?;
    assert_eq!(st, 400, "disks do not shrink");

    // CPUs and memory only while stopped.
    let (st, _) = api
        .try_send(
            Method::PATCH,
            &format!("/vms/{vm}"),
            Some(json!({"vcpus": 2})),
        )
        .await?;
    assert_eq!(st, 409, "running");
    power(api, vm, "stop?force=true", "stopped").await?;
    // Stopped, it keeps nothing on its host: any size goes, and its host
    // needs room for it as it starts.
    let (st, body) = api
        .try_send(
            Method::PATCH,
            &format!("/vms/{vm}"),
            Some(json!({"vcpus": 128})),
        )
        .await?;
    assert_eq!(st, 200, "{body}");
    let (st, body) = api
        .try_send(Method::POST, &format!("/vms/{vm}/start"), Some(json!({})))
        .await?;
    assert_eq!(st, 409, "no host has 128 vCPUs to spare: {body}");
    let (st, v) = api
        .try_send(
            Method::PATCH,
            &format!("/vms/{vm}"),
            // What it may grow to stays as it was unless given.
            Some(json!({"vcpus": 2, "max_vcpus": 2, "memory_mib": 1536})),
        )
        .await?;
    assert_eq!(st, 200, "{v}");
    assert_eq!(
        (v["vcpus"].as_u64(), v["memory_mib"].as_u64()),
        (Some(2), Some(1536))
    );

    // Stopped: the volume itself grows; snapshots in a qcow2 disk prevent
    // that (Ceph's do not).
    let s = api.post(&format!("/vms/{vm}/snapshots"), json!({})).await?;
    api.wait_task(s["task_id"].as_str().unwrap(), BOOT).await?;
    if !ceph {
        let (st, body) = resize(api, vm, 7).await?;
        assert_eq!(st, 409, "{body}");
    }
    let (_, t) = api
        .try_send(
            Method::DELETE,
            &format!("/snapshots/{}", s["id"].as_str().unwrap()),
            None,
        )
        .await?;
    api.wait_task(t["task_id"].as_str().unwrap(), BOOT).await?;
    let (st, v) = resize(api, vm, 7).await?;
    assert_eq!(st, 200, "{v}");

    power(api, vm, "start", "running").await?;
    assert_eq!(number(api, vm, "nproc").await?, 2);
    let mem_kib = number(api, vm, "awk '/MemTotal/ {print $2}' /proc/meminfo").await?;
    assert!(mem_kib > 1200 * 1024, "MemTotal {mem_kib} KiB");
    assert_eq!(number(api, vm, "lsblk -bdno SIZE /dev/vda").await?, 7 * GIB);
    // cloud-init grew the root file system at boot.
    let root = number(api, vm, "df -B1 --output=size / | tail -n 1").await?;
    assert!(root > 6 * GIB, "root file system {root} bytes");
    Ok(())
}
