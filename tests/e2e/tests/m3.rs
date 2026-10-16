//! M3 end to end: VPCs, security groups, guardrails, SNAT and floating IPs on
//! the lab's OVN.
//!
//! A parent tenant P and its child C. In C: VPC `a` (routed to the lab's
//! external network, 10.78.0.0/24) with VMs a1 and a2, and VPC `b` with b1.
//! - a1 reaches a2 (same network, default group), but not b1 (other VPC).
//! - a1 reaches the outside (the lab host at 10.78.0.1) through SNAT.
//! - a2's floating IP answers SSH from here only while a2 is in a group that
//!   allows it, and not while P has a guardrail denying it, whatever C's
//!   groups say. C's admin cannot remove P's guardrail.

use std::time::{Duration, Instant};

use hm_e2e::Api;

/// Sends router advertisements for 2001:db8:66::/64 (autoconfiguration on)
/// to all nodes on the first non-loopback interface.
const ROGUE_RA: &str = r#"
import socket, struct, time
ifname = next(n for _, n in socket.if_nameindex() if n != "lo")
idx = socket.if_nametoindex(ifname)
s = socket.socket(socket.AF_INET6, socket.SOCK_RAW, socket.IPPROTO_ICMPV6)
s.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_MULTICAST_HOPS, 255)
s.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_MULTICAST_IF, idx)
ra = struct.pack("!BBHBBHII", 134, 0, 0, 64, 0, 1800, 0, 0)
prefix = struct.pack("!BBBBIII", 3, 4, 64, 0xC0, 86400, 14400, 0)
prefix += socket.inet_pton(socket.AF_INET6, "2001:db8:66::")
for _ in range(3):
    s.sendto(ra + prefix, ("ff02::1", 0, 0, idx))
    time.sleep(0.5)
print("sent on", ifname)
"#;
use reqwest::Method;
use serde_json::{Value, json};

const PASSWORD: &str = "e2e-password-1";
/// The lab's external network, shared by test runs.
const EXT_NAME: &str = "lab-external";

