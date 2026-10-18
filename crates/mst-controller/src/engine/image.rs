//! Images: downloaded once, when they are added (`image.download`), into
//! the image store of the pool they are kept in: a host's local disk (a
//! host picked at random), or a shared pool. Hosts attached to a shared
//! pool make disks from its copy directly; others get a copy in their local
//! store, from a host that has one (a host attached to the shared pool
//! first copies it into its own). Every copy is recorded (`image_copies`),
//! and each serves as a source for the next.
//!
//! The download records the image's digest: the SHA-256 of the disk it
//! holds. Every copy is checked against it where it is made, so a host
//! handing over something else (a compromised one) spreads nothing. With
//! no copy reachable, a host downloads the image again, which must then
//! hold the same.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use mst_core::ids::{HostId, ImageId, LOCAL_POOL, StoragePoolId};
use mst_db::hosts::Host;
use mst_db::images::{Copy, Image, Stored};
use mst_db::storage_pools::Pool;
use mst_proto::command::Cmd;
use mst_proto::{
    CopyImage, DeleteImage, EndExport, ExportImage, FetchImage, MigrationTls, PruneStore,
};

use super::TaskCtx;
use super::backup::send;
use crate::state::AppState;

/// Downloads can be large, and slow (the agent gives up after two hours).
const DOWNLOAD: Duration = Duration::from_secs(3 * 3600);
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

/// Hosts that can take an image: connected, not fenced or decommissioned,
/// with an image store.
async fn store_hosts(state: &AppState) -> Result<Vec<Host>> {
    let mut out = vec![];
    for h in mst_db::hosts::list(&state.db).await? {
        if h.is_live()
            && !matches!(h.state.as_str(), "fenced" | "decommissioned")
            && mst_db::hosts::agent_supports(&state.db, h.id, "fetch_image").await?
        {
            out.push(h);
        }
    }
    Ok(out)
}

/// Whether the host reaches a shared pool: one attached to its cluster.
fn reaches(h: &Host, pool: &Pool) -> bool {
    h.cluster_id.is_some_and(|c| pool.clusters.contains(&c))
}

