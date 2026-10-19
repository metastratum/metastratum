//! An image's file, coming to the host that stores it (picked as the image
//! is added): in parts, as whoever uploads it sends them, or as the
//! controller downloads it from a URL. A download happens as the image is
//! added, and only then: the URL is kept nowhere, so a download cut off
//! (the controller went away) fails, and the image is added again. Once all
//! of the file is there, the host adds it to the store (`image.store`).

use std::time::Duration;

use anyhow::{Context, Result, bail};
use mst_core::ids::{HostId, ImageId, UserId};
use mst_db::images::Image;
use serde::Deserialize;

use crate::state::AppState;

/// What goes to the host in one command (within what gRPC takes).
pub const PIECE: usize = 2 << 20;

/// Why a part did not get there.
pub enum PartError {
    /// The host has less of the file than the part's offset (it lost
    /// some): it is sent from what it has.
    Behind(u64),
    Failed(anyhow::Error),
}

/// Sends a part of the image's file to `host`, at `offset`, in pieces.
/// `total` is the file's size, or 0 when not known.
pub async fn send_part(
    state: &AppState,
    host: HostId,
    image: ImageId,
    offset: u64,
    data: &[u8],
    total: u64,
) -> Result<(), PartError> {
    #[derive(Deserialize)]
    struct Received {
        received: u64,
    }
    let mut at = offset;
    for piece in data.chunks(PIECE) {
        let cmd = mst_proto::command::Cmd::ReceiveImage(mst_proto::ReceiveImage {
            image_id: image.to_string(),
            offset: at,
            data: piece.to_vec(),
            total,
        });
        let got: Received = state
            .agents
            .query(host, cmd, Duration::from_secs(60))
            .await
            .map_err(|e| PartError::Failed(anyhow::anyhow!("the host taking the image: {e}")))?;
        if got.received < at + piece.len() as u64 {
            return Err(PartError::Behind(got.received));
        }
        at += piece.len() as u64;
    }
    Ok(())
}

/// All of the image's file is there (`bytes` of it): the host stores it.
/// Returns whether it was still being added (not deleted, nor failed).
pub async fn done(
    tx: &mut mst_db::sqlx::PgConnection,
    image: &Image,
    bytes: u64,
    by: Option<UserId>,
) -> Result<bool> {
    if !mst_db::images::received_all(&mut *tx, image.id, bytes as i64).await? {
        return Ok(false);
    }
    mst_db::tasks::enqueue(
        &mut *tx,
        mst_db::tasks::NewTask {
            tenant_id: image.tenant_id,
            kind: "image.store",
            target: Some(("image", image.id.0)),
            params: serde_json::json!({}),
            created_by: by,
        },
    )
    .await?;
    Ok(true)
}

/// Downloads the image's file from `url` and sends it to its host, in the
/// background: as it is added, the one time. What fails fails the image.
pub fn download(state: AppState, image: Image, url: String, by: Option<UserId>) {
    tokio::spawn(async move {
        let id = image.id;
        if let Err(e) = fetch(&state, &image, &url, by).await {
            let why = format!("downloading it: {e:#}");
            match mst_db::images::failed(&state.db, id, &why).await {
                Ok(true) => {
                    tracing::info!(image = %id, error = %why, "an image download failed");
                    crate::engine::remove_leftover(&state, id).await;
                }
                // Deleted meanwhile: its host lets go of what came.
                Ok(false) => {}
                Err(f) => tracing::warn!(image = %id, error = %f, "recording a failed download"),
            }
        }
    });
}

async fn fetch(state: &AppState, image: &Image, url: &str, by: Option<UserId>) -> Result<()> {
    let host = image.download_host_id.context("no host to send it to")?;
    // The provider's images may come from internal addresses; tenants'
    // from public ones only. The host stores no larger an image than it
    // takes (`image_max_gib`).
    let policy = mst_storage::FetchPolicy {
        allow_private: crate::engine::image_trusted(&state.db, image).await?,
        max_bytes: u64::MAX,
        ..mst_storage::FetchPolicy::default()
    };
    let mut download = mst_storage::Download::start(url, policy).await?;
    let total = download.content_length().unwrap_or(0);
    if total > 0 {
        mst_db::images::set_file_bytes(&state.db, image.id, total as i64).await?;
    }
    tracing::info!(image = %image.id, bytes = total, "downloading an image");
    let mut at = 0u64;
    let mut buf: Vec<u8> = Vec::with_capacity(PIECE);
    loop {
        let chunk = download.chunk().await?;
        if let Some(c) = &chunk {
            buf.extend_from_slice(c);
        }
        let last = chunk.is_none();
        while buf.len() >= PIECE || (last && !buf.is_empty()) {
            let n = buf.len().min(PIECE);
            match send_part(state, host, image.id, at, &buf[..n], total).await {
                Ok(()) => {}
                Err(PartError::Behind(had)) => {
                    bail!("the host lost what it had (it has {had} bytes)")
                }
                Err(PartError::Failed(e)) => return Err(e),
            }
            buf.drain(..n);
            at += n as u64;
            if !mst_db::images::received_so_far(&state.db, image.id, at as i64).await? {
                bail!("the image was deleted meanwhile");
            }
        }
        if last {
            break;
        }
    }
    if at == 0 {
        bail!("the server sent nothing");
    }
    if total > 0 && at != total {
        bail!("the server sent {at} of the {total} bytes it said");
    }
    let mut tx = state.db.begin().await?;
    if done(&mut tx, image, at, by).await? {
        tx.commit().await?;
        tracing::info!(image = %image.id, bytes = at, "image downloaded");
    }
    Ok(())
}
