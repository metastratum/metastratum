//! Backups: a running VM's disk copied into a repository, restored as a new
//! VM with what it held then; a stopped VM's too; and a schedule.
//! Incremental ones: what changed since the last backup, tracked by QEMU
//! across a migration, found by comparison while the VM is stopped, taken
//! in full when a snapshot revert broke the tracking; merged into the next
//! when deleted, restoring whole throughout.
//!
//! Uses the NFS export at MST_E2E_BACKUP_NFS (default the lab's
//! `10.77.0.10:/srv/nfs/backups`, exported with no_root_squash).

use std::collections::BTreeMap;
use std::future::Future;
use std::time::Duration;

use mst_e2e::{Api, Console};
use reqwest::Method;
use serde_json::{Value, json};

const BOOT: Duration = Duration::from_secs(5 * 60);
const COPY: Duration = Duration::from_secs(10 * 60);
const PASSWORD: &str = "e2e-password";

#[tokio::test]
async fn backup_restore_and_schedule() -> anyhow::Result<()> {
    with_repository("backups", |api, run, repo| async move {
        scenario(&api, &run, &repo).await
    })
    .await
}

#[tokio::test]
async fn incremental_backups() -> anyhow::Result<()> {
    with_repository("backups-inc", |api, run, repo| async move {
        incremental(&api, &run, &repo).await
    })
    .await
}

/// Runs `scenario` with a repository of its own, then cleans up whatever
/// it left: schedules, backups, VMs, the repository.
async fn with_repository<F, Fut>(suite: &str, scenario: F) -> anyhow::Result<()>
where
    F: FnOnce(Api, String, String) -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    let Some(api) = mst_e2e::connect().await? else {
        return Ok(());
    };
    let run = mst_e2e::run_name(suite);
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

    let outcome = mst_e2e::attempt(scenario(api.clone(), run.clone(), repo.clone())).await;

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
        // Newest first: nothing builds on them.
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
            Some(json!({"repository_id": repo})),
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
            Some(json!({"repository_id": repo})),
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

/// Takes a backup of the VM (`full` or `incremental`), waits for it, and
/// returns it.
async fn back_up(api: &Api, vm: &str, repo: &str, mode: &str) -> anyhow::Result<Value> {
    let b = api
        .post(
            &format!("/vms/{vm}/backups"),
            json!({"repository_id": repo, "mode": mode}),
        )
        .await?;
    api.wait_task(b["task_id"].as_str().unwrap(), COPY).await?;
    let b = api
        .get(&format!("/backups/{}", b["id"].as_str().unwrap()))
        .await?;
    anyhow::ensure!(b["state"] == "ready", "{b}");
    Ok(b)
}

/// Writes `mib` MiB of random data to /var/tmp/<name> in the guest.
async fn scribble(api: &Api, vm: &str, name: &str, mib: u32) -> anyhow::Result<()> {
    sh(
        api,
        vm,
        &format!(
            "sudo dd if=/dev/urandom of=/var/tmp/{name} bs=1M count={mib} status=none && sync"
        ),
    )
    .await?;
    Ok(())
}

/// The SHA-256 of the guest's files /var/tmp/[a-z], by name.
async fn sums(api: &Api, vm: &str) -> anyhow::Result<BTreeMap<String, String>> {
    let out = sh(api, vm, "cd /var/tmp && sha256sum [a-z]").await?;
    // Shown when the test fails.
    eprintln!("sums on {vm}: {out:?}");
    Ok(out
        .lines()
        .filter_map(|l| {
            // The console may run the first line into what came before.
            let (sum, name) = l.trim().split_once("  ")?;
            let sum = sum.get(sum.len().checked_sub(64)?..)?;
            sum.bytes()
                .all(|b| b.is_ascii_hexdigit())
                .then(|| (name.to_owned(), sum.to_owned()))
        })
        .collect())
}

async fn delete_backup(api: &Api, id: &str) -> anyhow::Result<()> {
    let (_, t) = api
        .send(Method::DELETE, &format!("/backups/{id}"), None)
        .await?;
    api.wait_task(t["task_id"].as_str().unwrap(), COPY).await?;
    Ok(())
}

/// Restores a backup as a new VM, and returns what its files hash to.
async fn restored_sums(
    api: &Api,
    run: &str,
    backup: &str,
    name: &str,
) -> anyhow::Result<BTreeMap<String, String>> {
    let vm = api
        .post(
            &format!("/backups/{backup}/restore"),
            json!({"name": format!("{run}-{name}"), "password": PASSWORD}),
        )
        .await?;
    api.wait_task(vm["task_id"].as_str().unwrap(), COPY).await?;
    let id = vm["id"].as_str().unwrap().to_owned();
    let out = sums(api, &id).await?;
    mst_e2e::delete_vm(api, &id).await?;
    Ok(out)
}

