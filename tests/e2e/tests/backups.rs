//! Backups: a running VM's disk copied into a repository, restored as a new
//! VM with what it held then; a stopped VM's too; and a schedule.
//!
//! Uses the NFS export at MST_E2E_BACKUP_NFS (default the lab's
//! `10.77.0.10:/srv/nfs/backups`, exported with no_root_squash).

use std::time::Duration;

use mst_e2e::{Api, Console};
use reqwest::Method;
use serde_json::{Value, json};

const BOOT: Duration = Duration::from_secs(5 * 60);
const COPY: Duration = Duration::from_secs(10 * 60);
const PASSWORD: &str = "e2e-password";

#[tokio::test]
async fn backup_restore_and_schedule() -> anyhow::Result<()> {
    let Some(api) = mst_e2e::connect().await? else {
        return Ok(());
    };
    let run = mst_e2e::run_name("backups");
    let nfs = std::env::var("MST_E2E_BACKUP_NFS")
        .unwrap_or_else(|_| "10.77.0.10:/srv/nfs/backups".into());
    let (server, export) = nfs
        .split_once(':')
        .expect("MST_E2E_BACKUP_NFS is server:/export");
    let repo = api
        .post(
            "/backup-repositories",
            json!({"name": run, "server": server, "export": export}),
        )
        .await?;
    let repo = repo["id"].as_str().unwrap().to_owned();

    let outcome = mst_e2e::attempt(scenario(&api, &run, &repo)).await;

    // Clean up whatever the scenario left: schedules, backups, VMs, the
    // repository.
    let cleanup = async {
        let vms = api.get("/vms").await?;
        for v in vms.as_array().into_iter().flatten() {
            if v["name"].as_str().is_some_and(|n| n.starts_with(&run)) {
                let _ = api
                    .try_send(
                        Method::PUT,
                        &format!("/vms/{}/backup-schedule", v["id"].as_str().unwrap()),
                        Some(json!({"every": null})),
                    )
                    .await;
            }
        }
        for b in api.get("/backups").await?.as_array().into_iter().flatten() {
            if b["repository_id"] != repo.as_str() {
                continue;
            }
            let id = b["id"].as_str().unwrap();
            if b["state"] == "running" {
                wait_backup(&api, id, |b| b["state"] != "running").await?;
            }
            let (_, t) = api
                .try_send(Method::DELETE, &format!("/backups/{id}"), None)
                .await?;
            if let Some(task) = t["task_id"].as_str() {
                api.wait_task(task, COPY).await?;
            }
        }
        mst_e2e::clean(&api, &run).await?;
        let (st, body) = api
            .try_send(
                Method::DELETE,
                &format!("/backup-repositories/{repo}"),
                None,
            )
            .await?;
        anyhow::ensure!(st == 204, "deleting the repository: {body}");
        Ok(())
    }
    .await;
    outcome.finish(cleanup)
}

