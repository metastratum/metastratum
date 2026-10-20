//! What differs between guests' operating systems, for Windows: its image's
//! cloudbase-init (see `deploy/images/windows`) takes the VM's password as it
//! is, in its user data (`users`), where Linux takes a hash. That password
//! is kept sealed (`CloudInit::first_boot_password`) and put in the user data
//! hosts get only until the first boot has set it: the guest says so with a
//! file the user data's last command writes, which the controller asks its
//! guest agent for ([`first_boots`]). Then it is dropped, and the VM's seed
//! made again without it.

use std::time::Duration;

use anyhow::{Context, Result};
use mst_core::ids::VmId;
use mst_core::vm::{GuestOs, VmSpec};
use serde_json::json;

use crate::state::AppState;

/// Written as cloudbase-init has run the user data, password set, by VMs
/// made before the marker named its VM.
const LEGACY_FIRST_BOOT_DONE: &str = r"C:\ProgramData\metastratum-first-boot.txt";

/// Written as cloudbase-init has run the VM's user data, its account there
/// with its password: named after the VM, as a clone's or restored VM's
/// disk holds its source's.
pub fn first_boot_done(vm: VmId) -> String {
    format!(r"C:\ProgramData\metastratum-first-boot-{vm}.txt")
}

/// How often VMs whose first boot is pending are asked.
const EVERY: Duration = Duration::from_secs(30);

/// User data for a Windows VM, but its password: marks its first boot done
/// once the account is there (`users` runs before `runcmd`, and goes on
/// after failing: the marker is written only if the account exists).
pub fn windows_user_data(vm: VmId, account: &str) -> String {
    let marker = first_boot_done(vm);
    let doc =
        json!({ "runcmd": [format!("net user \"{account}\" >nul 2>&1 && echo done> {marker}")] });
    format!("#cloud-config\n{doc}\n")
}

/// What a VM's password is sealed under: its own, so it opens for no other.
fn password_context(vm: VmId) -> String {
    format!("vm.first_boot_password:{vm}")
}

/// Seals a Windows VM's password until its first boot.
pub fn seal_password(vm: VmId, password: &str) -> String {
    crate::secrets::seal(&password_context(vm), password)
}

/// The spec as the VM's host gets it (JSON): with a Windows VM's password
/// in its user data while its first boot is pending, never sealed.
pub fn host_spec_json(vm: VmId, spec: &VmSpec) -> Result<String> {
    let mut spec = spec.clone();
    if let Some(ci) = &mut spec.cloud_init
        && let Some(sealed) = ci.first_boot_password.take()
        && spec.guest_os == GuestOs::Windows
    {
        let password = crate::secrets::open(&password_context(vm), &sealed)?;
        ci.user_data = with_password(&ci.user_data, ci.windows_account(), &password)?;
    }
    Ok(serde_json::to_string(&spec)?)
}

/// `#cloud-config` user data (as the controller writes it: JSON) with the
/// account's password set; cloudbase-init makes the account if it is not
/// there, an administrator.
fn with_password(user_data: &str, account: &str, password: &str) -> Result<String> {
    let body = user_data
        .strip_prefix("#cloud-config\n")
        .context("a Windows VM's user data is not the controller's")?;
    let mut doc: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(body).context("a Windows VM's user data is not the controller's")?;
    let mut user = json!({ "name": account, "passwd": password });
    if !account.eq_ignore_ascii_case(mst_core::vm::WINDOWS_ADMINISTRATOR) {
        user["groups"] = json!(["Administrators"]);
    }
    doc.insert("users".into(), json!([user]));
    Ok(format!(
        "#cloud-config\n{}\n",
        serde_json::Value::Object(doc)
    ))
}

/// On the leader: Windows VMs whose first boot is pending are asked whether
/// it is done; once it is, their password is dropped.
pub async fn first_boots(state: AppState) {
    loop {
        tokio::time::sleep(EVERY).await;
        let pending = match mst_db::vms::first_boots_pending(&state.db).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "listing VMs whose first boot is pending");
                continue;
            }
        };
        for vm in pending {
            if let Err(e) = settle(&state, vm).await {
                tracing::debug!(vm = %vm, error = %format!("{e:#}"), "asking about a first boot");
            }
        }
    }
}

/// Drops the VM's password if its first boot is done.
async fn settle(state: &AppState, id: VmId) -> Result<()> {
    let vm = mst_db::vms::get(&state.db, id).await?;
    let Some(host) = vm.host_id else {
        return Ok(());
    };
    if vm.power != "running" || vm.status != "ready" {
        return Ok(());
    }
    // VMs made before the marker named its VM write the old one.
    let legacy = vm
        .spec
        .0
        .cloud_init
        .as_ref()
        .is_some_and(|c| c.user_data.contains("metastratum-first-boot.txt"));
    let path = match legacy {
        true => LEGACY_FIRST_BOOT_DONE.to_owned(),
        false => first_boot_done(id),
    };
    let cmd = mst_proto::command::Cmd::GuestFileExists(mst_proto::GuestFileExists {
        vm_id: id.to_string(),
        path,
    });
    let out = state
        .agents
        .send(host, cmd, Duration::from_secs(30))
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    if out != "true" {
        return Ok(());
    }
    let mut tx = state.db.begin().await?;
    // As it is now (locked), should it have changed meanwhile.
    let vm = mst_db::vms::get_for_update(&mut tx, id).await?;
    // Moved, or migrating, meanwhile: asked again later.
    if vm.status != "ready" || vm.host_id != Some(host) {
        return Ok(());
    }
    let mut spec = vm.spec.0.clone();
    let Some(ci) = &mut spec.cloud_init else {
        return Ok(());
    };
    if ci.first_boot_password.take().is_none() {
        return Ok(());
    }
    let generation = mst_db::vms::redefine(&mut tx, id, &spec).await?;
    // Its seed made again, without the password.
    if let Some(host) = vm.host_id {
        crate::api::define_on_host(state, &mut tx, &vm, host, &spec, generation)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e.message))?;
    }
    tx.commit().await?;
    tracing::info!(vm = %id, "first boot done: let go of the VM's password");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_password_goes_in_users_beside_the_rest() {
        let doc = |ud: String| -> serde_json::Value {
            serde_json::from_str(ud.strip_prefix("#cloud-config\n").unwrap()).unwrap()
        };
        let vm = VmId::new();
        let ud = doc(with_password(
            &windows_user_data(vm, "Administrator"),
            "Administrator",
            "pa\"ss",
        )
        .unwrap());
        assert_eq!(ud["users"][0]["name"], "Administrator");
        assert_eq!(ud["users"][0]["passwd"], "pa\"ss");
        // It is one already.
        assert!(ud["users"][0].get("groups").is_none());
        let named = doc(with_password(&windows_user_data(vm, "ops"), "ops", "x").unwrap());
        assert_eq!(named["users"][0]["name"], "ops");
        assert_eq!(named["users"][0]["groups"], json!(["Administrators"]));
        // The first boot is marked done once the account is there, named
        // after the VM (a clone's disk holds its source's marker).
        assert_eq!(
            ud["runcmd"][0],
            format!(
                "net user \"Administrator\" >nul 2>&1 && echo done> {}",
                first_boot_done(vm)
            ),
        );
        assert!(first_boot_done(vm).contains(&vm.to_string()));
        // Someone's own user data is theirs: no password goes in.
        assert!(with_password("#ps1_sysnative\nwhoami", "Administrator", "x").is_err());
    }
}