async fn incremental(api: &Api, run: &str, repo: &str) -> anyhow::Result<()> {
    let image = api.image().await?;
    let vm = api
        .post(
            "/vms",
            json!({"name": format!("{run}-vm"), "vcpus": 1, "memory_mib": 768, "disk_gib": 5,
                   "image_id": image, "password": PASSWORD, "migratable": true}),
        )
        .await?;
    api.wait_task(vm["task_id"].as_str().unwrap(), BOOT).await?;
    let vm = vm["id"].as_str().unwrap().to_owned();

    // A full backup to start from.
    scribble(api, &vm, "a", 16).await?;
    let full = back_up(api, &vm, repo, "full").await?;
    assert_eq!(
        (&full["kind"], &full["parent_id"]),
        (&json!("full"), &Value::Null),
        "{full}"
    );

    // Changes, overwriting some of what was there: only they are copied.
    scribble(api, &vm, "b", 16).await?;
    sh(
        api,
        &vm,
        "sudo dd if=/dev/urandom of=/var/tmp/a bs=1M count=4 conv=notrunc status=none && sync",
    )
    .await?;
    let first = back_up(api, &vm, repo, "incremental").await?;
    assert_eq!(
        (&first["kind"], &first["parent_id"]),
        (&json!("incremental"), &full["id"]),
        "{first}"
    );
    let (whole, part) = (
        full["stored_bytes"].as_i64().unwrap(),
        first["stored_bytes"].as_i64().unwrap(),
    );
    assert!(
        part < whole / 4,
        "an incremental backup of {part} bytes, the full one {whole}"
    );

    // The tracking moves with the VM.
    let m = api.post(&format!("/vms/{vm}/migrate"), json!({})).await?;
    api.wait_task(m["task_id"].as_str().unwrap(), COPY).await?;
    scribble(api, &vm, "c", 8).await?;
    let second = back_up(api, &vm, repo, "incremental").await?;
    assert_eq!(
        (&second["kind"], &second["parent_id"]),
        (&json!("incremental"), &first["id"]),
        "{second}"
    );
    let at_second = sums(api, &vm).await?;

    // Stopped, snapshotted and reverted (which drops the tracking): changes
    // are found by comparing.
    let stop = api.post(&format!("/vms/{vm}/stop"), json!({})).await?;
    api.wait_task(stop["task_id"].as_str().unwrap(), BOOT)
        .await?;
    api.wait_vm(&vm, BOOT, |v| v["power"] == "stopped").await?;
    let snap = api
        .post(&format!("/vms/{vm}/snapshots"), json!({"name": "before"}))
        .await?;
    api.wait_task(snap["task_id"].as_str().unwrap(), COPY)
        .await?;
    let revert = api
        .post(
            &format!("/snapshots/{}/revert", snap["id"].as_str().unwrap()),
            json!({}),
        )
        .await?;
    api.wait_task(revert["task_id"].as_str().unwrap(), COPY)
        .await?;
    let third = back_up(api, &vm, repo, "incremental").await?;
    assert_eq!(
        (&third["kind"], &third["parent_id"]),
        (&json!("incremental"), &second["id"]),
        "{third}"
    );
    assert_eq!(third["consistent"], true, "{third}");

    // Running again, without tracking since: in full, saying why; then
    // incremental on that.
    let start = api.post(&format!("/vms/{vm}/start"), json!({})).await?;
    api.wait_task(start["task_id"].as_str().unwrap(), BOOT)
        .await?;
    scribble(api, &vm, "d", 8).await?;
    let fourth = back_up(api, &vm, repo, "incremental").await?;
    assert_eq!(
        (&fourth["kind"], &fourth["parent_id"]),
        (&json!("full"), &Value::Null),
        "{fourth}"
    );
    assert!(
        fourth["full_reason"]
            .as_str()
            .is_some_and(|r| r.contains("change tracking")),
        "{fourth}"
    );
    scribble(api, &vm, "e", 8).await?;
    let fifth = back_up(api, &vm, repo, "incremental").await?;
    assert_eq!(
        (&fifth["kind"], &fifth["parent_id"]),
        (&json!("incremental"), &fourth["id"]),
        "{fifth}"
    );
    let at_fifth = sums(api, &vm).await?;

    // Deleting the full backup merges it into the first incremental one,
    // which stands alone then; deleting the second merges it into the third.
    let id = |b: &Value| b["id"].as_str().unwrap().to_owned();
    delete_backup(api, &id(&full)).await?;
    let first = api.get(&format!("/backups/{}", id(&first))).await?;
    assert_eq!(first["parent_id"], Value::Null, "{first}");
    delete_backup(api, &id(&second)).await?;
    let third = api.get(&format!("/backups/{}", id(&third))).await?;
    assert_eq!(third["parent_id"], first["id"], "{third}");

    // Restored, each is the VM as it was.
    assert_eq!(at_second.keys().collect::<Vec<_>>(), ["a", "b", "c"]);
    assert_eq!(
        restored_sums(api, run, &id(&third), "third").await?,
        at_second
    );
    assert_eq!(
        at_fifth.keys().collect::<Vec<_>>(),
        ["a", "b", "c", "d", "e"]
    );
    assert_eq!(
        restored_sums(api, run, &id(&fifth), "fifth").await?,
        at_fifth
    );

    // A schedule's full backups.
    let (_, set) = api
        .send(
            Method::PUT,
            &format!("/vms/{vm}/backup-schedule"),
            Some(json!({"every": "weekly", "keep": 4, "repository_id": repo, "full_every": 3})),
        )
        .await?;
    assert_eq!(set["schedule"]["full_every"], 3, "{set}");
    let (st, _) = api
        .try_send(
            Method::PUT,
            &format!("/vms/{vm}/backup-schedule"),
            Some(json!({"every": "weekly", "full_every": 0, "repository_id": repo})),
        )
        .await?;
    assert_eq!(st, 400);
    Ok(())
}
