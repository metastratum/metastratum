//! Images: downloaded once, when they are added (`image.download`), into
//! the image store of the pool they are kept in: a host's local disk (a
//! host picked at random), or a shared pool. Hosts attached to a shared
//! pool make disks from its copy directly; others get a copy in their local
//! store, from a host that has one (a host attached to the shared pool
//! first copies it into its own). Every copy is recorded (`image_copies`),
//! and each serves as a source for the next. With no copy reachable, a host
//! downloads the image again, which must then be the file first downloaded.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use mst_core::ids::{HostId, ImageId, LOCAL_POOL};
use mst_db::hosts::Host;
use mst_db::images::{Copy, Image, Stored};
use mst_db::storage_pools::Pool;
use mst_proto::command::Cmd;
use mst_proto::{CopyImage, DeleteImage, EndExport, ExportImage, FetchImage, MigrationTls};

use super::TaskCtx;
use super::backup::send;
use crate::state::AppState;

/// Downloads can be large, and slow.
const DOWNLOAD: Duration = Duration::from_secs(3 * 3600);
/// So can copies.
const COPY: Duration = Duration::from_secs(3 * 3600);
const QUICK: Duration = Duration::from_secs(60);
/// How often a VM waiting for its image looks again.
const POLL: Duration = Duration::from_secs(3);

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

/// Downloads an image onto a host picked at random.
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
    let result = async {
        let pool = mst_db::storage_pools::get(db, image.storage_pool_id).await?;
        let shared = pool.kind != "local";
        let hosts: Vec<Host> = store_hosts(&ctx.state)
            .await?
            .into_iter()
            .filter(|h| !shared || reaches(h, &pool))
            .collect();
        if hosts.is_empty() && shared {
            bail!("no connected host reaches {}", pool.name);
        }
        if hosts.is_empty() {
            bail!("no connected host to download it on");
        }
        let host = &hosts[rand::random_range(0..hosts.len())];
        ctx.step(&format!("downloading on {}", host.name), 0.1)
            .await;
        let path = mst_db::images::store_path(db, image.tenant_id).await?;
        let (at, into) = match shared {
            true => (Copy::shared(pool.id), Some(crate::api::agent_pool(&pool)?)),
            false => (Copy::local(host.id), None),
        };
        let stored = fetch(&ctx.state, &image, &path, host.id, into.clone(), None).await?;
        // Charged what it takes, stored (`image_gib`).
        let gib = (stored.size_bytes + (1 << 30) - 1) >> 30;
        let delta = [("image_gib".to_owned(), gib)].into_iter().collect();
        let mut tx = db.begin().await?;
        if let Err(e) = mst_db::quotas::reserve(&mut tx, image.tenant_id, &delta).await {
            drop(tx);
            remove(&ctx.state, &image, &path, host.id, into).await;
            return Err(e.into());
        }
        record(
            &ctx.state,
            &mut tx,
            &image,
            &path,
            (host.id, at, into),
            &stored,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
    .await;
    if let Err(e) = &result {
        mst_db::images::failed(db, id, &format!("{e:#}")).await?;
    }
    result
}

/// Has the host download the image into its store, or `pool`'s.
async fn fetch(
    state: &AppState,
    image: &Image,
    tenant_path: &str,
    host: HostId,
    pool: Option<mst_proto::Pool>,
    sha256: Option<&str>,
) -> Result<Stored> {
    let cmd = Cmd::FetchImage(FetchImage {
        pool,
        image_id: image.id.to_string(),
        tenant_path: tenant_path.to_owned(),
        source_url: image.source_url.clone(),
        format: image.format.clone(),
        trusted: super::image_trusted(&state.db, image).await?,
        sha256: sha256.unwrap_or_default().to_owned(),
    });
    let out = send(state, host, cmd, DOWNLOAD).await?;
    Ok(serde_json::from_str(&out)?)
}

/// Records a copy, made by `by.0` (in `by.2`'s store, or its own); should
/// the image be gone meanwhile, that host removes it again.
async fn record(
    state: &AppState,
    conn: &mut mst_db::sqlx::PgConnection,
    image: &Image,
    tenant_path: &str,
    by: (HostId, Copy, Option<mst_proto::Pool>),
    stored: &Stored,
) -> Result<()> {
    let (host, at, pool) = by;
    match mst_db::images::add_copy(conn, image.id, at, stored).await {
        Err(mst_db::Error::Domain(mst_core::Error::NotFound { .. })) => {
            remove(state, image, tenant_path, host, pool).await;
            bail!("the image was deleted meanwhile")
        }
        r => Ok(r?),
    }
}

/// Has the host remove a copy, in its store or `pool`'s (best effort:
/// hosts drop the copies of images that do not exist as they reconnect).
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

/// Whether the host reaches a shared pool: one attached to its cluster.
fn reaches(h: &Host, pool: &Pool) -> bool {
    h.cluster_id.is_some_and(|c| pool.clusters.contains(&c))
}

/// Waits for the image to be downloaded, if it is being, and makes sure
/// the host can make disks from it: from a shared pool it reaches that
/// keeps a copy (returned), or from its own store (None), copied there from
/// another host or downloaded again.
pub(super) async fn ensure_on(
    ctx: &TaskCtx,
    id: ImageId,
    host: HostId,
) -> Result<Option<mst_proto::Pool>> {
    let db = &ctx.state.db;
    let deadline = tokio::time::Instant::now() + DOWNLOAD;
    let mut said = false;
    let image = loop {
        let image = mst_db::images::get(db, id).await?;
        match image.state.as_str() {
            "ready" => break image,
            "failed" => bail!(
                "the image could not be downloaded: {}",
                image.error.as_deref().unwrap_or("no reason given")
            ),
            _ if tokio::time::Instant::now() > deadline => {
                bail!("the image is still being downloaded")
            }
            _ => {
                if !said {
                    ctx.step("waiting for the image to download", 0.15).await;
                    said = true;
                }
                tokio::time::sleep(POLL).await;
            }
        }
    };
    let lock = mst_db::images::CopyLock::take(db, id, host).await?;
    let result = copy_to(ctx, &image, host).await;
    lock.release().await;
    result
}

async fn copy_to(ctx: &TaskCtx, image: &Image, host: HostId) -> Result<Option<mst_proto::Pool>> {
    let state = &ctx.state;
    let db = &state.db;
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
                let stored = relay(state, image, &path, src, pool).await?;
                let mut conn = db.acquire().await?;
                let by = (src.id, Copy::local(src.id), None);
                record(state, &mut conn, image, &path, by, &stored).await?;
            }
            ctx.step(&format!("copying the image from {}", src.name), 0.25)
                .await;
            copy(state, image, &path, src, &target).await
        }
        .await;
        match copied {
            Ok(stored) => {
                let mut conn = db.acquire().await?;
                let by = (host, Copy::local(host), None);
                record(state, &mut conn, image, &path, by, &stored).await?;
                return Ok(None);
            }
            Err(e) => {
                let e = format!("{e:#}");
                // A host's copy gone missing; a shared pool's may be out of
                // reach of that host alone.
                if e.contains(NOT_IN_STORE) && via.is_none() {
                    mst_db::images::remove_copy(db, image.id, Copy::local(src.id)).await?;
                }
                tracing::warn!(image = %image.id, from = %src.name, to = %target.name, error = %e, "copying an image");
                failures.push(format!("from {}: {e}", src.name));
            }
        }
    }
    // No copy could be handed over: downloaded again, it must be the file
    // first downloaded (when that was recorded).
    ctx.step("downloading the image", 0.2).await;
    let sha256 = image.sha256.as_deref();
    let stored = match fetch(state, image, &path, host, None, sha256).await {
        Ok(s) => s,
        Err(e) if failures.is_empty() => return Err(e.context("downloading the image")),
        Err(e) => {
            return Err(e.context(format!(
                "downloading the image, as copying it failed ({})",
                failures.join("; ")
            )));
        }
    };
    let mut conn = db.acquire().await?;
    let by = (host, Copy::local(host), None);
    record(state, &mut conn, image, &path, by, &stored).await?;
    Ok(None)
}

