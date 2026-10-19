//! Windows Server VMs, from an image built with
//! `deploy/images/windows/build.sh` and served where the hosts reach it:
//! `MST_E2E_WINDOWS_IMAGE` (skipped without). The image is added once, as
//! `e2e-windows-server`, and kept for later runs (it is large).
//!
//! The VM's password reaches Windows at its first boot, and only then: the
//! controller lets go of it once the guest says the first boot is done, and
//! the password then signs in on the serial console.

use std::time::Duration;

use mst_e2e::Api;
use serde_json::{Value, json};

const IMAGE: &str = "e2e-windows-server";
const PW: &str = "E2e-Windows-pw1";

#[tokio::test]
async fn windows_vms_get_their_password_once() -> anyhow::Result<()> {
    let Some(api) = mst_e2e::connect().await? else {
        return Ok(());
    };
    let Ok(url) = std::env::var("MST_E2E_WINDOWS_IMAGE") else {
        eprintln!("MST_E2E_WINDOWS_IMAGE not set; skipping");
        return Ok(());
    };
    let run = mst_e2e::run_name("windows");
    let image = image(&api, &url).await?;
    mst_e2e::cleaned_up(&api, &run, async {
        let disk_gib = image["virtual_bytes"].as_u64().unwrap().div_ceil(1 << 30);
        // A password Windows would not take is refused at once.
        let (status, _) = api
            .try_send(
                reqwest::Method::POST,
                "/vms",
                Some(
                    json!({"name": format!("{run}-weak"), "vcpus": 2, "memory_mib": 2048,
                            "disk_gib": disk_gib, "image_id": image["id"], "password": "weak"}),
                ),
            )
            .await?;
        assert_eq!(status, 400);

        let vm = api
            .post(
                "/vms",
                json!({"name": run, "vcpus": 2, "memory_mib": 2048, "disk_gib": disk_gib,
                       "image_id": image["id"], "password": PW, "migratable": false}),
            )
            .await?;
        let id = vm["id"].as_str().unwrap().to_owned();
        assert_eq!(vm["guest_os"], "windows", "{vm}");
        assert_eq!(vm["first_boot_pending"], true, "{vm}");
        api.wait_task(vm["task_id"].as_str().unwrap(), Duration::from_secs(3600))
            .await?;

        // Its first boot (Windows specializing itself, then cloudbase-init)
        // sets the password; the controller lets go of it after.
        let v = api
            .wait_vm(&id, Duration::from_secs(1800), |v| {
                v["first_boot_pending"] == false
            })
            .await?;
        assert_eq!(v["power"], "running", "{v}");

        let mut serial = api.console(&id, "serial").await?;
        // Signed in with its password (as the Administrator, whom `whoami`
        // names as `<host>\administrator`).
        serial
            .sac_run("Administrator", PW, "whoami", "\\administrator")
            .await?;
        Ok(())
    })
    .await
}

/// The Windows image, added (and downloaded) on first use.
async fn image(api: &Api, url: &str) -> anyhow::Result<Value> {
    let find = || async {
        let images = api.get("/images").await?;
        anyhow::Ok(
            images
                .as_array()
                .into_iter()
                .flatten()
                .find(|i| i["name"] == IMAGE)
                .cloned(),
        )
    };
    if find().await?.is_none() {
        let added = api
            .post(
                "/images",
                json!({"name": IMAGE, "source_url": url, "format": "qcow2", "os": "windows"}),
            )
            .await?;
        api.wait_task(
            added["task_id"].as_str().unwrap(),
            Duration::from_secs(3600),
        )
        .await?;
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(3600);
    loop {
        let image = find()
            .await?
            .ok_or_else(|| anyhow::anyhow!("{IMAGE} is gone"))?;
        match image["state"].as_str() {
            Some("ready") => {
                assert_eq!(image["os"], "windows", "{image}");
                return Ok(image);
            }
            Some("failed") => anyhow::bail!("{IMAGE} could not be downloaded: {image}"),
            _ if std::time::Instant::now() > deadline => anyhow::bail!("{IMAGE} still downloading"),
            _ => tokio::time::sleep(Duration::from_secs(10)).await,
        }
    }
}
