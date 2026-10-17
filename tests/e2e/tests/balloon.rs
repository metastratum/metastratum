//! Memory ballooning: a VM made able to grow beyond the memory it starts
//! with gets more, or less, while it runs, through its balloon, and the
//! guest sees it at once; past its maximum, and anything else, only while
//! it is stopped. Its balloon reports how much the guest uses.

use std::time::{Duration, Instant};

use mst_e2e::{Api, Console};
use reqwest::Method;
use serde_json::{Value, json};

const BOOT: Duration = Duration::from_secs(5 * 60);
const PASSWORD: &str = "e2e-password";

#[tokio::test]
async fn memory_changes_while_the_vm_runs() -> anyhow::Result<()> {
    let Some(api) = mst_e2e::connect().await? else {
        return Ok(());
    };
    let name = mst_e2e::run_name("balloon");
    let image = api.image().await?;
    let vm = api
        .post(
            "/vms",
            json!({"name": name, "vcpus": 1, "memory_mib": 768, "max_memory_mib": 1536,
                   "disk_gib": 4, "image_id": image, "password": PASSWORD}),
        )
        .await?;
    let id = vm["id"].as_str().unwrap().to_owned();
    mst_e2e::cleaned_up(&api, &name, async {
        assert_eq!(
            (vm["memory_mib"].as_u64(), vm["max_memory_mib"].as_u64()),
            (Some(768), Some(1536)),
            "{vm}"
        );
        assert_eq!(vm["free_page_reporting"], true, "handed back by default");
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

async fn mem_total_mib(api: &Api, vm: &str) -> anyhow::Result<u64> {
    let kib: u64 = value(api, vm, "awk '/MemTotal/ {print $2}' /proc/meminfo")
        .await?
        .parse()?;
    Ok(kib / 1024)
}

/// The balloon's report, once `ok` holds for it.
async fn memory_until(
    api: &Api,
    vm: &str,
    within: Duration,
    what: &str,
    ok: impl Fn(&Value) -> bool,
) -> anyhow::Result<Value> {
    let deadline = Instant::now() + within;
    loop {
        let (st, m) = api
            .try_send(Method::GET, &format!("/vms/{vm}/memory"), None)
            .await?;
        if st == 200 && ok(&m) {
            return Ok(m);
        }
        if Instant::now() > deadline {
            anyhow::bail!("{what}: the balloon says {m}");
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
    // The guest's driver reports once it has booted.
    // The guest boots with its maximum; once its driver is up, the balloon
    // takes what is above its memory.
    let m = memory_until(api, vm, BOOT, "the balloon settles at 768 MiB", |m| {
        m["driver"] == true && m["actual_mib"] == 768
    })
    .await?;
    assert!(m["usable_mib"].as_u64().is_some(), "{m}");
    let boot_id = value(api, vm, "cat /proc/sys/kernel/random/boot_id").await?;
    let at_start = mem_total_mib(api, vm).await?;

    // Less, then more, while it runs.
    let (st, v) = patch(api, vm, json!({"memory_mib": 512})).await?;
    assert_eq!((st, v["memory_mib"].as_u64()), (200, Some(512)), "{v}");
    memory_until(
        api,
        vm,
        Duration::from_secs(90),
        "the balloon inflates",
        |m| m["actual_mib"] == 512,
    )
    .await?;
    let smaller = mem_total_mib(api, vm).await?;
    assert!(
        smaller + 200 < at_start,
        "the guest sees less: {at_start} MiB, then {smaller} MiB"
    );
    // A migration keeps what the balloon left the guest.
    let t = api.post(&format!("/vms/{vm}/migrate"), json!({})).await?;
    api.wait_task(t["task_id"].as_str().unwrap(), BOOT).await?;
    let m = memory_until(
        api,
        vm,
        Duration::from_secs(60),
        "after the migration",
        |m| m["driver"] == true,
    )
    .await?;
    assert_eq!(m["actual_mib"], 512, "{m}");
    assert_eq!(
        mem_total_mib(api, vm).await?,
        smaller,
        "the same memory after the migration"
    );

    let (st, v) = patch(api, vm, json!({"memory_mib": 1536})).await?;
    assert_eq!((st, v["memory_mib"].as_u64()), (200, Some(1536)), "{v}");
    memory_until(
        api,
        vm,
        Duration::from_secs(90),
        "the balloon deflates",
        |m| m["actual_mib"] == 1536,
    )
    .await?;
    let larger = mem_total_mib(api, vm).await?;
    assert!(
        larger > at_start + 600,
        "the guest sees more: {at_start} MiB, then {larger} MiB"
    );

    // Beyond the maximum, or anything else: only while stopped.
    for (body, why) in [
        (json!({"memory_mib": 2048}), "beyond the maximum"),
        (json!({"max_memory_mib": 2048}), "the maximum itself"),
        (json!({"vcpus": 2}), "CPUs"),
        (
            json!({"free_page_reporting": false}),
            "handing free memory back",
        ),
    ] {
        let (st, b) = patch(api, vm, body).await?;
        assert_eq!(st, 409, "{why}: {b}");
    }
    // All the while, the same boot.
    assert_eq!(
        value(api, vm, "cat /proc/sys/kernel/random/boot_id").await?,
        boot_id
    );
    let v = api.get(&format!("/vms/{vm}")).await?;
    assert_eq!(
        (v["power"].as_str(), v["max_memory_mib"].as_u64()),
        (Some("running"), Some(1536))
    );

    // Shut down from inside the guest, it is stopped as if through the API.
    // Stopped, the maximum changes too; memory set past it takes it along.
    sh(api, vm, "sudo systemctl poweroff --no-block").await?;
    let v = api
        .wait_vm(vm, BOOT, |v| {
            v["status"] == "ready" && v["power"] == "stopped"
        })
        .await?;
    assert_eq!(
        (v["desired_power"].as_str(), v["power_reason"].as_str()),
        (Some("stopped"), Some("shutdown")),
        "{v}"
    );
    let (st, v) = patch(api, vm, json!({"memory_mib": 2048})).await?;
    assert_eq!(st, 200, "{v}");
    assert_eq!(
        (v["memory_mib"].as_u64(), v["max_memory_mib"].as_u64()),
        (Some(2048), Some(2048))
    );

    // Keeping free memory in the guest: from the next start, which brings
    // the balloon up without reporting.
    let (st, v) = patch(api, vm, json!({"free_page_reporting": false})).await?;
    assert_eq!(
        (st, v["free_page_reporting"].as_bool()),
        (200, Some(false)),
        "{v}"
    );
    let t = api.post(&format!("/vms/{vm}/start"), json!({})).await?;
    api.wait_task(t["task_id"].as_str().unwrap(), BOOT).await?;
    api.wait_vm(vm, BOOT, |v| v["power"] == "running").await?;
    memory_until(api, vm, BOOT, "the balloon still reports", |m| {
        m["driver"] == true
    })
    .await?;
    let v = api.get(&format!("/vms/{vm}")).await?;
    assert_eq!(v["free_page_reporting"], false, "kept over the start: {v}");
    Ok(())
}
