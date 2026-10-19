//! Windows VMs, from images built with `deploy/images/windows/build.sh` and
//! served where the hosts reach them: Windows Server from
//! `MST_E2E_WINDOWS_IMAGE`, Windows 11 from `MST_E2E_WINDOWS11_IMAGE` (each
//! skipped without). The images are added once, and kept for later runs
//! (they are large).
//!
//! The VM's password reaches Windows at its first boot, and only then: the
//! controller lets go of it once the guest says the first boot is done.
//! Windows Server's VM is signed in to as its image's default account
//! (Administrator) on the serial console. Windows 11's image has none: its
//! VM names an account, made as it first boots, and runs with Secure Boot
//! and a TPM, as the image says.

use std::time::Duration;

use mst_e2e::Api;
use serde_json::{Value, json};

const PW: &str = "E2e-Windows-pw1";

struct Edition {
    image: &'static str,
    /// Where the image is served.
    url_var: &'static str,
    admin_user: Option<&'static str>,
    /// Its VMs' boot security, by default.
    secure: bool,
    memory_mib: u64,
    /// The account the VM is made with (none: the image's).
    username: Option<&'static str>,
    /// What `whoami` says once signed in on the serial console (Windows
    /// Server's Special Administration Console), if it has one.
    sac_whoami: Option<&'static str>,
}

const SERVER: Edition = Edition {
    image: "e2e-windows-server",
    url_var: "MST_E2E_WINDOWS_IMAGE",
    admin_user: Some("Administrator"),
    secure: false,
    memory_mib: 2048,
    username: None,
    // `<host>\administrator`.
    sac_whoami: Some("\\administrator"),
};

const WINDOWS_11: Edition = Edition {
    image: "e2e-windows-11",
    url_var: "MST_E2E_WINDOWS11_IMAGE",
    admin_user: None,
    secure: true,
    memory_mib: 4096,
    username: Some("e2e-ops"),
    sac_whoami: None,
};

#[tokio::test]
async fn windows_vms_get_their_account_and_password_once() -> anyhow::Result<()> {
    let Some(api) = mst_e2e::connect().await? else {
        return Ok(());
    };
    // One after the other: each takes much of a host.
    for edition in [SERVER, WINDOWS_11] {
        let Ok(url) = std::env::var(edition.url_var) else {
            eprintln!("{} not set; skipping {}", edition.url_var, edition.image);
            continue;
        };
        let run = mst_e2e::run_name(edition.image.trim_start_matches("e2e-"));
        let image = image(&api, &edition, &url).await?;
        mst_e2e::cleaned_up(&api, &run, scenario(&api, &run, &edition, &image)).await?;
    }
    Ok(())
}

async fn scenario(api: &Api, run: &str, edition: &Edition, image: &Value) -> anyhow::Result<()> {
    let disk_gib = image["virtual_bytes"].as_u64().unwrap().div_ceil(1 << 30);
    let body = |name: String, extra: Value| {
        let mut b = json!({"name": name, "vcpus": 2, "memory_mib": edition.memory_mib,
                           "disk_gib": disk_gib, "image_id": image["id"], "password": PW,
                           "migratable": false, "username": edition.username});
        b.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        b
    };
    let refused = |b: Value| async move {
        let (status, v) = api.try_send(reqwest::Method::POST, "/vms", Some(b)).await?;
        anyhow::ensure!(status == 400, "{status} {v}");
        anyhow::Ok(())
    };
    // A password Windows would not take is refused at once; so is a
    // Windows VM without one, or one holding its account's name.
    refused(body(format!("{run}-weak"), json!({"password": "weak"}))).await?;
    refused(body(format!("{run}-none"), json!({"password": null}))).await?;
    if let Some(user) = edition.username {
        refused(body(
            format!("{run}-name"),
            json!({"password": format!("{user}-X1!")}),
        ))
        .await?;
    }
    // An image without a default account needs one named.
    if edition.admin_user.is_none() {
        refused(body(format!("{run}-anon"), json!({"username": null}))).await?;
    }

    let vm = api.post("/vms", body(run.to_owned(), json!({}))).await?;
    let id = vm["id"].as_str().unwrap().to_owned();
    assert_eq!(vm["guest_os"], "windows", "{vm}");
    assert_eq!(vm["first_boot_pending"], true, "{vm}");
    let account = edition.username.or(edition.admin_user).unwrap();
    assert_eq!(vm["account"], account, "{vm}");
    assert_eq!(
        (&vm["secure_boot"], &vm["tpm"]),
        (&json!(edition.secure), &json!(edition.secure)),
        "as the image says: {vm}"
    );
    api.wait_task(vm["task_id"].as_str().unwrap(), Duration::from_secs(3600))
        .await?;

    // Its first boot (Windows specializing itself, cloudbase-init setting
    // the account up) sets the password; the controller lets go of it after.
    let v = api
        .wait_vm(&id, Duration::from_secs(1800), |v| {
            v["first_boot_pending"] == false
        })
        .await?;
    assert_eq!(v["power"], "running", "{v}");

    if let Some(whoami) = edition.sac_whoami {
        // Signed in with its password.
        let mut serial = api.console(&id, "serial").await?;
        serial.sac_run(account, PW, "whoami", whoami).await?;
    }
    Ok(())
}

/// The edition's image, added (and downloaded) on first use.
async fn image(api: &Api, edition: &Edition, url: &str) -> anyhow::Result<Value> {
    let name = edition.image;
    let find = || async {
        let images = api.get("/images").await?;
        anyhow::Ok(
            images
                .as_array()
                .into_iter()
                .flatten()
                .find(|i| i["name"] == name)
                .cloned(),
        )
    };
    if find().await?.is_none() {
        let added = api
            .post(
                "/images",
                json!({"name": name, "source_url": url, "format": "qcow2", "os": "windows",
                       "admin_user": edition.admin_user, "secure_boot": edition.secure,
                       "tpm": edition.secure}),
            )
            .await?;
        api.wait_image_ready(added["id"].as_str().unwrap(), Duration::from_secs(3600))
            .await?;
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(3600);
    loop {
        let image = find()
            .await?
            .ok_or_else(|| anyhow::anyhow!("{name} is gone"))?;
        match image["state"].as_str() {
            Some("ready") => {
                assert_eq!(image["os"], "windows", "{image}");
                assert_eq!(image["admin_user"].as_str(), edition.admin_user, "{image}");
                return Ok(image);
            }
            Some("failed") => anyhow::bail!("{name} could not be downloaded: {image}"),
            _ if std::time::Instant::now() > deadline => anyhow::bail!("{name} still downloading"),
            _ => tokio::time::sleep(Duration::from_secs(10)).await,
        }
    }
}