/// Has `host` copy the image from a shared pool's store into its own.
async fn relay(
    state: &AppState,
    image: &Image,
    tenant_path: &str,
    host: &Host,
    pool: &Pool,
) -> Result<Stored> {
    let out = send(
        state,
        host.id,
        Cmd::CopyImage(CopyImage {
            image_id: image.id.to_string(),
            tenant_path: tenant_path.to_owned(),
            source_pool: Some(crate::api::agent_pool(pool)?),
            ..Default::default()
        }),
        COPY,
    )
    .await?;
    Ok(serde_json::from_str(&out)?)
}

/// Copies the image from `src`'s store to `dst`'s: `src` serves it to
/// `dst` alone (credentials for this copy only).
async fn copy(
    state: &AppState,
    image: &Image,
    tenant_path: &str,
    src: &Host,
    dst: &Host,
) -> Result<Stored> {
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
        let out = send(
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
            }),
            COPY,
        )
        .await?;
        serde_json::from_str::<Stored>(&out).map_err(|e| anyhow!(e))
    }
    .await;
    let end = Cmd::EndExport(EndExport {
        volume_id: export_id,
    });
    let _ = send(state, src.id, end, QUICK).await;
    copied
}

/// Whether an agent's error says the image was not in its store after all
/// (removed by hand): the copy recorded there is forgotten, and the image
/// got there again.
pub(super) fn missing_from_store(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains(NOT_IN_STORE)
}
