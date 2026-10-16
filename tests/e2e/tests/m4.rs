//! M4 end to end: live migration on the lab, with traffic.
//!
//! An observer VM pings the migrating VM every 0.2 s and holds a TCP
//! connection to its SSH server open across the move. For each kind of
//! storage (local, and the NFS and Ceph pools the storage tests set up, if
//! present), the VM must end up on another host without rebooting (same boot
//! ID), lose few pings, and the connection must still work afterwards.
//!
//! Cancelling a migration leaves the VM where it was. With
//! `MST_E2E_EVACUATE=1` a host is also evacuated; that moves every VM on it,
//! not only the test's.

use std::time::Duration;

use mst_e2e::{Api, Console};
use reqwest::Method;
use serde_json::{Value, json};

const PW: &str = "e2e-password";

async fn create(api: &Api, image: &str, name: &str, pool: Option<&str>) -> anyhow::Result<Value> {
    api.post(
        "/vms",
        json!({"name": name, "vcpus": 1, "memory_mib": 768, "disk_gib": 4, "image_id": image,
               "password": PW, "storage_pool_id": pool}),
    )
    .await
}

async fn running(api: &Api, id: &str) -> anyhow::Result<Value> {
    api.wait_vm(id, Duration::from_secs(180), |v| {
        v["status"] == "ready" && v["power"] == "running"
    })
    .await
}

/// Runs a migration task to its end and returns it.
async fn migrate(api: &Api, id: &str, body: Value) -> anyhow::Result<Value> {
    let t = api.post(&format!("/vms/{id}/migrate"), body).await?;
    api.wait_task(t["task_id"].as_str().unwrap(), Duration::from_secs(600))
        .await
}

async fn sh(c: &mut Console, cmd: &str) -> anyhow::Result<String> {
    let (out, rc) = c.run("debian", PW, cmd).await?;
    anyhow::ensure!(rc == 0, "`{cmd}` failed ({rc}):\n{out}");
    Ok(out)
}

/// The boot ID in a console transcript (the command is echoed around it).
fn boot_id(out: &str) -> String {
    out.split(|c: char| !(c.is_ascii_hexdigit() || c == '-'))
        .find(|w| w.len() == 36 && w.matches('-').count() == 4)
        .unwrap_or_else(|| panic!("no boot ID in:\n{out}"))
        .to_owned()
}

/// Transmitted and received counts from ping's summary.
fn ping_counts(log: &str) -> Option<(u32, u32)> {
    let line = log.lines().find(|l| l.contains("packets transmitted"))?;
    let mut nums = line
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty());
    Some((nums.next()?.parse().ok()?, nums.next()?.parse().ok()?))
}

