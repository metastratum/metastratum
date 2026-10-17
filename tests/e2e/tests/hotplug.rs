//! CPU hotplug: a VM made able to grow beyond the virtual CPUs it starts
//! with gets more, or fewer, while it runs, plugged in and out, and the
//! guest uses them at once; past its maximum, and the maximum itself, only
//! while it is stopped.

use std::time::{Duration, Instant};

use mst_e2e::{Api, Console};
use reqwest::Method;
use serde_json::{Value, json};

const BOOT: Duration = Duration::from_secs(5 * 60);
const PASSWORD: &str = "e2e-password";

#[tokio::test]
async fn cpus_plug_in_and_out_while_the_vm_runs() -> anyhow::Result<()> {
    let Some(api) = mst_e2e::connect().await? else {
        return Ok(());
    };
    let name = format!(
        "e2e-hotplug-{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs()
    );
    let image = api.image().await?;
    let vm = api
        .post(
            "/vms",
            json!({"name": name, "vcpus": 1, "max_vcpus": 4, "memory_mib": 768,
                   "disk_gib": 4, "image_id": image, "password": PASSWORD}),
        )
        .await?;
    let id = vm["id"].as_str().unwrap().to_owned();
    mst_e2e::cleaned_up(&api, &name, async {
        assert_eq!(
            (vm["vcpus"].as_u64(), vm["max_vcpus"].as_u64()),
            (Some(1), Some(4)),
            "{vm}"
        );
        api.wait_task(vm["task_id"].as_str().unwrap(), BOOT).await?;
        scenario(&api, &id).await
    })
    .await
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

/// The CPUs the guest has online, once there are `want`.
async fn online_cpus(api: &Api, vm: &str, want: u64) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let n: u64 = value(api, vm, "nproc").await?.parse()?;
        if n == want {
            return Ok(());
        }
        if Instant::now() > deadline {
            anyhow::bail!("the guest has {n} CPUs online, not {want}");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

async fn patch(api: &Api, vm: &str, body: Value) -> anyhow::Result<(u16, Value)> {
    api.try_send(Method::PATCH, &format!("/vms/{vm}"), Some(body))
        .await
}

async fn scenario(api: &Api, vm: &str) -> anyhow::Result<()> {
    api.wait_vm(vm, BOOT, |v| v["power"] == "running").await?;
    online_cpus(api, vm, 1).await?;
    let boot_id = value(api, vm, "cat /proc/sys/kernel/random/boot_id").await?;

    // More, then fewer, while it runs.
    let (st, v) = patch(api, vm, json!({"vcpus": 3})).await?;
    assert_eq!((st, v["vcpus"].as_u64()), (200, Some(3)), "{v}");
    online_cpus(api, vm, 3).await?;
    let (st, v) = patch(api, vm, json!({"vcpus": 2})).await?;
    assert_eq!((st, v["vcpus"].as_u64()), (200, Some(2)), "{v}");
    online_cpus(api, vm, 2).await?;

    // A migration keeps them.
    let t = api.post(&format!("/vms/{vm}/migrate"), json!({})).await?;
    api.wait_task(t["task_id"].as_str().unwrap(), BOOT).await?;
    online_cpus(api, vm, 2).await?;

    // All of them, and down to one: the VM stays laid out for plugging.
    let (st, v) = patch(api, vm, json!({"vcpus": 4})).await?;
    assert_eq!((st, v["cpu_hotplug"].as_bool()), (200, Some(true)), "{v}");
    online_cpus(api, vm, 4).await?;
    let (st, v) = patch(api, vm, json!({"vcpus": 1})).await?;
    assert_eq!((st, v["max_vcpus"].as_u64()), (200, Some(4)), "{v}");
    online_cpus(api, vm, 1).await?;

    // Beyond the maximum, or the maximum itself: only while stopped.
    for (body, why) in [
        (json!({"vcpus": 5}), "beyond the maximum"),
        (json!({"max_vcpus": 8}), "the maximum itself"),
    ] {
        let (st, b) = patch(api, vm, body).await?;
        assert_eq!(st, 409, "{why}: {b}");
    }
    assert_eq!(
        value(api, vm, "cat /proc/sys/kernel/random/boot_id").await?,
        boot_id,
        "the same boot all the while"
    );

    // Stopped, CPUs set past the maximum take it along.
    let t = api
        .post(&format!("/vms/{vm}/stop?force=true"), json!({}))
        .await?;
    api.wait_task(t["task_id"].as_str().unwrap(), BOOT).await?;
    api.wait_vm(vm, BOOT, |v| {
        v["status"] == "ready" && v["power"] == "stopped"
    })
    .await?;
    let (st, v) = patch(api, vm, json!({"vcpus": 6})).await?;
    assert_eq!(st, 200, "{v}");
    assert_eq!(
        (v["vcpus"].as_u64(), v["max_vcpus"].as_u64()),
        (Some(6), Some(6))
    );
    Ok(())
}