/// Downloads an image into the pool it is kept in.
pub async fn download(ctx: &TaskCtx) -> Result<()> {
    let db = &ctx.state.db;
    let id = target(ctx)?;
    let image = match mst_db::images::get(db, id).await {
        Ok(i) => i,
        // Deleted meanwhile.
        Err(mst_db::Error::Domain(mst_core::Error::NotFound { .. })) => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    if image.state != "downloading" {
        // Done already (a resumed task).
        return Ok(());
    }
    let result = first_download(ctx, &image).await;
    if let Err(e) = &result
        && let Err(f) = mst_db::images::failed(db, id, &format!("{e:#}")).await
    {
        // The API lets it be downloaded again: no task is under way.
        tracing::warn!(image = %id, error = %f, "recording a failed download");
    }
    result
}

async fn first_download(ctx: &TaskCtx, image: &Image) -> Result<()> {
    let state = &ctx.state;
    let db = &state.db;
    let pool = mst_db::storage_pools::get(db, image.storage_pool_id).await?;
    let shared = pool.kind != "local";
    let hosts: Vec<Host> = store_hosts(state)
        .await?
        .into_iter()
        .filter(|h| !shared || reaches(h, &pool))
        .collect();
    if hosts.is_empty() {
        match shared {
            true => bail!("no connected host reaches {}", pool.name),
            false => bail!("no connected host to download it on"),
        }
    }
    // Resumed (a controller restarted): on the host it was downloading on,
    // which has it, or will; another would download it all over again.
    let host = match hosts.iter().find(|h| Some(h.id) == image.download_host_id) {
        Some(h) => h,
        None => {
            let h = &hosts[rand::random_range(0..hosts.len())];
            mst_db::images::set_download_host(db, image.id, h.id).await?;
            h
        }
    };
    ctx.step(&format!("downloading on {}", host.name), 0.1)
        .await;
    let path = mst_db::images::store_path(db, image.tenant_id).await?;
    let (at, into) = match shared {
        true => (Copy::shared(pool.id), Some(crate::api::agent_pool(&pool)?)),
        false => (Copy::local(host.id), None),
    };
    // Downloaded again (no copy was left): it must hold what it did.
    let digest = image.digest.as_deref();
    let stored = fetch(state, image, &path, host.id, into.clone(), digest).await?;
    if stored.digest.is_empty() {
        bail!("{} reported no digest for the image", host.name);
    }

    // Charged what it takes, stored (`image_gib`).
    let gib = (stored.size_bytes + (1 << 30) - 1) >> 30;
    let delta = [("image_gib".to_owned(), gib)].into_iter().collect();
    let mut tx = db.begin().await?;
    let recorded = match mst_db::quotas::reserve(&mut tx, image.tenant_id, &delta).await {
        Ok(()) => mst_db::images::downloaded(&mut tx, image.id, at, &stored).await,
        Err(e) => Err(e),
    };
    match recorded {
        Ok(()) => {
            tx.commit().await?;
            Ok(())
        }
        Err(e) => {
            // Not kept: the host lets go of it (with no lock held).
            tx.rollback().await?;
            remove(state, image, &path, host.id, into).await;
            match e {
                mst_db::Error::Domain(mst_core::Error::NotFound { .. }) => {
                    bail!("the image was deleted meanwhile")
                }
                e => Err(e.into()),
            }
        }
    }
}

/// Has the host download the image into its store, or `pool`'s. With
/// `digest`, the image must hold that.
async fn fetch(
    state: &AppState,
    image: &Image,
    tenant_path: &str,
    host: HostId,
    pool: Option<mst_proto::Pool>,
    digest: Option<&str>,
) -> Result<Stored> {
    let cmd = Cmd::FetchImage(FetchImage {
        pool,
        image_id: image.id.to_string(),
        tenant_path: tenant_path.to_owned(),
        source_url: image.source_url.clone(),
        format: image.format.clone(),
        trusted: super::image_trusted(&state.db, image).await?,
        digest: digest.unwrap_or_default().to_owned(),
    });
    let out = send(state, host, cmd, DOWNLOAD).await?;
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

/// Waits for the image to be downloaded, if it is being (without holding
/// a task slot), and makes sure the host can make disks from it: from a
/// shared pool it reaches that keeps a copy (returned), or from its own
/// store (None), copied there from another host or downloaded again.
pub(super) async fn ensure_on(
    ctx: &TaskCtx,
    id: ImageId,
    host: HostId,
) -> Result<Option<mst_proto::Pool>> {
    let db = &ctx.state.db;
    let image = mst_db::images::get(db, id).await?;
    let image = match image.state.as_str() {
        "ready" => image,
        _ => {
            ctx.step("waiting for the image to download", 0.15).await;
            ctx.idle(ready(&ctx.state, id)).await?
        }
    };
    let lock = mst_db::images::CopyLock::take(db, id, host).await?;
    let result = copy_to(ctx, &image, host).await;
    lock.release().await;
    result
}

/// The image once downloaded.
async fn ready(state: &AppState, id: ImageId) -> Result<Image> {
    let deadline = tokio::time::Instant::now() + DOWNLOAD;
    loop {
        let image = mst_db::images::get(&state.db, id).await?;
        match image.state.as_str() {
            "ready" => return Ok(image),
            "failed" => bail!(
                "the image could not be downloaded: {}",
                image.error.as_deref().unwrap_or("no reason given")
            ),
            _ if tokio::time::Instant::now() > deadline => {
                bail!("the image is still being downloaded")
            }
            _ => tokio::time::sleep(POLL).await,
        }
    }
}

async fn copy_to(ctx: &TaskCtx, image: &Image, host: HostId) -> Result<Option<mst_proto::Pool>> {
    let state = &ctx.state;
    let db = &state.db;
    let digest = image.digest.as_deref();
    let copies = mst_db::images::copies(db, image.id).await?;
    if copies.contains(&Copy::local(host)) {
        return Ok(None);
    }
    let target = mst_db::hosts::get(db, host).await?;
    let mut shared = vec![];
    for c in copies.iter().filter(|c| c.pool_id != LOCAL_POOL) {
        let pool = mst_db::storage_pools::get(db, c.pool_id).await?;
        if reaches(&target, &pool) {
            return Ok(Some(crate::api::agent_pool(&pool)?));
        }
        shared.push(pool);
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
        let copied = async {
            if let Some(pool) = via {
                ctx.step(
                    &format!("copying the image from {} on {}", pool.name, src.name),
                    0.2,
                )
                .await;
                relay(state, image, &path, src, pool).await?;
                record(state, image, &path, src.id).await?;
            }
            ctx.step(&format!("copying the image from {}", src.name), 0.25)
                .await;
            copy(state, image, &path, src, &target).await
        }
        .await;
        match copied {
            Ok(()) => {
                record(state, image, &path, host).await?;
                return Ok(None);
            }
            Err(e) => {
                let e = format!("{e:#}");
                if e.contains(NOT_IN_STORE) {
                    // Gone from where it was recorded: forgotten there.
                    let at = match via {
                        Some(pool) => Copy::shared(pool.id),
                        None => Copy::local(src.id),
                    };
                    mst_db::images::remove_copy(db, image.id, at).await?;
                }
                tracing::warn!(image = %image.id, from = %src.name, to = %target.name, error = %e, "copying an image");
                failures.push(format!("from {}: {e}", src.name));
            }
        }
    }
    // No copy could be handed over: downloaded again, it must hold what
    // was first downloaded.
    ctx.step("downloading the image", 0.2).await;
    if let Err(e) = fetch(state, image, &path, host, None, digest).await {
        return Err(match failures.is_empty() {
            true => e.context("downloading the image"),
            false => e.context(format!(
                "downloading the image, as copying it failed ({})",
                failures.join("; ")
            )),
        });
    }
    record(state, image, &path, host).await?;
    Ok(None)
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

/// Every hour (on the leader): every host's store, and every shared pool's
/// (through a host that reaches it), let go of images that no longer exist,
/// and of what downloads and copies cut off left.
pub async fn prune(state: AppState) {
    loop {
        tokio::time::sleep(PRUNE_EVERY).await;
        if let Err(e) = prune_once(&state).await {
            tracing::warn!(error = %format!("{e:#}"), "pruning image stores");
        }
    }
}

async fn prune_once(state: &AppState) -> Result<()> {
    let keep: Vec<String> = mst_db::images::all_ids(&state.db)
        .await?
        .into_iter()
        .map(|i| i.to_string())
        .collect();
    let mut hosts = vec![];
    for h in store_hosts(state).await? {
        if mst_db::hosts::agent_supports(&state.db, h.id, "prune_store").await? {
            hosts.push(h);
        }
    }
    let prune = |host: HostId, pool: Option<mst_proto::Pool>| {
        let cmd = Cmd::PruneStore(PruneStore {
            pool,
            image_ids: keep.clone(),
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
