//! Network interfaces added to a running VM, moved to another network and
//! removed, the guest following each time (DHCP); and a VM with an
//! interface plugged in while it ran migrates.

use std::time::{Duration, Instant};

use mst_e2e::Api;
use reqwest::Method;
use serde_json::{Value, json};

const PW: &str = "e2e-password";

#[tokio::test]
async fn interfaces_come_move_and_go_while_the_vm_runs() -> anyhow::Result<()> {
    let Some(admin) = mst_e2e::connect().await? else {
        return Ok(());
    };
    let run = mst_e2e::run_name("nics");
    let root = admin.get("/auth/me").await?["tenant_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let t = admin
        .post(
            "/tenants",
            json!({"parent_id": root, "slug": run, "name": "NICs"}),
        )
        .await?["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // Deleting the tenant (after its VMs) removes its networks.
    mst_e2e::cleaned_up(&admin, &run, scenario(&admin, &run, &t)).await
}

async fn scenario(admin: &Api, run: &str, t: &str) -> anyhow::Result<()> {
    let clusters: Vec<Value> = admin
        .get("/clusters")
        .await?
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].clone())
        .collect();
    admin
        .send(
            Method::PUT,
            &format!("/tenants/{t}/grants"),
            Some(json!({"clusters": clusters, "storage_pools": [], "external_networks": []})),
        )
        .await?;
    let vpc = admin
        .post("/vpcs", json!({"tenant_id": t, "name": "v"}))
        .await?;
    let net = |name: &str, cidr: &str| {
        admin.post(
            "/networks",
            json!({"vpc_id": vpc["id"], "name": name, "cidr": cidr}),
        )
    };
    let a = net("a", "10.61.0.0/24").await?;
    let b = net("b", "10.62.0.0/24").await?;
    let c = net("c", "10.63.0.0/24").await?;

    let image = admin.image().await?;
    // A peer to reach: in b, and (added while it runs) in c.
    let peer = admin
        .post(
            "/vms",
            json!({"tenant_id": t, "name": format!("{run}-peer"), "vcpus": 1, "memory_mib": 512,
                   "disk_gib": 4, "image_id": image, "password": PW, "network_id": b["id"]}),
        )
        .await?;
    let peer_id = peer["id"].as_str().unwrap().to_owned();
    let vm = admin
        .post(
            "/vms",
            json!({"tenant_id": t, "name": format!("{run}-vm"), "vcpus": 1, "memory_mib": 1024,
                   "disk_gib": 4, "image_id": image, "password": PW, "network_id": a["id"]}),
        )
        .await?;
    let id = vm["id"].as_str().unwrap().to_owned();
    admin
        .wait_task(vm["task_id"].as_str().unwrap(), Duration::from_secs(600))
        .await?;
    admin
        .wait_vm(&id, Duration::from_secs(120), |v| v["power"] == "running")
        .await?;
    admin
        .wait_task(peer["task_id"].as_str().unwrap(), Duration::from_secs(600))
        .await?;
    let p = admin
        .post(
            &format!("/vms/{peer_id}/nics"),
            json!({"network_id": c["id"]}),
        )
        .await?;
    let peer_b = p["nics"][0]["ip"].as_str().unwrap().to_owned();
    let peer_c = p["nics"][1]["ip"].as_str().unwrap().to_owned();
    let mut peer_serial = admin.console(&peer_id, "serial").await?;
    has_address(&mut peer_serial, &peer_c).await?;
    drop(peer_serial);
    let mut serial = admin.console(&id, "serial").await?;
    let (out, rc) = serial.run("debian", PW, &ping(&peer_b)).await?;
    assert_eq!(rc, 0, "the peer, routed: {out}");

    // Added while it runs: plugged in, and the guest takes its address.
    let v = admin
        .post(&format!("/vms/{id}/nics"), json!({"network_id": b["id"]}))
        .await?;
    let nics = v["nics"].as_array().unwrap();
    assert_eq!(nics.len(), 2, "{v}");
    let added = nics[1].clone();
    assert_eq!(added["network_id"], b["id"]);
    let ip = added["ip"].as_str().unwrap().to_owned();
    assert!(ip.starts_with("10.62.0."), "{added}");
    has_address(&mut serial, &ip).await?;
    let (out, rc) = serial.run("debian", PW, &ping(&peer_b)).await?;
    assert_eq!(rc, 0, "the peer, in the added network: {out}");

    // Moved to another network: a new address, the guest following.
    let nic = added["id"].as_str().unwrap();
    let (_, v) = admin
        .send(
            Method::PATCH,
            &format!("/vms/{id}/nics/{nic}"),
            Some(json!({"network_id": c["id"]})),
        )
        .await?;
    let moved = v["nics"][1].clone();
    assert_eq!(
        (&moved["network_id"], &moved["mac"]),
        (&c["id"], &added["mac"]),
        "{v}"
    );
    let ip = moved["ip"].as_str().unwrap().to_owned();
    assert!(ip.starts_with("10.63.0."), "{moved}");
    has_address(&mut serial, &ip).await?;
    let (out, rc) = serial.run("debian", PW, &ping(&peer_c)).await?;
    assert_eq!(rc, 0, "the peer, in the network it moved to: {out}");
    let (out, _) = serial.run("debian", PW, "ip -4 -o addr | cat").await?;
    assert!(!out.contains("10.62.0."), "the old address is gone: {out}");
    drop(serial);

    // Migrated with the interface plugged in while it ran.
    let hosts = admin.get("/hosts").await?;
    if hosts.as_array().unwrap().len() > 1 {
        let before = admin.get(&format!("/vms/{id}")).await?["host_id"].clone();
        let task = admin.post(&format!("/vms/{id}/migrate"), json!({})).await?;
        admin
            .wait_task(task["task_id"].as_str().unwrap(), Duration::from_secs(600))
            .await
            .map_err(|e| e.context("migrating with an interface plugged in while it ran"))?;
        let after = admin.get(&format!("/vms/{id}")).await?;
        assert_ne!(after["host_id"], before, "it moved: {after}");
        assert_eq!(after["power"], "running", "{after}");
        let mut serial = admin.console(&id, "serial").await?;
        for ip in [&peer_b, &peer_c] {
            let (out, rc) = serial.run("debian", PW, &ping(ip)).await?;
            assert_eq!(rc, 0, "{ip} after migrating: {out}");
        }
    }

    // Removed: unplugged from the guest. The last one stays.
    let (_, v) = admin
        .send(Method::DELETE, &format!("/vms/{id}/nics/{nic}"), None)
        .await?;
    assert_eq!(v["nics"].as_array().unwrap().len(), 1, "{v}");
    let mut serial = admin.console(&id, "serial").await?;
    let mac = added["mac"].as_str().unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (out, _) = serial.run("debian", PW, "ip -o link | cat").await?;
        if !out.to_lowercase().contains(mac) {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "still in the guest: {out}");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let last = v["nics"][0]["id"].as_str().unwrap();
    let (st, _) = admin
        .try_send(Method::DELETE, &format!("/vms/{id}/nics/{last}"), None)
        .await?;
    assert_eq!(st, 409, "a VM keeps one interface");
    // Another tenant's network is not one to move to.
    let theirs = admin.get("/vpcs").await?;
    if let Some(other) = theirs
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|v| v["networks"].as_array().into_iter().flatten())
        .next()
    {
        let (st, _) = admin
            .try_send(
                Method::PATCH,
                &format!("/vms/{id}/nics/{last}"),
                Some(json!({"network_id": other["id"]})),
            )
            .await?;
        assert_eq!(st, 404, "the provider's network is not the tenant's");
    }
    Ok(())
}

/// Pings `ip`, giving the path a minute to come up.
fn ping(ip: &str) -> String {
    format!("ping -c1 -w60 {ip} >/dev/null; ping -c2 -W2 {ip}")
}

/// Waits until the guest has `ip` on an interface (DHCP takes a moment).
async fn has_address(serial: &mut mst_e2e::Console, ip: &str) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let (out, _) = serial.run("debian", PW, "ip -4 -o addr | cat").await?;
        if out.contains(&format!(" {ip}/")) {
            return Ok(());
        }
        if Instant::now() > deadline {
            let (log, _) = serial
                .run(
                    "debian",
                    PW,
                    "sudo journalctl -u systemd-networkd --no-pager -n 40 | cat",
                )
                .await?;
            anyhow::bail!("the guest did not take {ip}: {out}\nnetworkd: {log}");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}