#[tokio::test]
async fn live_migration_keeps_vms_running_and_reachable() -> anyhow::Result<()> {
    let Some(api) = mst_e2e::connect().await? else {
        return Ok(());
    };
    let run = format!(
        "e2e-{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs()
    );
    // In a task, so a failed assertion still reaches the cleanup below.
    let result = tokio::spawn({
        let (api, run) = (api.clone(), run.clone());
        async move { scenario(&api, &run).await }
    })
    .await;
    if let Ok(vms) = api.get("/vms").await {
        for vm in vms.as_array().into_iter().flatten() {
            if vm["name"].as_str().is_some_and(|n| n.starts_with(&run)) {
                let id = vm["id"].as_str().unwrap();
                let _ = api
                    .try_send(Method::POST, &format!("/vms/{id}/migrate/cancel"), None)
                    .await;
                for _ in 0..60 {
                    if api
                        .get(&format!("/vms/{id}"))
                        .await
                        .is_ok_and(|v| v["status"] != "migrating")
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                if let Ok((_, t)) = api
                    .try_send(Method::DELETE, &format!("/vms/{id}"), None)
                    .await
                    && let Some(t) = t["task_id"].as_str()
                {
                    let _ = api.wait_task(t, Duration::from_secs(120)).await;
                }
            }
        }
    }
    match result {
        Ok(r) => r,
        Err(panic) => std::panic::resume_unwind(panic.into_panic()),
    }
}

async fn scenario(api: &Api, run: &str) -> anyhow::Result<()> {
    let image = api.image().await?;
    let pools = api.get("/storage-pools").await?;
    let pool = |name: &str| {
        pools
            .as_array()
            .into_iter()
            .flatten()
            .find(|p| p["name"] == name && !p["cluster_ids"].as_array().is_none_or(Vec::is_empty))
            .map(|p| p["id"].as_str().unwrap().to_owned())
    };
    let mut variants = vec![("local", None)];
    for (label, name) in [("nfs", "e2e-nfs"), ("rbd", "e2e-rbd")] {
        match pool(name) {
            Some(id) => variants.push((label, Some(id))),
            None => eprintln!("no {name} pool; skipping {label} (run the storage tests first)"),
        }
    }

    let obs = create(api, &image, &format!("{run}-obs"), None).await?;
    let obs_id = obs["id"].as_str().unwrap().to_owned();
    running(api, &obs_id).await?;
    let mut o = api.console(&obs_id, "serial").await?;

    for (label, pool) in &variants {
        let vm = create(api, &image, &format!("{run}-{label}"), pool.as_deref()).await?;
        let id = vm["id"].as_str().unwrap().to_owned();
        let before = running(api, &id).await?;
        let ip = before["nics"][0]["ip"].as_str().unwrap().to_owned();
        let mut c = api.console(&id, "serial").await?;
        let boot = boot_id(&sh(&mut c, "cat /proc/sys/kernel/random/boot_id").await?);
        drop(c);

        // Traffic across the move: pings, and a TCP connection that only
        // speaks after the move.
        sh(&mut o, "rm -f /tmp/ping.log /tmp/tcp.* /tmp/go").await?;
        sh(
            &mut o,
            &format!("(nohup ping -i 0.2 {ip} > /tmp/ping.log 2>&1 &)"),
        )
        .await?;
        let tcp = format!(
            "exec 3<>/dev/tcp/{ip}/22 && head -c 7 <&3 > /tmp/tcp.banner && \
             while [ ! -e /tmp/go ]; do sleep 0.2; done && \
             printf 'SSH-2.0-e2e\\r\\n' >&3 && head -c 4 <&3 > /tmp/tcp.after"
        );
        sh(
            &mut o,
            &format!("(nohup bash -c '{tcp}' >/dev/null 2>&1 &)"),
        )
        .await?;
        tokio::time::sleep(Duration::from_secs(3)).await;
        sh(&mut o, "grep -q SSH- /tmp/tcp.banner").await?;

        let task = migrate(api, &id, json!({})).await?;
        assert_eq!(task["state"], "succeeded", "{label}: {task}");
        let after = running(api, &id).await?;
        assert_ne!(
            after["host_id"], before["host_id"],
            "{label}: the VM did not move"
        );
        eprintln!("{label}: {} -> {}", before["host_name"], after["host_name"]);

        // Let a few pings through on the new host, then stop them.
        tokio::time::sleep(Duration::from_secs(3)).await;
        sh(&mut o, "touch /tmp/go; sleep 2; pkill -INT ping; sleep 1").await?;
        let log = sh(&mut o, "tail -3 /tmp/ping.log").await?;
        let (sent, got) = ping_counts(&log).unwrap_or_else(|| panic!("no ping summary:\n{log}"));
        let lost = sent - got;
        eprintln!("{label}: {lost} of {sent} pings lost");
        assert!(
            lost <= 5 && lost * 10 <= sent,
            "{label}: lost {lost} of {sent} pings"
        );
        let size = sh(&mut o, "stat -c %s /tmp/tcp.after").await?;
        assert!(
            size.contains('4'),
            "{label}: the TCP connection did not survive: {size}"
        );

        // Same boot: the guest never restarted.
        let mut c = api.console(&id, "serial").await?;
        assert_eq!(
            boot_id(&sh(&mut c, "cat /proc/sys/kernel/random/boot_id").await?),
            boot,
            "{label}: rebooted"
        );
    }

    // Cancelling right away: nothing moves, and the VM is usable again.
    let vm = api.get("/vms?descendants=true").await?;
    let subject = vm
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == format!("{run}-local"))
        .unwrap()
        .clone();
    let id = subject["id"].as_str().unwrap();
    let t = api.post(&format!("/vms/{id}/migrate"), json!({})).await?;
    let (st, _) = api
        .try_send(Method::POST, &format!("/vms/{id}/migrate/cancel"), None)
        .await?;
    assert_eq!(st, 204);
    let task = api
        .wait_task_end(t["task_id"].as_str().unwrap(), Duration::from_secs(300))
        .await?;
    assert_eq!(task["state"], "failed", "{task}");
    assert!(
        task["error"].as_str().unwrap_or("").contains("cancelled"),
        "{task}"
    );
    let back = running(api, id).await?;
    assert_eq!(back["host_id"], subject["host_id"]);
    assert!(back["migration"].is_null());
    // While nothing migrates, a cancel is refused.
    let (st, _) = api
        .try_send(Method::POST, &format!("/vms/{id}/migrate/cancel"), None)
        .await?;
    assert_eq!(st, 409);

    // A VM that may not migrate: refused, and it sees more of the host's CPU
    // than the cluster's baseline gives the observer.
    let bound = api
        .post(
            "/vms",
            json!({"name": format!("{run}-bound"), "vcpus": 1, "memory_mib": 512, "disk_gib": 3,
                   "image_id": image, "password": PW, "migratable": false}),
        )
        .await?;
    let bound_id = bound["id"].as_str().unwrap();
    assert_eq!(bound["migratable"], false);
    running(api, bound_id).await?;
    let (st, body) = api
        .try_send(
            Method::POST,
            &format!("/vms/{bound_id}/migrate"),
            Some(json!({})),
        )
        .await?;
    assert_eq!(st, 409, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or("")
            .contains("not migratable"),
        "{body}"
    );
    let flags = |out: String| -> usize {
        // Skip the echoed command; terminal escapes may precede the line.
        out.lines()
            .filter(|l| !l.contains("grep"))
            .find_map(|l| {
                l.split_once("flags")
                    .and_then(|(_, rest)| rest.split_once(':'))
            })
            .map(|(_, list)| list.split_whitespace().count())
            .unwrap_or(0)
    };
    let mut c = api.console(bound_id, "serial").await?;
    let host_flags = flags(sh(&mut c, "grep -m1 ^flags /proc/cpuinfo").await?);
    let baseline_flags = flags(sh(&mut o, "grep -m1 ^flags /proc/cpuinfo").await?);
    eprintln!("CPU flags: host {host_flags}, cluster baseline {baseline_flags}");
    assert!(
        host_flags > baseline_flags,
        "host {host_flags} vs baseline {baseline_flags}"
    );
    drop(c);

    // The setting changes only while the VM is stopped, from the next start.
    let patch = json!({"migratable": true});
    let (st, _) = api
        .try_send(
            Method::PATCH,
            &format!("/vms/{bound_id}"),
            Some(patch.clone()),
        )
        .await?;
    assert_eq!(st, 409, "changed while running");
    let t = api
        .post(&format!("/vms/{bound_id}/stop?force=true"), json!({}))
        .await?;
    api.wait_task(t["task_id"].as_str().unwrap(), Duration::from_secs(60))
        .await?;
    api.wait_vm(bound_id, Duration::from_secs(30), |v| {
        v["power"] == "stopped"
    })
    .await?;
    let (st, body) = api
        .try_send(Method::PATCH, &format!("/vms/{bound_id}"), Some(patch))
        .await?;
    assert_eq!((st, &body["migratable"]), (200, &json!(true)), "{body}");
    let t = api
        .post(&format!("/vms/{bound_id}/start"), json!({}))
        .await?;
    api.wait_task(t["task_id"].as_str().unwrap(), Duration::from_secs(60))
        .await?;
    running(api, bound_id).await?;
    let mut c = api.console(bound_id, "serial").await?;
    let now = flags(sh(&mut c, "grep -m1 ^flags /proc/cpuinfo").await?);
    assert_eq!(
        now, baseline_flags,
        "after allowing migration it should have the cluster's CPU"
    );
    drop(c);
    let task = migrate(api, bound_id, json!({})).await?;
    assert_eq!(task["state"], "succeeded", "{task}");

    if std::env::var_os("MST_E2E_EVACUATE").is_some() {
        let host = back["host_id"].as_str().unwrap().to_owned();
        let t = api
            .post(&format!("/hosts/{host}/evacuate"), json!({}))
            .await?;
        let task = api
            .wait_task(t["task_id"].as_str().unwrap(), Duration::from_secs(1200))
            .await;
        // Put the host back in service whatever happened.
        api.send(
            Method::PATCH,
            &format!("/hosts/{host}"),
            Some(json!({"state": "ready"})),
        )
        .await?;
        let task = task?;
        assert_eq!(task["state"], "succeeded", "{task}");
        let left: Vec<Value> = api
            .get("/vms?descendants=true")
            .await?
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["host_id"] == host.as_str() && v["power"] == "running")
            .cloned()
            .collect();
        assert!(
            left.is_empty(),
            "still running on the evacuated host: {left:?}"
        );
    }
    Ok(())
}
