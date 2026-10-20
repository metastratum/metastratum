//! Sleep: a VM may sleep as its setting says (made without, it is offered
//! none; allowed while stopped, from its next start), shows as asleep once
//! the guest suspends itself, and starting it wakes it, the same boot
//! going on.

use std::time::Duration;

use mst_e2e::{Api, Console};
use reqwest::Method;
use serde_json::{Value, json};

const BOOT: Duration = Duration::from_secs(5 * 60);
const PW: &str = "e2e-password";

#[tokio::test]
async fn vms_sleep_as_told_and_wake_when_started() -> anyhow::Result<()> {
    let Some(api) = mst_e2e::connect().await? else {
        return Ok(());
    };
    let run = mst_e2e::run_name("sleep");
    mst_e2e::cleaned_up(&api, &run, scenario(&api, &run)).await
}

async fn console(api: &Api, vm: &str) -> anyhow::Result<Console> {
    api.console(vm, "serial").await
}

async fn sh(api: &Api, vm: &str, cmd: &str) -> anyhow::Result<String> {
    let (out, code) = console(api, vm).await?.run("debian", PW, cmd).await?;
    anyhow::ensure!(code == 0, "{cmd}: {out}");
    Ok(out)
}

/// Whether the guest is offered suspend to RAM.
async fn offered_sleep(api: &Api, vm: &str) -> anyhow::Result<bool> {
    Ok(sh(api, vm, "cat /sys/power/mem_sleep")
        .await?
        .contains("deep"))
}

async fn power(api: &Api, vm: &str, action: &str, body: Value, want: &str) -> anyhow::Result<()> {
    let t = api.post(&format!("/vms/{vm}/{action}"), body).await?;
    api.wait_task(t["task_id"].as_str().unwrap(), BOOT).await?;
    api.wait_vm(vm, BOOT, |v| v["status"] == "ready" && v["power"] == want)
        .await?;
    Ok(())
}

async fn scenario(api: &Api, run: &str) -> anyhow::Result<()> {
    let image = api.image().await?;
    let vm = api
        .post(
            "/vms",
            json!({"name": format!("{run}-vm"), "vcpus": 1, "memory_mib": 768,
                   "disk_gib": 4, "image_id": image, "password": PW, "sleep": false}),
        )
        .await?;
    let id = vm["id"].as_str().unwrap().to_owned();
    assert_eq!(vm["sleep"], false, "{vm}");
    api.wait_task(vm["task_id"].as_str().unwrap(), BOOT).await?;
    api.wait_vm(&id, BOOT, |v| v["power"] == "running").await?;
    assert!(!offered_sleep(api, &id).await?, "made without sleep");
    // What its first boot set up (sudo's rules too) is on disk before it is
    // switched off hard.
    sh(api, &id, "cloud-init status --wait >/dev/null; sync").await?;

    // Only while stopped.
    let (st, v) = api
        .try_send(
            Method::PATCH,
            &format!("/vms/{id}"),
            Some(json!({"sleep": true})),
        )
        .await?;
    assert_eq!(st, 409, "{v}");
    power(api, &id, "stop", json!({"force": true}), "stopped").await?;
    let (st, v) = api
        .try_send(
            Method::PATCH,
            &format!("/vms/{id}"),
            Some(json!({"sleep": true})),
        )
        .await?;
    anyhow::ensure!(st == 200 && v["sleep"] == true, "PATCH: {st} {v}");
    power(api, &id, "start", json!({}), "running").await?;
    assert!(offered_sleep(api, &id).await?, "allowed to sleep");

    // Asleep (in a moment, so the console has its answer first), then
    // woken: the same boot goes on.
    let boot = sh(api, &id, "cat /proc/sys/kernel/random/boot_id").await?;
    let boot = boot
        .split(|c: char| !(c.is_ascii_hexdigit() || c == '-'))
        .find(|w| w.len() == 36 && w.matches('-').count() == 4)
        .ok_or_else(|| anyhow::anyhow!("no boot ID in {boot}"))?
        .to_owned();
    sh(api, &id, "sudo systemd-run --on-active=3 systemctl suspend").await?;
    api.wait_vm(&id, Duration::from_secs(90), |v| v["power"] == "suspended")
        .await?;
    power(api, &id, "start", json!({}), "running").await?;
    let again = sh(api, &id, "cat /proc/sys/kernel/random/boot_id").await?;
    assert!(
        again.contains(&boot),
        "woken, not rebooted: {boot} / {again}"
    );
    Ok(())
}
