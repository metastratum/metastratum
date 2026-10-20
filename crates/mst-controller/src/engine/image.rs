//! Images: their file comes to a host as they are added (uploaded, or
//! downloaded by the controller: see `crate::receive`), which adds it to
//! the image store of the pool they are kept in (`image.store`): its own
//! local disk (the host picked at random), or a shared pool. Hosts attached to a shared
//! pool make disks from its copy directly; others get a copy in their local
//! store, from a host that has one (a host attached to the shared pool
//! first copies it into its own). Every copy is recorded (`image_copies`),
//! and each serves as a source for the next.
//!
//! Storing it records the image's digest: the SHA-256 of the disk it
//! holds. Every copy is checked against it where it is made, so a host
//! handing over something else (a compromised one) spreads nothing. An
//! image comes to a host only as it is added: with no copy reachable, no
//! VM is made from it (nothing downloads it again).

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use mst_core::ids::{HostId, ImageId, LOCAL_POOL, StoragePoolId, VmId};
use mst_db::hosts::Host;
use mst_db::images::{Copy, Image, Stored};
use mst_db::storage_pools::Pool;
use mst_proto::command::Cmd;
use mst_proto::{
    CopyImage, DeleteImage, EndExport, ExportImage, MigrationTls, PruneStore, StoreImage,
};

use super::TaskCtx;
use super::backup::send;
use crate::state::AppState;

/// Storing an image (checking and converting it) can take a while.
const STORE: Duration = Duration::from_secs(3 * 3600);
/// How long the host an image's file came to may be away before storing it
/// fails (its file is there alone).
const HOST_AWAY: Duration = Duration::from_secs(30 * 60);
/// So can copies.
const COPY: Duration = Duration::from_secs(3 * 3600);
const QUICK: Duration = Duration::from_secs(60);
/// How often a VM waiting for its image looks again.
const POLL: Duration = Duration::from_secs(3);
/// How often hosts' and shared pools' stores are rid of deleted images.
const PRUNE_EVERY: Duration = Duration::from_secs(3600);

/// What an agent says once its store holds an image.
const NOT_IN_STORE: &str = "is not in the image store";

fn target(ctx: &TaskCtx) -> Result<ImageId> {
    ctx.task
        .target_id
        .map(ImageId)
        .context("task has no target image")
}

/// Hosts that can take an image: connected, not fenced or decommissioned.
pub async fn store_hosts(state: &AppState) -> Result<Vec<Host>> {
    let mut out = vec![];
    for h in mst_db::hosts::list(&state.db).await? {
        if h.is_live() && !matches!(h.state.as_str(), "fenced" | "decommissioned") {
            out.push(h);
        }
    }
    Ok(out)
}

/// Whether the host reaches a shared pool: one attached to its cluster.
pub fn reaches(h: &Host, pool: &Pool) -> bool {
    h.cluster_id.is_some_and(|c| pool.clusters.contains(&c))
}

