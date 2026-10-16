//! Per-VM limits a cluster sets: disk throughput (QEMU throttling) and
//! network traffic (OVN QoS), measured inside guests, and lifted again on
//! running VMs.

use std::time::Duration;

use mst_e2e::Api;
use reqwest::Method;
use serde_json::{Value, json};

/// Receives one connection on port 5001 and discards what it sends.
const RECEIVER: &str = r#"
import socket
s = socket.socket()
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("", 5001))
s.listen()
while True:
    c, _ = s.accept()
    while c.recv(1 << 16):
        pass
    c.close()
"#;

/// Sends 25 MB to the receiver and prints how long it took.
fn sender(to: &str) -> String {
    format!(
        r#"
import socket, time
c = socket.create_connection(("{to}", 5001), timeout=60)
block = b"x" * (1 << 16)
t = time.time()
for _ in range(400):
    c.sendall(block)
c.shutdown(socket.SHUT_WR)
c.recv(1)
print("ELAPSED=%.2f" % (time.time() - t))
"#
    )
}

fn b64(s: &str) -> String {
    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, s)
}

/// The number after `key=` in a console transcript (its last occurrence).
fn number(out: &str, key: &str) -> f64 {
    out.rsplit(key)
        .next()
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.trim_end_matches(',').parse().ok())
        .unwrap_or_else(|| panic!("no {key} in:\n{out}"))
}

/// Seconds a direct (uncached) write of 100 MB takes inside the guest.
async fn disk_seconds(serial: &mut mst_e2e::Console) -> anyhow::Result<f64> {
    let (out, rc) = serial
        .run(
            "debian",
            "e2e-password",
            "dd if=/dev/zero of=/var/tmp/limit-test bs=1M count=100 oflag=direct 2>&1 | tail -1 | sed 's/.*copied, /SECONDS=/'; rm -f /var/tmp/limit-test",
        )
        .await?;
    assert_eq!(rc, 0, "{out}");
    Ok(number(&out, "SECONDS="))
}

async fn net_seconds(serial: &mut mst_e2e::Console, to: &str) -> anyhow::Result<f64> {
    let (out, rc) = serial
        .run(
            "debian",
            "e2e-password",
            &format!("echo {} | base64 -d | python3 -", b64(&sender(to))),
        )
        .await?;
    assert_eq!(rc, 0, "{out}");
    Ok(number(&out, "ELAPSED="))
}

async fn set_limits(api: &Api, clusters: &[Value], limits: Value) -> anyhow::Result<()> {
    for c in clusters {
        let mut body = json!({
            "name": c["name"], "cpu_overcommit": c["cpu_overcommit"],
            "memory_overcommit": c["memory_overcommit"], "ha_enabled": c["ha_enabled"],
        });
        for (k, v) in limits.as_object().unwrap() {
            body[k] = v.clone();
        }
        api.send(
            Method::PUT,
            &format!("/clusters/{}", c["id"].as_str().unwrap()),
            Some(body),
        )
        .await?;
    }
    Ok(())
}

#[tokio::test]
async fn vms_are_held_to_their_clusters_limits() -> anyhow::Result<()> {
    let Some(api) = mst_e2e::connect().await? else {
        return Ok(());
    };
    let clusters = api.get("/clusters").await?.as_array().unwrap().clone();
    let restore = |c: &Value| {
        json!({"vm_net_mbps": c["vm_net_mbps"], "vm_disk_iops": c["vm_disk_iops"],
               "vm_disk_mbps": c["vm_disk_mbps"]})
    };
    set_limits(
        &api,
        &clusters,
        json!({"vm_net_mbps": 50, "vm_disk_mbps": 20}),
    )
    .await?;
    let image = api.image().await?;
    let run = format!("lim-{}", std::process::id());
    let mut vms = vec![];
    for n in ["a", "b"] {
        let vm = api
            .post(
                "/vms",
                json!({"name": format!("{run}-{n}"), "vcpus": 1, "memory_mib": 1024,
                       "disk_gib": 5, "image_id": image, "password": "e2e-password"}),
            )
            .await?;
        vms.push(vm["id"].as_str().unwrap().to_owned());
    }

    let result = async {
        for id in &vms {
            api.wait_vm(id, Duration::from_secs(180), |v| {
                v["status"] == "ready" && v["power"] == "running"
            })
            .await?;
        }
        let b = api.get(&format!("/vms/{}", vms[1])).await?;
        let b_ip = b["nics"][0]["ip"].as_str().unwrap().to_owned();
        let mut on_b = api.console(&vms[1], "serial").await?;
        let (out, rc) = on_b
            .run(
                "debian",
                "e2e-password",
                &format!(
                    "echo {} | base64 -d > /tmp/rx.py; nohup python3 /tmp/rx.py >/dev/null 2>&1 & sleep 1",
                    b64(RECEIVER)
                ),
            )
            .await?;
        assert_eq!(rc, 0, "{out}");
        let mut on_a = api.console(&vms[0], "serial").await?;

        // 100 MB at 20 MB/s, 25 MB at 50 Mbit/s: about 5 and 4 seconds.
        let disk_limited = disk_seconds(&mut on_a).await?;
        let net_limited = net_seconds(&mut on_a, &b_ip).await?;
        eprintln!("limited: disk {disk_limited:.1}s, network {net_limited:.1}s");
        assert!(disk_limited >= 4.0, "100 MB took {disk_limited}s at 20 MB/s");
        assert!(net_limited >= 3.0, "25 MB took {net_limited}s at 50 Mbit/s");

        // Lifted on the running VMs, the same runs get faster: the limits
        // were what held them back.
        set_limits(
            &api,
            &clusters,
            json!({"vm_net_mbps": null, "vm_disk_iops": null, "vm_disk_mbps": null}),
        )
        .await?;
        tokio::time::sleep(Duration::from_secs(5)).await;
        let disk_free = disk_seconds(&mut on_a).await?;
        let net_free = net_seconds(&mut on_a, &b_ip).await?;
        eprintln!("unlimited: disk {disk_free:.1}s, network {net_free:.1}s");
        assert!(disk_free < disk_limited / 2.0, "{disk_free}s vs {disk_limited}s");
        assert!(net_free < net_limited / 2.0, "{net_free}s vs {net_limited}s");
        anyhow::Ok(())
    }
    .await;

    for c in &clusters {
        set_limits(&api, std::slice::from_ref(c), restore(c)).await?;
    }
    for id in &vms {
        let _ = api
            .try_send(Method::DELETE, &format!("/vms/{id}"), None)
            .await;
    }
    result
}
