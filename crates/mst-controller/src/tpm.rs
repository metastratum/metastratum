//! VMs' TPM state. It lives on the host the VM runs on (swtpm's, kept by
//! libvirt) and goes where the VM goes: live migration carries it by itself;
//! a VM started on another host gets it from the host it left, or, when that
//! one is gone (a host failed), from the copy the database keeps, sealed to
//! the VM. The leader refreshes that copy as the state changes ([`sync`]).

use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use mst_core::ids::{HostId, VmId};
use mst_proto::command::Cmd;
use serde::Deserialize;

use crate::state::AppState;

/// How often the copies are brought up to date.
const EVERY: Duration = Duration::from_secs(60);
const QUICK: Duration = Duration::from_secs(60);

/// What a VM's TPM state is sealed under: its own, so it opens for no other.
fn context(vm: VmId) -> String {
    format!("vm.tpm_state:{vm}")
}

/// A host's answer to `ExportTpm`.
#[derive(Deserialize)]
struct Exported {
    /// None when the host has no state for the VM.
    digest: Option<String>,
    /// Base64; left out when the digest is the one already known.
    state: Option<String>,
}

/// The VM's TPM state on `host`, if it has any: (base64 state, digest).
/// With `known`, the state is left out (`None`) when it has that digest.
async fn export(
    state: &AppState,
    host: HostId,
    vm: VmId,
    known: Option<&str>,
) -> Result<Option<(Option<String>, String)>> {
    let cmd = Cmd::ExportTpm(mst_proto::ExportTpm {
        vm_id: vm.to_string(),
        known: known.unwrap_or_default().to_owned(),
    });
    let out: Exported = state
        .agents
        .query(host, cmd, QUICK)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    Ok(out.digest.map(|d| (out.state, d)))
}

async fn import(state: &AppState, host: HostId, vm: VmId, b64: &str, digest: &str) -> Result<()> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .context("TPM state is not base64")?;
    let cmd = Cmd::ImportTpm(mst_proto::ImportTpm {
        vm_id: vm.to_string(),
        state: bytes,
        digest: digest.to_owned(),
    });
    state
        .agents
        .call(host, cmd, QUICK)
        .await
        .map_err(|e| anyhow::anyhow!(e))
}

/// Keeps the copy in the database.
async fn keep(state: &AppState, vm: VmId, b64: &str, digest: &str) -> Result<()> {
    let sealed = crate::secrets::seal(&context(vm), b64);
    mst_db::vms::set_tpm_state(&state.db, vm, &sealed, digest).await?;
    Ok(())
}

/// The copy in the database: (base64 state, digest).
async fn kept(state: &AppState, vm: VmId) -> Result<Option<(String, String)>> {
    match mst_db::vms::tpm_state(&state.db, vm).await? {
        Some((sealed, digest)) => Ok(Some((crate::secrets::open(&context(vm), &sealed)?, digest))),
        None => Ok(None),
    }
}

/// Before the VM starts on `host`: its TPM state put there. `from` is the
/// host it ran on before, when that is another one: its state comes from
/// there, or from the copy if that host does not answer. A VM that stays
/// keeps what its host has, or gets the copy if that has nothing (it was
/// set up there again). Without either, it starts with a new TPM.
pub async fn carry(state: &AppState, vm: VmId, host: HostId, from: Option<HostId>) -> Result<()> {
    let mut newest = None;
    match from {
        Some(from) => match export(state, from, vm, None).await {
            Ok(Some((Some(b64), digest))) => {
                keep(state, vm, &b64, &digest).await?;
                newest = Some((b64, digest));
            }
            Ok(_) => tracing::info!(%vm, %from, "the host the VM left has no TPM state for it"),
            Err(e) => {
                tracing::info!(%vm, %from, error = %format!("{e:#}"), "the host the VM left does not answer: it gets the TPM state last copied")
            }
        },
        None => {
            let known = mst_db::vms::tpm_state(&state.db, vm)
                .await?
                .map(|(_, digest)| digest);
            if export(state, host, vm, known.as_deref()).await?.is_some() {
                // What its host has is the latest.
                return Ok(());
            }
        }
    }
    let newest = match newest {
        Some(n) => Some(n),
        None => kept(state, vm).await?,
    };
    let Some((b64, digest)) = newest else {
        tracing::info!(%vm, %host, "no TPM state to carry: the VM starts with a new TPM");
        return Ok(());
    };
    import(state, host, vm, &b64, &digest).await?;
    tracing::info!(%vm, %host, %digest, "TPM state carried");
    Ok(())
}

/// Removes the VM's TPM state from its host and the copy (its TPM was
/// removed: one added later is new).
pub async fn discard(state: &AppState, vm: VmId, host: HostId) -> Result<()> {
    let cmd = Cmd::ImportTpm(mst_proto::ImportTpm {
        vm_id: vm.to_string(),
        state: vec![],
        digest: String::new(),
    });
    state
        .agents
        .call(host, cmd, QUICK)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    mst_db::vms::drop_tpm_state(&state.db, vm).await?;
    Ok(())
}

/// On the leader: VMs' TPM state copied to the database as it changes.
pub async fn sync(state: AppState) {
    loop {
        tokio::time::sleep(EVERY).await;
        let vms = match mst_db::vms::with_tpm(&state.db).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "listing VMs with a TPM");
                continue;
            }
        };
        for (vm, host) in vms {
            if let Err(e) = refresh(&state, vm, host).await {
                tracing::debug!(%vm, error = %format!("{e:#}"), "copying TPM state");
            }
        }
    }
}

async fn refresh(state: &AppState, vm: VmId, host: HostId) -> Result<()> {
    let known = mst_db::vms::tpm_state(&state.db, vm)
        .await?
        .map(|(_, digest)| digest);
    if let Some((Some(b64), digest)) = export(state, host, vm, known.as_deref()).await? {
        keep(state, vm, &b64, &digest).await?;
        tracing::debug!(%vm, "TPM state copied");
    }
    Ok(())
}