async fn wait_backup(api: &Api, id: &str, pred: impl Fn(&Value) -> bool) -> anyhow::Result<Value> {
    let deadline = std::time::Instant::now() + COPY;
    loop {
        let b = api.get(&format!("/backups/{id}")).await?;
        if pred(&b) {
            return Ok(b);
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "backup {id} stuck: {b}"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Runs a command on the VM's serial console.
async fn sh(api: &Api, vm: &str, cmd: &str) -> anyhow::Result<String> {
    let mut serial: Console = api.console(vm, "serial").await?;
    let (out, code) = serial.run("debian", PASSWORD, cmd).await?;
    anyhow::ensure!(code == 0, "{cmd} failed ({code}): {out}");
    Ok(out)
}

async fn scenario(api: &Api, run: &str, repo: &str) -> anyhow::Result<()> {
    let image = api.image().await?;
    let src = api
        .post(
            "/vms",
            json!({"name": format!("{run}-src"), "vcpus": 1, "memory_mib": 1024, "disk_gib": 5,
                   "image_id": image, "password": PASSWORD}),
        )
        .await?;
    api.wait_task(src["task_id"].as_str().unwrap(), BOOT)
        .await?;
    let src = src["id"].as_str().unwrap().to_owned();

    // Something on the disk to find again.
    sh(
        api,
        &src,
        &format!("echo {run} | sudo tee /var/tmp/marker && sync"),
    )
    .await?;

    // Backed up while it runs; the VM carries on.
    let b = api
        .post(
            &format!("/vms/{src}/backups"),
            json!({"repository_id": repo}),
        )
        .await?;
    api.wait_task(b["task_id"].as_str().unwrap(), COPY).await?;
    let backup = api
        .get(&format!("/backups/{}", b["id"].as_str().unwrap()))
        .await?;
    assert_eq!(backup["state"], "ready", "{backup}");
    assert!(backup["stored_bytes"].as_i64().unwrap() > 0, "{backup}");
    assert_eq!(backup["vm_name"], format!("{run}-src"));
    let vm = api.get(&format!("/vms/{src}")).await?;
    assert_eq!(vm["power"], "running", "the backup left the VM running");
    // One at a time.
    let (st, _) = api
        .try_send(
            Method::POST,
            &format!("/vms/{src}/backups"),
            Some(json!({})),
        )
        .await?;
    assert!(st == 202 || st == 409, "{st}");
    if st == 202 {
        let list = api.get(&format!("/vms/{src}/backups")).await?;
        let second = list["backups"][0]["id"].as_str().unwrap().to_owned();
        wait_backup(api, &second, |b| b["state"] != "running").await?;
    }

    // Changed after the backup: the restore must not see this.
    sh(api, &src, "echo later | sudo tee /var/tmp/marker && sync").await?;

    let restored = api
        .post(
            &format!("/backups/{}/restore", backup["id"].as_str().unwrap()),
            json!({"name": format!("{run}-restored"), "password": PASSWORD}),
        )
        .await?;
    api.wait_task(restored["task_id"].as_str().unwrap(), COPY)
        .await?;
    let restored = restored["id"].as_str().unwrap().to_owned();
    let vm = api.get(&format!("/vms/{restored}")).await?;
    assert_eq!(vm["disks"][0]["size_bytes"], 5u64 << 30, "{vm}");
    let out = sh(api, &restored, "cat /var/tmp/marker").await?;
    assert!(out.contains(run) && !out.contains("later"), "{out}");

    // A stopped VM's backup is consistent: nothing was running.
    let stop = api
        .post(&format!("/vms/{restored}/stop"), json!({}))
        .await?;
    api.wait_task(stop["task_id"].as_str().unwrap(), BOOT)
        .await?;
    // The guest shuts down in its own time; meanwhile backups are refused.
    let (st, _) = api
        .try_send(
            Method::POST,
            &format!("/vms/{restored}/backups"),
            Some(json!({})),
        )
        .await?;
    assert!(st == 409 || st == 202, "{st}");
    api.wait_vm(&restored, BOOT, |v| v["power"] == "stopped")
        .await?;
    if st == 202 {
        // It had stopped already: that backup is the stopped one.
        let list = api.get(&format!("/vms/{restored}/backups")).await?;
        let id = list["backups"][0]["id"].as_str().unwrap().to_owned();
        wait_backup(api, &id, |b| b["state"] != "running").await?;
    }
    let b = api
        .post(
            &format!("/vms/{restored}/backups"),
            json!({"repository_id": repo}),
        )
        .await?;
    api.wait_task(b["task_id"].as_str().unwrap(), COPY).await?;
    let offline = api
        .get(&format!("/backups/{}", b["id"].as_str().unwrap()))
        .await?;
    assert_eq!(offline["state"], "ready", "{offline}");
    assert_eq!(offline["consistent"], true, "{offline}");

    // A schedule takes its first backup within a couple of minutes.
    let (_, set) = api
        .send(
            Method::PUT,
            &format!("/vms/{src}/backup-schedule"),
            Some(json!({"every": "daily", "keep": 1, "repository_id": repo})),
        )
        .await?;
    assert_eq!(set["schedule"]["every"], "daily", "{set}");
    let deadline = std::time::Instant::now() + Duration::from_secs(4 * 60);
    loop {
        let list = api.get(&format!("/vms/{src}/backups")).await?;
        let scheduled = list["backups"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|b| b["scheduled"] == true && b["state"] == "ready");
        if scheduled.is_some() {
            break;
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "no scheduled backup: {list}"
        );
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    // Backups outlive their VM.
    let (_, del) = api
        .send(Method::DELETE, &format!("/vms/{restored}"), None)
        .await?;
    api.wait_task(del["task_id"].as_str().unwrap(), BOOT)
        .await?;
    let gone = api
        .get(&format!("/backups/{}", offline["id"].as_str().unwrap()))
        .await?;
    assert_eq!(gone["vm_id"], Value::Null, "{gone}");
    assert_eq!(gone["vm_name"], format!("{run}-restored"));
    Ok(())
}
