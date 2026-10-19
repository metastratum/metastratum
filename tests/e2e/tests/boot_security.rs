//! Secure Boot and a TPM: a VM boots with Secure Boot and has a TPM whose
//! state is its own and goes where it goes: kept across a live migration
//! (and as it starts on another host: see the capacity suite), new once
//! taken away and given again. Secure Boot switched off applies from the
//! next start.

use std::time::Duration;

use mst_e2e::{Api, Console};
use reqwest::Method;
use serde_json::{Value, json};

const BOOT: Duration = Duration::from_secs(5 * 60);
const PW: &str = "e2e-password";

#[tokio::test]
async fn secure_boot_and_tpm_follow_the_vm() -> anyhow::Result<()> {
    let Some(api) = mst_e2e::connect().await? else {
        return Ok(());
    };
    let hosts = api.get("/hosts").await?;
    if !hosts
        .as_array()
        .into_iter()
        .flatten()
        .any(|h| h["connected"] == true && h["tpm"] == true && h["secure_boot"] == true)
    {
        eprintln!("no host gives guests a TPM and Secure Boot; skipping");
        return Ok(());
    }
    let run = mst_e2e::run_name("boot-security");
    mst_e2e::cleaned_up(&api, &run, scenario(&api, &run)).await
}

async fn console(api: &Api, vm: &str) -> anyhow::Result<Console> {
    api.console(vm, "serial").await
}

/// Whether the guest's kernel saw Secure Boot on.
async fn secure_boot(api: &Api, vm: &str) -> anyhow::Result<bool> {
    let (out, code) = console(api, vm)
        .await?
        .run("debian", PW, "sudo dmesg | grep -o 'Secure boot [a-z]*'")
        .await?;
    anyhow::ensure!(code == 0, "no word on Secure Boot: {out}");
    Ok(out.contains("Secure boot enabled"))
}

async fn power(api: &Api, vm: &str, action: &str, want: &str) -> anyhow::Result<()> {
    let t = api.post(&format!("/vms/{vm}/{action}"), json!({})).await?;
    api.wait_task(t["task_id"].as_str().unwrap(), BOOT).await?;
    api.wait_vm(vm, BOOT, |v| v["status"] == "ready" && v["power"] == want)
        .await?;
    Ok(())
}

async fn patch(api: &Api, vm: &str, body: Value) -> anyhow::Result<Value> {
    let (st, v) = api
        .try_send(Method::PATCH, &format!("/vms/{vm}"), Some(body))
        .await?;
    anyhow::ensure!(st == 200, "PATCH: {st} {v}");
    Ok(v)
}

async fn scenario(api: &Api, run: &str) -> anyhow::Result<()> {
    let image = api.image().await?;
    let body = |name: &str, extra: Value| {
        let mut b = json!({"name": format!("{run}-{name}"), "vcpus": 1, "memory_mib": 768,
                           "disk_gib": 4, "image_id": image, "password": PW,
                           "secure_boot": true, "tpm": true});
        b.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        b
    };
    // Secure Boot is UEFI's.
    let (st, v) = api
        .try_send(
            Method::POST,
            "/vms",
            Some(body("bios", json!({"firmware": "bios"}))),
        )
        .await?;
    assert_eq!(st, 400, "{v}");

    let vm = api.post("/vms", body("vm", json!({}))).await?;
    let id = vm["id"].as_str().unwrap().to_owned();
    assert_eq!(
        (&vm["secure_boot"], &vm["tpm"]),
        (&json!(true), &json!(true))
    );
    api.wait_task(vm["task_id"].as_str().unwrap(), BOOT).await?;
    api.wait_vm(&id, BOOT, |v| v["power"] == "running").await?;

    assert!(secure_boot(api, &id).await?, "booted with Secure Boot");
    let mut serial = console(api, &id).await?;
    assert!(!serial.tpm_owner_auth_set("debian", PW).await?, "a new TPM");
    serial.tpm_set_owner_auth("debian", PW).await?;
    assert!(serial.tpm_owner_auth_set("debian", PW).await?);
    // What its first boot set up (sudo's rules too) is on disk before it is
    // switched off hard.
    let (out, code) = serial
        .run("debian", PW, "cloud-init status --wait >/dev/null; sync")
        .await?;
    anyhow::ensure!(code == 0, "{out}");
    drop(serial);

    // Live: the state goes along.
    let (st, t) = api
        .try_send(Method::POST, &format!("/vms/{id}/migrate"), Some(json!({})))
        .await?;
    if st == 202 {
        let from = api.get(&format!("/vms/{id}")).await?["host_id"].clone();
        api.wait_task(t["task_id"].as_str().unwrap(), BOOT).await?;
        let v = api.get(&format!("/vms/{id}")).await?;
        assert_ne!(v["host_id"], from, "migrated: {v}");
        assert!(
            console(api, &id)
                .await?
                .tpm_owner_auth_set("debian", PW)
                .await?,
            "the TPM kept its state on {}",
            v["host_name"]
        );
    } else {
        eprintln!("no host to migrate to ({st} {t}); skipping the migration");
    }

    // Only while stopped.
    let (st, v) = api
        .try_send(
            Method::PATCH,
            &format!("/vms/{id}"),
            Some(json!({"tpm": false})),
        )
        .await?;
    assert_eq!(st, 409, "{v}");
    power(api, &id, "stop?force=true", "stopped").await?;
    let v = patch(api, &id, json!({"secure_boot": false, "tpm": false})).await?;
    assert_eq!(
        (&v["secure_boot"], &v["tpm"]),
        (&json!(false), &json!(false))
    );
    // Given again, the TPM is a new one.
    let v = patch(api, &id, json!({"tpm": true})).await?;
    assert_eq!(v["tpm"], true);
    power(api, &id, "start", "running").await?;
    assert!(!secure_boot(api, &id).await?, "Secure Boot is off");
    assert!(
        !console(api, &id)
            .await?
            .tpm_owner_auth_set("debian", PW)
            .await?,
        "the TPM taken away was gone for good"
    );
    Ok(())
}