#[tokio::test]
async fn vpcs_security_groups_guardrails_and_floating_ips() -> anyhow::Result<()> {
    let Some(admin) = hm_e2e::connect().await? else {
        return Ok(());
    };
    let run = format!(
        "e2e{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs()
    );
    let root = admin.get("/auth/me").await?["tenant_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let p = admin
        .post(
            "/tenants",
            json!({"parent_id": root, "slug": format!("{run}-p"), "name": "P"}),
        )
        .await?["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let c = admin
        .post(
            "/tenants",
            json!({"parent_id": p, "slug": "c", "name": "C"}),
        )
        .await?["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let result = tokio::spawn({
        let (admin, run, p, c) = (admin.clone(), run.clone(), p.clone(), c.clone());
        async move { scenario(&admin, &run, &p, &c).await }
    })
    .await;

    // VMs first; deleting the tenants then removes their network setup.
    if let Ok(vms) = admin.get(&format!("/vms?tenant_id={c}")).await {
        for vm in vms.as_array().into_iter().flatten() {
            let (_, t) = admin
                .try_send(
                    Method::DELETE,
                    &format!("/vms/{}", vm["id"].as_str().unwrap()),
                    None,
                )
                .await?;
            if let Some(t) = t["task_id"].as_str() {
                let _ = admin.wait_task(t, Duration::from_secs(120)).await;
            }
        }
    }
    for t in [&c, &p] {
        let (st, body) = admin
            .try_send(Method::DELETE, &format!("/tenants/{t}"), None)
            .await?;
        assert_eq!(st, 204, "deleting tenant {t}: {body}");
    }
    match result {
        Ok(r) => r,
        Err(panic) => std::panic::resume_unwind(panic.into_panic()),
    }
}

/// The lab's external network, registered on first use.
async fn external_network(admin: &Api) -> anyhow::Result<String> {
    let nets = admin.get("/external-networks").await?;
    if let Some(n) = nets
        .as_array()
        .into_iter()
        .flatten()
        .find(|n| n["name"] == EXT_NAME)
    {
        return Ok(n["id"].as_str().unwrap().to_owned());
    }
    let n = admin
        .post(
            "/external-networks",
            json!({"name": EXT_NAME, "physical_network": "external", "cidr": "10.78.0.0/24",
                   "gateway": "10.78.0.1", "pool_start": "10.78.0.200", "pool_end": "10.78.0.250"}),
        )
        .await?;
    Ok(n["id"].as_str().unwrap().to_owned())
}

async fn create_vm(
    api: &Api,
    tenant: &str,
    image: &str,
    name: &str,
    network: &str,
    groups: &[&str],
) -> anyhow::Result<Value> {
    api.post(
        "/vms",
        json!({"tenant_id": tenant, "name": name, "vcpus": 1, "memory_mib": 512, "disk_gib": 3,
               "image_id": image, "password": "e2e-password", "network_id": network,
               "security_group_ids": groups}),
    )
    .await
}

/// Whether something answers SSH at `addr`.
async fn ssh_banner(addr: &str) -> bool {
    let attempt = async {
        let mut s = tokio::net::TcpStream::connect(addr).await.ok()?;
        let mut buf = [0u8; 64];
        let n = tokio::io::AsyncReadExt::read(&mut s, &mut buf).await.ok()?;
        Some(buf[..n].starts_with(b"SSH-"))
    };
    tokio::time::timeout(Duration::from_secs(3), attempt)
        .await
        .ok()
        .flatten()
        .unwrap_or(false)
}

/// Waits until SSH at `addr` is (or is not) reachable.
async fn wait_ssh(addr: &str, reachable: bool, why: &str) {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if ssh_banner(addr).await == reachable {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{addr} SSH should be {}: {why}",
            if reachable { "open" } else { "closed" }
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn scenario(admin: &Api, run: &str, p: &str, c: &str) -> anyhow::Result<()> {
    let base = admin.base.clone();
    let ext = external_network(admin).await?;
    let image = admin.image().await?;
    // P and C may use every cluster and the external network.
    let clusters: Vec<Value> = admin
        .get("/clusters")
        .await?
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].clone())
        .collect();
    for t in [p, c] {
        admin
            .send(
                Method::PUT,
                &format!("/tenants/{t}/grants"),
                Some(
                    json!({"clusters": clusters, "storage_pools": [], "external_networks": [ext]}),
                ),
            )
            .await?;
    }
    for (t, name) in [(p, "pat"), (c, "cid")] {
        admin
            .post(
                "/users",
                json!({"tenant_id": t, "username": name, "password": PASSWORD, "role": "admin"}),
            )
            .await?;
    }
    let pat = Api::login(&base, "pat", PASSWORD, Some(&format!("{run}-p"))).await?;
    let cid = Api::login(&base, "cid", PASSWORD, Some(&format!("{run}-p/c"))).await?;

    // C's network layout.
    let vpc_a = cid
        .post(
            "/vpcs",
            json!({"tenant_id": c, "name": "a", "external_network_id": ext}),
        )
        .await?;
    assert!(
        vpc_a["gateway_ip"]
            .as_str()
            .is_some_and(|ip| ip.starts_with("10.78.0.")),
        "{vpc_a}"
    );
    let vpc_b = cid
        .post("/vpcs", json!({"tenant_id": c, "name": "b"}))
        .await?;
    let net_a = cid
        .post(
            "/networks",
            json!({"vpc_id": vpc_a["id"], "name": "a", "cidr": "10.10.0.0/24"}),
        )
        .await?;
    let net_b = cid
        .post(
            "/networks",
            json!({"vpc_id": vpc_b["id"], "name": "b", "cidr": "10.20.0.0/24"}),
        )
        .await?;
    // Ranges may repeat across VPCs, not within one.
    let (st, _) = cid
        .try_send(
            Method::POST,
            "/networks",
            Some(json!({"vpc_id": vpc_a["id"], "name": "x", "cidr": "10.10.0.128/25"})),
        )
        .await?;
    assert_eq!(st, 409);
    let dup = cid
        .post(
            "/networks",
            json!({"vpc_id": vpc_b["id"], "name": "dup", "cidr": "10.10.0.0/24"}),
        )
        .await?;
    cid.send(
        Method::DELETE,
        &format!("/networks/{}", dup["id"].as_str().unwrap()),
        None,
    )
    .await?;

    let groups = cid.get(&format!("/security-groups?tenant_id={c}")).await?;
    let default = groups
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["name"] == "default")
        .expect("default group")["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let ssh = cid
        .post("/security-groups", json!({"tenant_id": c, "name": "ssh"}))
        .await?;
    let ssh_id = ssh["id"].as_str().unwrap().to_owned();
    cid.post(
        &format!("/security-groups/{ssh_id}/rules"),
        json!({"direction": "ingress", "protocol": "tcp", "port_min": 22, "remote_cidr": "0.0.0.0/0"}),
    )
    .await?;

    let (a_net, b_net) = (net_a["id"].as_str().unwrap(), net_b["id"].as_str().unwrap());
    let a1 = create_vm(&cid, c, &image, &format!("{run}-a1"), a_net, &[&default]).await?;
    let a2 = create_vm(&cid, c, &image, &format!("{run}-a2"), a_net, &[&default]).await?;
    let b1 = create_vm(&cid, c, &image, &format!("{run}-b1"), b_net, &[&default]).await?;
    for vm in [&a1, &a2, &b1] {
        cid.wait_vm(vm["id"].as_str().unwrap(), Duration::from_secs(120), |v| {
            v["status"] == "ready" && v["power"] == "running"
        })
        .await?;
    }
    let ip = |v: &Value| v["nics"][0]["ip"].as_str().unwrap().to_owned();
    let (a1_id, a2_id) = (a1["id"].as_str().unwrap(), a2["id"].as_str().unwrap());

    // Inside the VPC, out through SNAT, but not into another VPC.
    let mut serial = cid.console(a1_id, "serial").await?;
    let (out, rc) = serial
        .run(
            "debian",
            "e2e-password",
            &format!("ping -c3 -W2 {}", ip(&a2)),
        )
        .await?;
    assert_eq!(rc, 0, "a1 should reach a2:\n{out}");
    let (out, rc) = serial
        .run("debian", "e2e-password", "ping -c3 -W2 10.78.0.1")
        .await?;
    assert_eq!(rc, 0, "a1 should reach the outside through SNAT:\n{out}");
    let (out, rc) = serial
        .run(
            "debian",
            "e2e-password",
            &format!("ping -c2 -W2 {}", ip(&b1)),
        )
        .await?;
    assert_ne!(rc, 0, "a1 must not reach b1 in another VPC:\n{out}");

    // Networks are IPv4 only, and IPv6 must not become a way around that:
    // a guest reaches no neighbour over IPv6, and cannot advertise itself
    // as their router.
    let mut neighbour = cid.console(a2_id, "serial").await?;
    let (out, _) = neighbour
        .run(
            "debian",
            "e2e-password",
            // Let the kernel act on advertisements itself (systemd-networkd
            // turns that off and handles them in its own way), so that a2
            // would surely take one that reached it.
            "i=$(ip -o link | awk -F': ' '!/ lo:/{print $2; exit}'); \
             sudo sysctl -qw net.ipv6.conf.$i.accept_ra=2 net.ipv6.conf.$i.autoconf=1; \
             echo LL=$(ip -6 -o addr show dev $i scope link | awk '{print $4}' | cut -d/ -f1) \
             RA=$(cat /proc/sys/net/ipv6/conf/$i/accept_ra)",
        )
        .await?;
    assert!(
        out.contains("RA=2"),
        "a2 does not accept router advertisements:\n{out}"
    );
    let a2_ll = out
        .split("LL=")
        .last()
        .and_then(|l| l.split_whitespace().next())
        .filter(|l| l.starts_with("fe80"))
        .unwrap_or_else(|| panic!("a2 has no IPv6 link-local address:\n{out}"))
        .to_owned();
    let (out, rc) = serial
        .run(
            "debian",
            "e2e-password",
            &format!(
                "ping -6 -c2 -W2 {a2_ll}%$(ip -o link | awk -F': ' '!/ lo:/{{print $2; exit}}')"
            ),
        )
        .await?;
    assert_ne!(rc, 0, "a1 must not reach a2 over IPv6:\n{out}");
    let script = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, ROGUE_RA);
    let (out, rc) = serial
        .run(
            "debian",
            "e2e-password",
            &format!("echo {script} | base64 -d | sudo python3 -"),
        )
        .await?;
    assert_eq!(rc, 0, "sending the router advertisement:\n{out}");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let (out, _) = neighbour
        .run("debian", "e2e-password", "ip -6 addr; ip -6 route")
        .await?;
    assert!(
        !out.contains("2001:db8:66"),
        "a2 took a1's router advertisement:\n{out}"
    );
    drop(neighbour);
    drop(serial);

    // A floating IP for a2: closed until a group allows SSH.
    let nic = a2["nics"][0]["id"].as_str().unwrap();
    let fip = cid
        .post(
            "/floating-ips",
            json!({"tenant_id": c, "external_network_id": ext, "nic_id": nic}),
        )
        .await?;
    let addr = format!("{}:22", fip["ip"].as_str().unwrap());
    let set_groups = |groups: Vec<String>| {
        let cid = cid.clone();
        let path = format!("/vms/{a2_id}/nics/{nic}/security-groups");
        async move {
            cid.send(
                Method::PUT,
                &path,
                Some(json!({"security_group_ids": groups})),
            )
            .await
        }
    };
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        !ssh_banner(&addr).await,
        "the default group must not admit SSH from outside"
    );
    set_groups(vec![default.clone(), ssh_id.clone()]).await?;
    wait_ssh(&addr, true, "a2 joined the ssh group").await;

    // P's guardrail beats C's group, and C cannot remove it.
    let policies = pat
        .post(
            &format!("/tenants/{p}/network-policies"),
            json!({"direction": "ingress", "protocol": "tcp", "port_min": 22, "description": "no SSH from anywhere"}),
        )
        .await?;
    let policy = policies.as_array().unwrap()[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    wait_ssh(&addr, false, "P's guardrail denies SSH").await;
    let seen = cid.get(&format!("/tenants/{c}/network-policies")).await?;
    assert_eq!(seen[0]["inherited"], true, "{seen}");
    let (st, _) = cid
        .try_send(Method::DELETE, &format!("/network-policies/{policy}"), None)
        .await?;
    assert!(
        st == 403 || st == 404,
        "C's admin removed P's guardrail ({st})"
    );
    pat.send(Method::DELETE, &format!("/network-policies/{policy}"), None)
        .await?;
    wait_ssh(&addr, true, "the guardrail is gone").await;

    // Leaving the group closes it again.
    set_groups(vec![default.clone()]).await?;
    wait_ssh(&addr, false, "a2 left the ssh group").await;

    // A VPC with floating IPs in use cannot be disconnected.
    let (st, _) = cid
        .try_send(
            Method::PATCH,
            &format!("/vpcs/{}", vpc_a["id"].as_str().unwrap()),
            Some(json!({"external_network_id": null})),
        )
        .await?;
    assert_eq!(st, 409);
    Ok(())
}