/// Has the host an image's file came to add it to the pool it is kept in.
pub async fn store(ctx: &TaskCtx) -> Result<()> {
    let db = &ctx.state.db;
    let id = target(ctx)?;
    let image = match mst_db::images::get(db, id).await {
        Ok(i) => i,
        // Deleted meanwhile.
        Err(mst_db::Error::Domain(mst_core::Error::NotFound { .. })) => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    if image.state != "storing" {
        // Done already (a resumed task).
        return Ok(());
    }
    let result = first_store(ctx, &image).await;
    if let Err(e) = &result {
        match mst_db::images::failed(db, id, &format!("{e:#}")).await {
            // What it left (it may have gone on, cut off from here as its
            // agent reconnected) goes: recorded nowhere, it would only hold
            // up copies to that host.
            Ok(true) => remove_leftover(&ctx.state, id).await,
            // Made ready by another run of this task meanwhile.
            Ok(false) => {}
            // It shows as stopped (no task is under way), and fails within
            // minutes (see `prune`).
            Err(f) => tracing::warn!(image = %id, error = %f, "recording a failed store"),
        }
    }
    result
}

/// Has the host an image's file came to remove what it left there (the
/// file, or a store cut off).
pub async fn remove_leftover(state: &AppState, id: ImageId) {
    let left = async {
        let image = mst_db::images::get(&state.db, id).await?;
        let Some(host) = image.download_host_id else {
            return anyhow::Ok(());
        };
        let pool = mst_db::storage_pools::get(&state.db, image.storage_pool_id).await?;
        let into = match pool.kind.as_str() {
            "local" => None,
            _ => Some(crate::api::agent_pool(&pool)?),
        };
        let path = mst_db::images::store_path(&state.db, image.tenant_id).await?;
        remove(state, &image, &path, host, into).await;
        Ok(())
    };
    if let Err(e) = left.await {
        tracing::info!(image = %id, error = %format!("{e:#}"), "removing what a failed image left");
    }
}

async fn first_store(ctx: &TaskCtx, image: &Image) -> Result<()> {
    let state = &ctx.state;
    let db = &state.db;
    let pool = mst_db::storage_pools::get(db, image.storage_pool_id).await?;
    // The tenant may have lost the pool since (a resumed task).
    if !crate::api::may_use_pool(state, image.tenant_id, &pool)
        .await
        .map_err(|e| anyhow!("{}", e.message))?
    {
        bail!("the tenant may no longer use storage pool {}", pool.name);
    }
    let shared = pool.kind != "local";
    let path = mst_db::images::store_path(db, image.tenant_id).await?;
    let want = image
        .download_host_id
        .context("no host has the image's file")?;
    // Where its file came to, which holds it alone: a host away for a while
    // (its agent restarting, the network) is waited for, and storing it is
    // asked again (the host keeps what it stored).
    let deadline = tokio::time::Instant::now() + HOST_AWAY;
    let (stored, host, at) = loop {
        let host = store_hosts(state).await?.into_iter().find(|h| h.id == want);
        let Some(host) = host else {
            if tokio::time::Instant::now() > deadline {
                bail!(
                    "the host the image's file came to has not been back for {} minutes",
                    HOST_AWAY.as_secs() / 60
                );
            }
            ctx.step("waiting for the host the image's file came to", 0.1)
                .await;
            tokio::time::sleep(Duration::from_secs(10)).await;
            continue;
        };
        ctx.step(&format!("storing on {}", host.name), 0.1).await;
        let (at, into) = match shared {
            true => (Copy::shared(pool.id), Some(crate::api::agent_pool(&pool)?)),
            false => (Copy::local(host.id), None),
        };
        match store_on(state, image, &path, host.id, into).await {
            Ok(stored) => break (stored, host, at),
            // Cut off from it: once it is back, asked again.
            Err(_)
                if !mst_db::hosts::get(db, want).await?.is_live()
                    && tokio::time::Instant::now() < deadline =>
            {
                tracing::info!(image = %image.id, host = %host.name, "the host storing an image went away; waiting for it");
            }
            Err(e) => return Err(e),
        }
    };
    if stored.digest.is_empty() {
        bail!("{} reported no digest for the image", host.name);
    }

    // Recorded only while it is being stored (which locks it): not over
    // what another run of this task recorded, or after it was deleted.
    // Then charged what it takes, stored (`image_gib`).
    let gib = (stored.size_bytes + (1 << 30) - 1) >> 30;
    let delta = [("image_gib".to_owned(), gib)].into_iter().collect();
    let mut tx = db.begin().await?;
    let recorded = match mst_db::images::stored(&mut tx, image.id, at, &stored).await {
        Ok(true) => mst_db::quotas::reserve(&mut tx, image.tenant_id, &delta)
            .await
            .map(|()| true),
        r => r,
    };
    match recorded {
        Ok(true) => {
            tx.commit().await?;
            Ok(())
        }
        // Gone, or recorded by another run: what the host holds is (or
        // was) the image's, which is not this run's to remove.
        Ok(false) => Ok(()),
        // Not kept (over quota): the host lets go of it as storing it
        // fails (see `store`).
        Err(e) => Err(e.into()),
    }
}

/// Has the host the image's file came to add it to its store, or
/// `pool`'s.
async fn store_on(
    state: &AppState,
    image: &Image,
    tenant_path: &str,
    host: HostId,
    pool: Option<mst_proto::Pool>,
) -> Result<Stored> {
    let cmd = Cmd::StoreImage(StoreImage {
        pool,
        image_id: image.id.to_string(),
        tenant_path: tenant_path.to_owned(),
        format: image.format.clone(),
        trusted: super::image_trusted(&state.db, image).await?,
        size: image.file_bytes.unwrap_or_default() as u64,
    });
    let out = send(state, host, cmd, STORE).await?;
    Ok(serde_json::from_str(&out)?)
}

/// Records a copy made by `host`; should the image be gone meanwhile, the
/// host removes it again.
async fn record(state: &AppState, image: &Image, tenant_path: &str, host: HostId) -> Result<()> {
    let mut conn = state.db.acquire().await?;
    let added = mst_db::images::add_copy(&mut conn, image.id, Copy::local(host)).await;
    drop(conn);
    match added {
        Err(mst_db::Error::Domain(mst_core::Error::NotFound { .. })) => {
            remove(state, image, tenant_path, host, None).await;
            bail!("the image was deleted meanwhile")
        }
        r => Ok(r?),
    }
}

/// Has the host remove a copy, in its store or `pool`'s (best effort: the
/// stores are pruned of deleted images every hour).
async fn remove(
    state: &AppState,
    image: &Image,
    tenant_path: &str,
    host: HostId,
    pool: Option<mst_proto::Pool>,
) {
    let cmd = Cmd::DeleteImage(DeleteImage {
        image_id: image.id.to_string(),
        pools: pool.into_iter().collect(),
        tenant_path: tenant_path.to_owned(),
    });
    let _ = send(state, host, cmd, QUICK).await;
}

/// Makes sure the host can make disks for VM `vm` from the image: from a
/// shared pool it reaches that keeps a copy (returned), or from its own
/// store (None), copied there from another host.
/// Waiting (for the image to download, or for another task getting it onto
/// the host) holds no task slot, and ends should the VM be deleted
/// meanwhile; copying holds a transfer slot (see [`TaskCtx::transfer`]).
pub(super) async fn ensure_on(
    ctx: &TaskCtx,
    id: ImageId,
    host: HostId,
    vm: VmId,
) -> Result<Option<mst_proto::Pool>> {
    let db = &ctx.state.db;
    let image = mst_db::images::get(db, id).await?;
    let image = match image.state.as_str() {
        "ready" => image,
        _ => {
            ctx.step("waiting for the image to download", 0.15).await;
            ctx.idle(unless_deleted(&ctx.state, vm, ready(&ctx.state, id)))
                .await?
        }
    };
    let take = async { Ok(mst_db::images::CopyLock::take(db, id, host).await?) };
    let lock = ctx.idle(unless_deleted(&ctx.state, vm, take)).await?;
    let result = match available(ctx, &image, host).await {
        Ok(Some(source)) => Ok(source),
        Ok(None) => ctx.transfer(copy_to(ctx, &image, host)).await,
        Err(e) => Err(e),
    };
    lock.release().await;
    result
}

/// `wait`, unless VM `vm` is to be deleted first (its create then gives
/// way to the delete queued behind it).
async fn unless_deleted<T>(
    state: &AppState,
    vm: VmId,
    wait: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    let deleting = async {
        loop {
            match mst_db::vms::get(&state.db, vm).await {
                Ok(v) if v.status == "deleting" => return,
                Err(mst_db::Error::Domain(mst_core::Error::NotFound { .. })) => return,
                _ => tokio::time::sleep(POLL).await,
            }
        }
    };
    tokio::select! {
        r = wait => r,
        () = deleting => bail!("the VM is being deleted"),
    }
}

/// The image, once it is added (its file came, and the host stored it).
async fn ready(state: &AppState, id: ImageId) -> Result<Image> {
    let deadline = tokio::time::Instant::now() + STORE;
    loop {
        let image = mst_db::images::get(&state.db, id).await?;
        match image.state.as_str() {
            "ready" => return Ok(image),
            "failed" => bail!(
                "the image could not be added: {}",
                image.error.as_deref().unwrap_or("no reason given")
            ),
            // Its upload stopped: it may not go on.
            "uploading" => bail!("the image is not all uploaded"),
            _ if tokio::time::Instant::now() > deadline => {
                bail!("the image is still being added")
            }
            _ => tokio::time::sleep(POLL).await,
        }
    }
}

/// Where the host makes disks from the image without a copy being made:
/// its own store (`Some(None)`), or a shared pool it reaches (`Some(pool)`).
async fn available(
    ctx: &TaskCtx,
    image: &Image,
    host: HostId,
) -> Result<Option<Option<mst_proto::Pool>>> {
    let db = &ctx.state.db;
    let copies = mst_db::images::copies(db, image.id).await?;
    if copies.contains(&Copy::local(host)) {
        return Ok(Some(None));
    }
    let target = mst_db::hosts::get(db, host).await?;
    for c in copies.iter().filter(|c| c.pool_id != LOCAL_POOL) {
        let pool = mst_db::storage_pools::get(db, c.pool_id).await?;
        if reaches(&target, &pool) {
            return Ok(Some(Some(crate::api::agent_pool(&pool)?)));
        }
    }
    Ok(None)
}

/// Copies the images whose only copy is on `host` (being evacuated) to
/// other hosts, one each: nothing downloads an image again, so they would
/// go with it. Returns what could not be copied.
pub(super) async fn keep_elsewhere(ctx: &TaskCtx, host: HostId) -> Result<Vec<String>> {
    let state = &ctx.state;
    let mut failures = vec![];
    let images = mst_db::images::only_on_host(&state.db, host).await?;
    let others: Vec<Host> = store_hosts(state)
        .await?
        .into_iter()
        .filter(|h| h.id != host && h.state == "ready")
        .collect();
    for (i, image) in images.iter().enumerate() {
        if others.is_empty() {
            failures.push(format!("{}: no other host to keep it", image.name));
            continue;
        }
        let to = &others[i % others.len()];
        ctx.step(
            &format!("copying image {} to {}", image.name, to.name),
            0.95,
        )
        .await;
        if let Err(e) = copy_to(ctx, image, to.id).await {
            failures.push(format!("{} to {}: {e:#}", image.name, to.name));
        }
    }
    Ok(failures)
}

/// Copies the image into the host's store: from another host's, or a
/// shared pool's (through a host reaching it).
async fn copy_to(ctx: &TaskCtx, image: &Image, host: HostId) -> Result<Option<mst_proto::Pool>> {
    let state = &ctx.state;
    let db = &state.db;
    let copies = mst_db::images::copies(db, image.id).await?;
    let target = mst_db::hosts::get(db, host).await?;
    let mut shared = vec![];
    for c in copies.iter().filter(|c| c.pool_id != LOCAL_POOL) {
        shared.push(mst_db::storage_pools::get(db, c.pool_id).await?);
    }

    let path = mst_db::images::store_path(db, image.tenant_id).await?;
    let hosts = store_hosts(state).await?;
    let mut sources: Vec<&Host> = hosts
        .iter()
        .filter(|h| h.id != host && copies.contains(&Copy::local(h.id)))
        .collect();
    // Spread the copying over the hosts that have it.
    sources.sort_by_key(|_| rand::random::<u32>());
    // Then hosts that reach a shared pool keeping it, which copy it into
    // their own store first.
    let mut relays: Vec<(&Host, &Pool)> = shared
        .iter()
        .flat_map(|p| {
            hosts
                .iter()
                .filter(move |h| h.id != host && reaches(h, p))
                .map(move |h| (h, p))
        })
        .filter(|(h, _)| !sources.iter().any(|s| s.id == h.id))
        .collect();
    relays.sort_by_key(|_| rand::random::<u32>());
    let mut failures = vec![];
    let relayed = relays.into_iter().map(|(h, p)| (h, Some(p)));
    for (src, via) in sources.into_iter().map(|h| (h, None)).chain(relayed) {
        // Each stage fails reading from where (the pool, or `src`'s store).
        let copied = async {
            if let Some(pool) = via {
                ctx.step(
                    &format!("copying the image from {} on {}", pool.name, src.name),
                    0.2,
                )
                .await;
                let from = Copy::shared(pool.id);
                relay(state, image, &path, src, pool)
                    .await
                    .map_err(|e| (from, e))?;
                record(state, image, &path, src.id)
                    .await
                    .map_err(|e| (from, e))?;
            }
            ctx.step(&format!("copying the image from {}", src.name), 0.25)
                .await;
            copy(state, image, &path, src, &target)
                .await
                .map_err(|e| (Copy::local(src.id), e))
        }
        .await;
        match copied {
            Ok(()) => {
                record(state, image, &path, host).await?;
                return Ok(None);
            }
            Err((from, e)) => {
                let e = format!("{e:#}");
                if e.contains(NOT_IN_STORE) {
                    // Gone from where it was recorded: forgotten there.
                    mst_db::images::remove_copy(db, image.id, from).await?;
                }
                tracing::warn!(image = %image.id, from = %src.name, to = %target.name, error = %e, "copying an image");
                failures.push(format!("from {}: {e}", src.name));
            }
        }
    }
    // No copy could be handed over. The image is not downloaded again:
    // once a host or pool holding it is back, it can be.
    bail!(
        "no copy of the image could be had{}: once a host or pool holding one is back, try again",
        match failures.is_empty() {
            true => String::new(),
            false => format!(" ({})", failures.join("; ")),
        }
    );
}

/// Has `host` copy the image from a shared pool's store into its own.
async fn relay(
    state: &AppState,
    image: &Image,
    tenant_path: &str,
    host: &Host,
    pool: &Pool,
) -> Result<()> {
    send(
        state,
        host.id,
        Cmd::CopyImage(CopyImage {
            image_id: image.id.to_string(),
            tenant_path: tenant_path.to_owned(),
            source_pool: Some(crate::api::agent_pool(pool)?),
            digest: image.digest.clone().unwrap_or_default(),
            ..Default::default()
        }),
        COPY,
    )
    .await?;
    Ok(())
}

/// Copies the image from `src`'s store to `dst`'s: `src` serves it to
/// `dst` alone (credentials for this copy only), which checks it holds
/// the image's digest.
async fn copy(
    state: &AppState,
    image: &Image,
    tenant_path: &str,
    src: &Host,
    dst: &Host,
) -> Result<()> {
    let export_id = format!("img-{}-{}", image.id, dst.id);
    let tls = mst_pki::MigrationCredentials::generate(
        &[src.address.clone(), src.name.clone()],
        &dst.id.to_string(),
        COPY,
    )?;
    let served = send(
        state,
        src.id,
        Cmd::ExportImage(ExportImage {
            image_id: image.id.to_string(),
            tenant_path: tenant_path.to_owned(),
            tls: Some(MigrationTls {
                ca_pem: tls.ca_pem.clone(),
                cert_pem: tls.server_cert_pem.clone(),
                key_pem: tls.server_key_pem.clone(),
            }),
            address: src.address.clone(),
            export_id: export_id.clone(),
        }),
        QUICK,
    )
    .await
    .context("serving the image")?;
    let copied = async {
        let port = serde_json::from_str::<serde_json::Value>(&served)?["port"]
            .as_u64()
            .context("no port the image is served on")?;
        send(
            state,
            dst.id,
            Cmd::CopyImage(CopyImage {
                image_id: image.id.to_string(),
                tenant_path: tenant_path.to_owned(),
                nbd_address: src.address.clone(),
                nbd_port: u32::try_from(port)?,
                tls: Some(MigrationTls {
                    ca_pem: tls.ca_pem.clone(),
                    cert_pem: tls.client_cert_pem.clone(),
                    key_pem: tls.client_key_pem.clone(),
                }),
                export_id: export_id.clone(),
                source_pool: None,
                digest: image.digest.clone().unwrap_or_default(),
            }),
            COPY,
        )
        .await
        .map(|_| ())
        .map_err(|e| anyhow!(e))
    }
    .await;
    let end = Cmd::EndExport(EndExport {
        volume_id: export_id,
    });
    let _ = send(state, src.id, end, QUICK).await;
    copied
}

/// Whether an agent's error says the image was not in a store after all.
pub(super) fn missing_from_store(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains(NOT_IN_STORE)
}

/// After an image was deleted: every host drops its copy, and a host that
/// reaches each shared pool that held one drops that. What fails here goes
/// with the hourly prune ([`prune`]).
pub async fn deleted(
    state: AppState,
    image: Image,
    tenant_path: String,
    shared: Vec<StoragePoolId>,
) {
    let mut done = vec![];
    let mut failed = vec![];
    let hosts = store_hosts(&state).await.unwrap_or_default();
    for h in &hosts {
        let cmd = Cmd::DeleteImage(DeleteImage {
            image_id: image.id.to_string(),
            pools: vec![],
            tenant_path: tenant_path.clone(),
        });
        match state.agents.call(h.id, cmd, QUICK).await {
            Ok(()) => done.push(h.name.clone()),
            Err(e) => failed.push(format!("{}: {e}", h.name)),
        }
    }
    for pool_id in shared {
        let Ok(pool) = mst_db::storage_pools::get(&state.db, pool_id).await else {
            continue;
        };
        let Ok(spec) = crate::api::agent_pool(&pool) else {
            continue;
        };
        let mut removed = false;
        for h in hosts.iter().filter(|h| reaches(h, &pool)) {
            let cmd = Cmd::DeleteImage(DeleteImage {
                image_id: image.id.to_string(),
                pools: vec![spec.clone()],
                tenant_path: tenant_path.clone(),
            });
            match state.agents.call(h.id, cmd, QUICK).await {
                Ok(()) => {
                    done.push(format!("{} (from {})", pool.name, h.name));
                    removed = true;
                    break;
                }
                Err(e) => failed.push(format!("{} from {}: {e}", pool.name, h.name)),
            }
        }
        if !removed {
            failed.push(format!("{}: no host removed it; pruned later", pool.name));
        }
    }
    crate::cleanup::record(
        &state,
        "image.cleanup",
        ("image", image.id.0),
        serde_json::json!({"name": image.name, "done": done, "failed": failed}),
    )
    .await;
}

/// On the leader: images whose adding stopped fail (every few minutes), and
/// every hour every host's store, and every shared pool's (through a host
/// that reaches it), let go of images that no longer exist, and of what
/// stores and copies cut off left.
pub async fn prune(state: AppState) {
    let mut last = tokio::time::Instant::now();
    loop {
        tokio::time::sleep(STALLED_EVERY).await;
        if let Err(e) = fail_stalled(&state).await {
            tracing::warn!(error = %format!("{e:#}"), "looking for images whose adding stopped");
        }
        if last.elapsed() >= PRUNE_EVERY {
            last = tokio::time::Instant::now();
            if let Err(e) = prune_once(&state).await {
                tracing::warn!(error = %format!("{e:#}"), "pruning image stores");
            }
        }
    }
}

/// How often images whose adding stopped are looked for.
const STALLED_EVERY: Duration = Duration::from_secs(120);
/// A download no part of which came for this long fails: the controller
/// downloading it went away (nothing else will go on with it).
const DOWNLOAD_STALLED: Duration = Duration::from_secs(10 * 60);
/// An upload no part reached for this long fails.
const UPLOAD_STALLED: Duration = Duration::from_secs(24 * 3600);

/// Fails images whose file stopped coming, or whose store stopped without
/// saying how: nothing goes on with them. What came goes.
async fn fail_stalled(state: &AppState) -> Result<()> {
    let now = chrono::Utc::now();
    let stalled =
        mst_db::images::stalled(&state.db, now - DOWNLOAD_STALLED, now - UPLOAD_STALLED).await?;
    for image in stalled {
        let why = match image.uploaded() {
            true => "the upload stopped: no part of it arrived for a day",
            false => "the download stopped (cut off as a controller went away)",
        };
        if mst_db::images::failed(&state.db, image.id, why).await? {
            tracing::info!(image = %image.id, why, "adding an image stopped");
            remove_leftover(state, image.id).await;
        }
    }
    for id in mst_db::images::stopped_stores(&state.db).await? {
        let why = "storing it stopped without saying why";
        if mst_db::images::failed(&state.db, id, why).await? {
            tracing::info!(image = %id, "storing an image stopped");
            remove_leftover(state, id).await;
        }
    }
    Ok(())
}

async fn prune_once(state: &AppState) -> Result<()> {
    let listed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let keep: Vec<String> = mst_db::images::all_ids(&state.db)
        .await?
        .into_iter()
        .map(|i| i.to_string())
        .collect();
    let hosts = store_hosts(state).await?;
    let prune = |host: HostId, pool: Option<mst_proto::Pool>| {
        let cmd = Cmd::PruneStore(PruneStore {
            pool,
            image_ids: keep.clone(),
            listed_at,
        });
        async move { send(state, host, cmd, Duration::from_secs(600)).await }
    };
    for h in &hosts {
        if let Err(e) = prune(h.id, None).await {
            tracing::info!(host = %h.name, error = %format!("{e:#}"), "pruning a host's image store");
        }
    }
    for pool in mst_db::storage_pools::list(&state.db).await? {
        if pool.kind == "local" {
            continue;
        }
        let spec = crate::api::agent_pool(&pool)?;
        if let Some(h) = hosts.iter().find(|h| reaches(h, &pool))
            && let Err(e) = prune(h.id, Some(spec)).await
        {
            tracing::info!(pool = %pool.name, error = %format!("{e:#}"), "pruning a pool's image store");
        }
    }
    Ok(())
}
