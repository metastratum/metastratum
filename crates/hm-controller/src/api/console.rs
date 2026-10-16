//! Virtual console: ticket issuance and the WebSocket relay.
//!
//! The browser first obtains a single-use ticket (authenticated and
//! authorised like any API call), then opens a WebSocket carrying only the
//! ticket. The controller asks the VM's agent to open a reverse tunnel and
//! relays bytes: RFB for noVNC, raw terminal bytes for xterm.js.

use std::time::Duration;

use axum::Json;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use hm_core::ids::VmId;
use hm_proto::{ConsoleKind, TunnelChunk, tunnel_chunk};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::agents::Tunnel;
use crate::authz::{Perm, Principal, authorize_resource};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

const TICKET_TTL: Duration = Duration::from_secs(30);

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_ticket))
        .routes(routes!(console_socket))
}

#[derive(Clone, Copy, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Graphical console (RFB over WebSocket, for noVNC).
    Vnc,
    /// Serial console (raw bytes, for a terminal emulator).
    Serial,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Vnc => "vnc",
            Kind::Serial => "serial",
        }
    }
}

#[derive(Deserialize, ToSchema)]
pub struct ConsoleRequest {
    kind: Kind,
}

#[derive(Serialize, ToSchema)]
pub struct ConsoleTicket {
    /// Single-use; valid for 30 seconds.
    ticket: String,
    /// WebSocket path to connect to with the ticket.
    path: String,
}

#[utoipa::path(post, path = "/vms/{id}/console", tag = "vms", params(("id" = VmId, Path)),
    request_body = ConsoleRequest, responses((status = 200, body = ConsoleTicket), (status = 404), (status = 409)))]
async fn create_ticket(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<VmId>,
    Json(req): Json<ConsoleRequest>,
) -> ApiResult<Json<ConsoleTicket>> {
    let vm = hm_db::vms::get(&state.db, id).await?;
    authorize_resource(&state.db, &p, Perm::VmConsole, vm.tenant_id, "vm").await?;
    if vm.power != "running" {
        return Err(ApiError::conflict("VM is not running"));
    }
    let ticket = hm_pki::random_hex(24);
    hm_db::console::create_ticket(
        &state.db,
        &hm_pki::hash_secret(&ticket),
        p.user_id,
        id,
        req.kind.as_str(),
        TICKET_TTL,
    )
    .await?;
    Ok(Json(ConsoleTicket {
        path: format!("/api/v1/console/{ticket}"),
        ticket,
    }))
}

/// WebSocket endpoint; the ticket in the path is the only credential.
#[utoipa::path(get, path = "/console/{ticket}", tag = "vms", params(("ticket" = String, Path)),
    responses((status = 101, description = "Switching to the console WebSocket"), (status = 401), (status = 502)))]
async fn console_socket(
    State(state): State<AppState>,
    Path(ticket): Path<String>,
    ws: WebSocketUpgrade,
) -> ApiResult<Response> {
    let (user, vm_id, kind) =
        hm_db::console::consume_ticket(&state.db, &hm_pki::hash_secret(&ticket))
            .await?
            .ok_or_else(ApiError::unauthorized)?;
    let vm = hm_db::vms::get(&state.db, vm_id).await?;
    let host = vm
        .host_id
        .ok_or_else(|| ApiError::conflict("VM is not placed on a host"))?;
    let kind = if kind == "vnc" {
        ConsoleKind::Vnc
    } else {
        ConsoleKind::Serial
    };

    // Open the tunnel before upgrading so failures become a proper HTTP error.
    let tunnel = state
        .agents
        .open_console(host, vm_id, kind)
        .await
        .map_err(|e| {
            tracing::warn!(vm = %vm_id, error = %e, "opening console");
            ApiError::new(
                axum::http::StatusCode::BAD_GATEWAY,
                "console_unavailable",
                e.to_string(),
            )
        })?;
    tracing::info!(vm = %vm_id, %user, ?kind, "console session started");
    // noVNC asks for the "binary" subprotocol.
    Ok(ws
        .protocols(["binary"])
        .on_upgrade(move |socket| relay(socket, tunnel))
        .into_response())
}

async fn relay(socket: WebSocket, tunnel: Tunnel) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let Tunnel {
        mut from_agent,
        to_agent,
    } = tunnel;

    let upstream = async {
        while let Some(Ok(msg)) = ws_rx.next().await {
            let data = match msg {
                Message::Binary(b) => b.to_vec(),
                Message::Text(t) => t.as_bytes().to_vec(),
                Message::Close(_) => break,
                Message::Ping(_) | Message::Pong(_) => continue,
            };
            let chunk = TunnelChunk {
                msg: Some(tunnel_chunk::Msg::Data(data)),
            };
            if to_agent.send(Ok(chunk)).await.is_err() {
                break;
            }
        }
    };
    let downstream = async {
        while let Ok(Some(chunk)) = from_agent.message().await {
            if let Some(tunnel_chunk::Msg::Data(data)) = chunk.msg
                && ws_tx.send(Message::Binary(data.into())).await.is_err()
            {
                break;
            }
        }
        let _ = ws_tx.send(Message::Close(None)).await;
    };
    // Either side ending ends the session; dropping the halves closes both.
    tokio::select! {
        () = upstream => {},
        () = downstream => {},
    }
}
