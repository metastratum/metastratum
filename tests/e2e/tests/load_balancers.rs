//! Load balancers: an address on the lab's external network spreads HTTP
//! connections from here over two VMs, and leaves out one that stops
//! answering; an address inside the VPC serves its VMs; members change; a
//! deleted load balancer stops answering. The VPC cannot drop its external
//! network, nor the network go, while load balancers use them.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use mst_e2e::Api;
use reqwest::Method;
use serde_json::{Value, json};

const PASSWORD: &str = "e2e-password";
const BOOT: Duration = Duration::from_secs(5 * 60);
/// The lab's external network, shared by test runs (as in m3).
const EXT_NAME: &str = "lab-external";

#[tokio::test]
async fn load_balancers_spread_check_and_follow_members() -> anyhow::Result<()> {
    let Some(admin) = mst_e2e::connect().await? else {
        return Ok(());
    };
    let run = format!(
        "e2e-lb-{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs()
    );
    let root = admin.get("/auth/me").await?["tenant_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let t = admin
        .post(
            "/tenants",
            json!({"parent_id": root, "slug": run, "name": "LB"}),
        )
        .await?["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let result = scenario(&admin, &run, &t).await;

    // VMs first; deleting the tenant then removes its load balancers and
    // network setup.
    if let Ok(vms) = admin.get(&format!("/vms?tenant_id={t}")).await {
        for vm in vms.as_array().into_iter().flatten() {
            let (_, task) = admin
                .try_send(
                    Method::DELETE,
                    &format!("/vms/{}", vm["id"].as_str().unwrap()),
                    None,
                )
                .await?;
            if let Some(task) = task["task_id"].as_str() {
                let _ = admin.wait_task(task, Duration::from_secs(120)).await;
            }
        }
    }
    let (st, body) = admin
        .try_send(Method::DELETE, &format!("/tenants/{t}"), None)
        .await?;
    assert_eq!(st, 204, "deleting the tenant: {body}");
    result
}

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

async fn sh(api: &Api, vm: &str, cmd: &str) -> anyhow::Result<String> {
    let mut serial = api.console(vm, "serial").await?;
    let (out, code) = serial.run("debian", PASSWORD, cmd).await?;
    anyhow::ensure!(code == 0, "{cmd} failed ({code}): {out}");
    Ok(out)
}

/// What answers at `url`, one new connection per request.
async fn fetch(url: &str) -> Option<String> {
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .timeout(Duration::from_secs(3))
        .build()
        .ok()?;
    let body = client.get(url).send().await.ok()?.text().await.ok()?;
    Some(body.trim().to_owned())
}

/// Requests `url` until the set of answers seen in a row of `n` is `want`.
async fn wait_answers(url: &str, want: &[&str], n: usize, why: &str) {
    let want: BTreeSet<String> = want.iter().map(|s| s.to_string()).collect();
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut last = BTreeSet::new();
    while Instant::now() < deadline {
        let mut seen = BTreeSet::new();
        for _ in 0..n {
            seen.insert(fetch(url).await.unwrap_or_else(|| "(nothing)".into()));
        }
        if seen == want {
            return;
        }
        last = seen;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    panic!("{url}: wanted {want:?}, got {last:?}: {why}");
}

async fn scenario(admin: &Api, run: &str, t: &str) -> anyhow::Result<()> {
    let ext = external_network(admin).await?;
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
            Some(json!({"clusters": clusters, "storage_pools": [], "external_networks": [ext]})),
        )
        .await?;
    let image = admin.image().await?;
    let vpc = admin
        .post(
            "/vpcs",
            json!({"tenant_id": t, "name": "web", "external_network_id": ext}),
        )
        .await?;
    let vpc = vpc["id"].as_str().unwrap().to_owned();
    let net = admin
        .post(
            "/networks",
            json!({"vpc_id": vpc, "name": "web", "cidr": "10.30.0.0/24"}),
        )
        .await?;
    let net = net["id"].as_str().unwrap().to_owned();
    // The members let anyone reach port 8080; the default group lets the
    // tenant's VMs talk among themselves.
    let groups = admin
        .get(&format!("/security-groups?tenant_id={t}"))
        .await?;
    let default = groups
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["name"] == "default")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let web = admin
        .post("/security-groups", json!({"tenant_id": t, "name": "web"}))
        .await?;
    let web = web["id"].as_str().unwrap().to_owned();
    admin
        .post(
            &format!("/security-groups/{web}/rules"),
            json!({"direction": "ingress", "protocol": "tcp", "port_min": 8080, "port_max": 8080,
                   "remote_cidr": "0.0.0.0/0"}),
        )
        .await?;

    // Each can be read on its own (as Terraform does).
    for (path, id) in [
        ("vpcs", &vpc),
        ("networks", &net),
        ("security-groups", &web),
    ] {
        let one = admin.get(&format!("/{path}/{id}")).await?;
        assert_eq!(one["id"].as_str(), Some(id.as_str()), "{path}: {one}");
    }

    let mut vms = vec![];
    for (name, groups) in [
        ("w1", vec![&default, &web]),
        ("w2", vec![&default, &web]),
        ("client", vec![&default]),
    ] {
        let vm = admin
            .post(
                "/vms",
                json!({"tenant_id": t, "name": format!("{run}-{name}"), "vcpus": 1, "memory_mib": 512,
                       "disk_gib": 3, "image_id": image, "password": PASSWORD, "network_id": net,
                       "security_group_ids": groups}),
            )
            .await?;
        vms.push(vm);
    }
    for vm in &vms {
        admin
            .wait_task(vm["task_id"].as_str().unwrap(), BOOT)
            .await?;
    }
    let id = |i: usize| vms[i]["id"].as_str().unwrap().to_owned();
    let (w1, w2, client) = (id(0), id(1), id(2));
    let (n1, n2) = (format!("{run}-w1"), format!("{run}-w2"));
    // A web server on each member that answers with its name.
    for (vm, name) in [(&w1, &n1), (&w2, &n2)] {
        sh(
            admin,
            vm,
            &format!(
                "mkdir -p /tmp/w && echo {name} > /tmp/w/index.html && cd /tmp/w && \
                 (setsid nohup python3 -m http.server 8080 >/dev/null 2>&1 &) && sleep 1"
            ),
        )
        .await?;
    }

    // On the external network: reachable from here, spread over both.
    let lb = admin
        .post(
            "/load-balancers",
            json!({"tenant_id": t, "name": "web", "vpc_id": vpc,
                   "listeners": [{"protocol": "tcp", "port": 80, "target_port": 8080}],
                   "member_vm_ids": [w1, w2]}),
        )
        .await?;
    assert_eq!(lb["external"], true, "{lb}");
    let lb_id = lb["id"].as_str().unwrap().to_owned();
    let url = format!("http://{}/", lb["ip"].as_str().unwrap());
    wait_answers(&url, &[&n1, &n2], 12, "both members answer").await;

    // One stops answering: health checks leave it out.
    sh(admin, &w2, "pkill -f http.server").await?;
    wait_answers(&url, &[&n1], 12, "w2 is left out once it stops answering").await;

    // The VPC keeps its way out while the address is on it.
    let (st, body) = admin
        .try_send(
            Method::PATCH,
            &format!("/vpcs/{vpc}"),
            Some(json!({"external_network_id": null})),
        )
        .await?;
    assert_eq!(st, 409, "{body}");

    // Inside the VPC, for its VMs: the client reaches w1 through it.
    let inner = admin
        .post(
            "/load-balancers",
            json!({"tenant_id": t, "name": "inner", "vpc_id": vpc, "network_id": net,
                   "listeners": [{"protocol": "tcp", "port": 8000, "target_port": 8080}],
                   "member_vm_ids": [w1]}),
        )
        .await?;
    assert_eq!(inner["external"], false, "{inner}");
    let inner_ip = inner["ip"].as_str().unwrap().to_owned();
    let fetch_inside = format!(
        "python3 -c \"import urllib.request; print('got', urllib.request.urlopen('http://{inner_ip}:8000/', timeout=5).read().decode())\""
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match sh(admin, &client, &fetch_inside).await {
            Ok(out) if out.contains(&format!("got {n1}")) => break,
            other if Instant::now() > deadline => panic!("inside the VPC: {other:?}"),
            _ => tokio::time::sleep(Duration::from_secs(2)).await,
        }
    }
    let (st, body) = admin
        .try_send(Method::DELETE, &format!("/networks/{net}"), None)
        .await?;
    assert_eq!(st, 409, "{body}");

    // Members change: w2 is back and answering, w1 leaves.
    sh(
        admin,
        &w2,
        "cd /tmp/w && (setsid nohup python3 -m http.server 8080 >/dev/null 2>&1 &) && sleep 1",
    )
    .await?;
    let (_, lb) = admin
        .send(
            Method::PUT,
            &format!("/load-balancers/{lb_id}"),
            Some(json!({"name": "web", "health_check": true,
                        "listeners": [{"protocol": "tcp", "port": 80, "target_port": 8080}],
                        "member_vm_ids": [w2]})),
        )
        .await?;
    assert_eq!(lb["members"].as_array().unwrap().len(), 1, "{lb}");
    wait_answers(&url, &[&n2], 12, "only w2 is a member now").await;

    // Deleted: nothing answers there any more.
    admin
        .send(Method::DELETE, &format!("/load-balancers/{lb_id}"), None)
        .await?;
    wait_answers(&url, &["(nothing)"], 3, "the load balancer is gone").await;
    Ok(())
}
