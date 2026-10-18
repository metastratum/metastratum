//! Storage pools: where VM disks live.

use std::collections::HashSet;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use mst_core::ids::{ClusterId, LOCAL_POOL, StoragePoolId, TenantId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::authz::{Perm, Principal, audit, authorize, authorize_resource};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_pools, create_pool))
        .routes(routes!(update_pool, delete_pool))
        .routes(routes!(usable_pools))
}

/// Config keys that are secrets: accepted, stored, never returned.
const SECRET_KEYS: &[&str] = &["key"];

#[derive(Serialize, ToSchema)]
pub struct PoolView {
    id: StoragePoolId,
    name: String,
    /// `local`, `nfs` or `rbd`.
    kind: String,
    /// Connection details, without secrets.
    config: Value,
    /// Clusters the pool is attached to; empty for `local`, which every host
    /// has.
    cluster_ids: Vec<ClusterId>,
    volumes: i64,
    created_at: DateTime<Utc>,
}

fn redact(mut config: Value) -> Value {
    if let Some(o) = config.as_object_mut() {
        for k in SECRET_KEYS {
            if o.contains_key(*k) {
                o.insert((*k).into(), json!("(set)"));
            }
        }
    }
    config
}

async fn view(state: &AppState, p: mst_db::storage_pools::Pool) -> ApiResult<PoolView> {
    Ok(PoolView {
        volumes: mst_db::storage_pools::volume_count(&state.db, p.id).await?,
        id: p.id,
        name: p.name,
        kind: p.kind,
        config: redact(p.config),
        cluster_ids: p.clusters,
        created_at: p.created_at,
    })
}

#[utoipa::path(get, path = "/storage-pools", tag = "hosts", responses((status = 200, body = Vec<PoolView>), (status = 403)))]
async fn list_pools(State(state): State<AppState>, p: Principal) -> ApiResult<Json<Vec<PoolView>>> {
    authorize(&state.db, &p, Perm::InfraRead, p.tenant_id).await?;
    let mut out = vec![];
    for pool in mst_db::storage_pools::list(&state.db).await? {
        out.push(view(&state, pool).await?);
    }
    Ok(Json(out))
}

#[derive(Deserialize, ToSchema)]
pub struct PoolSpec {
    name: String,
    /// `nfs` or `rbd` (the local pool is built in).
    kind: Option<String>,
    /// NFS: `server`, `export`, optional mount `options`. RBD: `monitors`
    /// (list of host:port), `pool`, `user`, `key`. On update, omitting `key`
    /// keeps the stored one.
    config: Value,
    cluster_ids: Vec<ClusterId>,
}

fn validate_config(kind: &str, config: &Value) -> ApiResult<()> {
    let str_field = |k: &str| {
        config
            .get(k)
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
    };
    match kind {
        "nfs" => {
            if !str_field("server") || !str_field("export") {
                return Err(ApiError::bad_request(
                    "NFS pools need a server and an export",
                ));
            }
            if !config["export"].as_str().unwrap_or("").starts_with('/') {
                return Err(ApiError::bad_request("the export must be an absolute path"));
            }
        }
        "rbd" => {
            let monitors = config
                .get("monitors")
                .and_then(Value::as_array)
                .is_some_and(|m| !m.is_empty() && m.iter().all(Value::is_string));
            if !monitors || !str_field("pool") || !str_field("user") || !str_field("key") {
                return Err(ApiError::bad_request(
                    "RBD pools need monitors, a pool, a user and a key",
                ));
            }
        }
        _ => return Err(ApiError::bad_request("kind must be nfs or rbd")),
    }
    Ok(())
}

/// The storage a pool's data is on: pools with the same are one storage
/// under two names (one's volumes are the other's).
fn storage_of(kind: &str, config: &Value) -> Option<String> {
    let field = |k: &str| config.get(k).and_then(Value::as_str).map(str::trim);
    match kind {
        "nfs" => Some(format!(
            "nfs:{}:{}",
            field("server")?.to_lowercase(),
            field("export")?.trim_end_matches('/')
        )),
        "rbd" => {
            let mut monitors: Vec<String> = config
                .get("monitors")?
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .map(|m| {
                    let m = m.trim().to_lowercase();
                    if m.contains(':') {
                        m
                    } else {
                        format!("{m}:6789")
                    }
                })
                .collect();
            monitors.sort();
            Some(format!("rbd:{}:{}", monitors.join(","), field("pool")?))
        }
        _ => None,
    }
}

/// Whether two pools are one storage under two names.
pub fn same_storage(a: &mst_db::storage_pools::Pool, b: &mst_db::storage_pools::Pool) -> bool {
    a.id != b.id
        && a.kind == b.kind
        && storage_of(&a.kind, &a.config).is_some_and(|s| Some(s) == storage_of(&b.kind, &b.config))
}

/// Refuses a pool on the storage of another: a disk moved from one to the
/// other would be its own copy, and removed as the old one.
async fn check_unique(
    state: &AppState,
    id: Option<StoragePoolId>,
    kind: &str,
    config: &Value,
) -> ApiResult<()> {
    let Some(storage) = storage_of(kind, config) else {
        return Ok(());
    };
    for other in mst_db::storage_pools::list(&state.db).await? {
        if Some(other.id) != id
            && other.kind == kind
            && storage_of(&other.kind, &other.config) == Some(storage.clone())
        {
            return Err(ApiError::conflict(format!(
                "{} is on that storage already",
                other.name
            )));
        }
    }
    Ok(())
}

fn valid_name(name: &str) -> ApiResult<&str> {
    let n = name.trim();
    if n.is_empty() || n.len() > 64 {
        return Err(ApiError::bad_request("name must be 1-64 characters"));
    }
    Ok(n)
}

#[utoipa::path(post, path = "/storage-pools", tag = "hosts", request_body = PoolSpec,
    responses((status = 201, body = PoolView), (status = 400), (status = 403), (status = 409)))]
async fn create_pool(
    State(state): State<AppState>,
    p: Principal,
    Json(req): Json<PoolSpec>,
) -> ApiResult<(StatusCode, Json<PoolView>)> {
    authorize(&state.db, &p, Perm::InfraWrite, p.tenant_id).await?;
    let kind = req.kind.as_deref().unwrap_or("");
    validate_config(kind, &req.config)?;
    check_unique(&state, None, kind, &req.config).await?;
    let name = valid_name(&req.name)?;
    let mut config = req.config.clone();
    seal_config(&mut config);
    let mut tx = state.db.begin().await?;
    let id = mst_db::storage_pools::create(&mut tx, name, kind, &config, &req.cluster_ids).await?;
    tx.commit().await?;
    audit(
        &state.db,
        Some(&p),
        Some(p.tenant_id),
        "storage_pool.create",
        Some(("storage_pool", id.0)),
        json!({"name": name, "kind": kind, "config": redact(req.config)}),
    )
    .await;
    let pool = mst_db::storage_pools::get(&state.db, id).await?;
    Ok((StatusCode::CREATED, Json(view(&state, pool).await?)))
}

#[utoipa::path(put, path = "/storage-pools/{id}", tag = "hosts", params(("id" = StoragePoolId, Path)), request_body = PoolSpec,
    responses((status = 200, body = PoolView), (status = 400), (status = 403), (status = 404)))]
async fn update_pool(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<StoragePoolId>,
    Json(mut req): Json<PoolSpec>,
) -> ApiResult<Json<PoolView>> {
    authorize(&state.db, &p, Perm::InfraWrite, p.tenant_id).await?;
    let current = mst_db::storage_pools::get(&state.db, id).await?;
    if current.kind == "local" {
        return Err(ApiError::bad_request("the local pool cannot be changed"));
    }
    // Secrets are never returned, so clients cannot send them back.
    if let (Some(new), Some(old)) = (req.config.as_object_mut(), current.config.as_object()) {
        for k in SECRET_KEYS {
            if !new.contains_key(*k)
                && let Some(v) = old.get(*k)
            {
                new.insert((*k).into(), v.clone());
            }
        }
    }
    validate_config(&current.kind, &req.config)?;
    check_unique(&state, Some(id), &current.kind, &req.config).await?;
    let name = valid_name(&req.name)?;
    let removed: Vec<_> = current
        .clusters
        .iter()
        .filter(|c| !req.cluster_ids.contains(c))
        .collect();
    if !removed.is_empty() && mst_db::storage_pools::volume_count(&state.db, id).await? > 0 {
        return Err(ApiError::conflict(
            "detaching clusters from a pool with volumes is not supported yet",
        ));
    }
    let mut config = req.config.clone();
    seal_config(&mut config);
    let mut tx = state.db.begin().await?;
    mst_db::storage_pools::update(&mut tx, id, name, &config, &req.cluster_ids).await?;
    tx.commit().await?;
    audit(
        &state.db,
        Some(&p),
        Some(p.tenant_id),
        "storage_pool.update",
        Some(("storage_pool", id.0)),
        json!({"name": name, "config": redact(req.config)}),
    )
    .await;
    let pool = mst_db::storage_pools::get(&state.db, id).await?;
    Ok(Json(view(&state, pool).await?))
}

/// Removes a pool that holds no VM disks. By default it is only unlinked:
/// hosts unmount it and forget its credentials, and its storage is left as
/// it is; with `delete_data` metastratum's data on the storage goes too.
#[utoipa::path(delete, path = "/storage-pools/{id}", tag = "hosts", params(("id" = StoragePoolId, Path), DeletePool),
    responses((status = 204), (status = 403), (status = 404), (status = 409, description = "Has volumes or images")))]
async fn delete_pool(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<StoragePoolId>,
    axum::extract::Query(q): axum::extract::Query<DeletePool>,
) -> ApiResult<StatusCode> {
    authorize(&state.db, &p, Perm::InfraWrite, p.tenant_id).await?;
    let pool = mst_db::storage_pools::get(&state.db, id).await?;
    if pool.kind == "local" {
        return Err(ApiError::bad_request("the local pool is built in"));
    }
    let n = mst_db::storage_pools::volume_count(&state.db, id).await?;
    if n > 0 {
        return Err(ApiError::conflict(format!(
            "the pool still holds {n} volumes"
        )));
    }
    let n = mst_db::images::count_in_pool(&state.db, id).await?;
    if n > 0 {
        return Err(ApiError::conflict(format!(
            "the pool keeps {n} images; delete them first"
        )));
    }
    // Data on storage another pool uses stays; that pool needs it.
    let backend = crate::cleanup::backend(&pool);
    let shared = mst_db::storage_pools::list(&state.db)
        .await?
        .iter()
        .any(|o| o.id != id && backend.is_some() && crate::cleanup::backend(o) == backend);
    let mut tx = state.db.begin().await?;
    mst_db::storage_pools::delete(&mut tx, id).await?;
    tx.commit().await?;
    tokio::spawn(crate::cleanup::pool_deleted(
        state.clone(),
        pool.clone(),
        q.delete_data,
        shared,
    ));
    audit(
        &state.db,
        Some(&p),
        Some(p.tenant_id),
        "storage_pool.delete",
        Some(("storage_pool", id.0)),
        json!({"name": pool.name, "delete_data": q.delete_data}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DeletePool {
    /// Also remove metastratum's data from the pool's storage (cached images,
    /// volumes left). Without it the pool is only unlinked: hosts let go of
    /// it, and the storage is left as it is. Data on storage another pool
    /// uses always stays.
    #[serde(default)]
    delete_data: bool,
}

#[derive(Serialize, ToSchema)]
pub struct UsablePool {
    id: StoragePoolId,
    name: String,
    kind: String,
}

/// Pools the tenant may put VM disks in: local storage, and shared pools
/// granted to it.
#[utoipa::path(get, path = "/tenants/{id}/storage-pools", tag = "tenants", params(("id" = TenantId, Path)),
    responses((status = 200, body = Vec<UsablePool>), (status = 404)))]
async fn usable_pools(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<TenantId>,
) -> ApiResult<Json<Vec<UsablePool>>> {
    authorize_resource(&state.db, &p, Perm::VmRead, id, "tenant").await?;
    let mut out = vec![];
    for pool in mst_db::storage_pools::list(&state.db).await? {
        if may_use(&state, id, &pool).await? {
            out.push(UsablePool {
                id: pool.id,
                name: pool.name,
                kind: pool.kind,
            });
        }
    }
    Ok(Json(out))
}

/// Local storage is everyone's; shared pools must be granted.
pub async fn may_use(
    state: &AppState,
    tenant: TenantId,
    pool: &mst_db::storage_pools::Pool,
) -> ApiResult<bool> {
    Ok(pool.id == LOCAL_POOL
        || mst_db::grants::may_use(&state.db, tenant, "storage_pool", pool.id.0).await?)
}

/// Clusters a VM using `pool` may run in, given the tenant's clusters.
pub fn clusters_for(
    pool: &mst_db::storage_pools::Pool,
    allowed: Option<HashSet<ClusterId>>,
) -> Option<HashSet<ClusterId>> {
    if pool.kind == "local" {
        return allowed;
    }
    let attached: HashSet<_> = pool.clusters.iter().copied().collect();
    Some(match allowed {
        Some(a) => a.intersection(&attached).copied().collect(),
        None => attached,
    })
}

/// The pool as the agent needs it, secrets included.
/// A pool as an agent needs it, secrets opened.
pub fn agent_pool(pool: &mst_db::storage_pools::Pool) -> anyhow::Result<mst_proto::Pool> {
    let mut config = pool.config.clone();
    if let Some(key) = config.get("key").and_then(|k| k.as_str()) {
        config["key"] = crate::secrets::open(crate::secrets::POOL_KEY, key)?.into();
    }
    Ok(mst_proto::Pool {
        id: pool.id.to_string(),
        kind: pool.kind.clone(),
        config_json: config.to_string(),
    })
}

/// Seals a config's secrets that came in as plaintext.
fn seal_config(config: &mut serde_json::Value) {
    if let Some(key) = config.get("key").and_then(|k| k.as_str())
        && !mst_pki::is_sealed(key)
    {
        config["key"] = crate::secrets::seal(crate::secrets::POOL_KEY, key).into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configs_are_validated_and_secrets_redacted() {
        assert!(validate_config("nfs", &json!({"server": "s", "export": "/e"})).is_ok());
        assert!(validate_config("nfs", &json!({"server": "s", "export": "e"})).is_err());
        assert!(
            validate_config(
                "rbd",
                &json!({"monitors": ["m:6789"], "pool": "p", "user": "u", "key": "k"})
            )
            .is_ok()
        );
        assert!(
            validate_config(
                "rbd",
                &json!({"monitors": [], "pool": "p", "user": "u", "key": "k"})
            )
            .is_err()
        );
        assert!(validate_config("local", &json!({})).is_err());
        assert_eq!(
            redact(json!({"user": "u", "key": "secret"})),
            json!({"user": "u", "key": "(set)"})
        );
    }
}

#[cfg(test)]
mod same_storage_tests {
    use super::storage_of;
    use serde_json::json;

    #[test]
    fn the_same_storage_under_two_names_is_found() {
        let nfs = |server: &str, export: &str| {
            storage_of("nfs", &json!({"server": server, "export": export}))
        };
        assert_eq!(nfs("10.0.0.1", "/srv/a"), nfs(" 10.0.0.1", "/srv/a/"));
        assert_ne!(nfs("10.0.0.1", "/srv/a"), nfs("10.0.0.1", "/srv/a/b"));
        let rbd = |mons: serde_json::Value, pool: &str| {
            storage_of(
                "rbd",
                &json!({"monitors": mons, "pool": pool, "user": "u", "key": "k"}),
            )
        };
        assert_eq!(
            rbd(json!(["m1", "m2:6789"]), "p"),
            rbd(json!(["m2", "m1:6789"]), "p"),
            "monitors in any order, the default port or not"
        );
        assert_ne!(rbd(json!(["m1"]), "p"), rbd(json!(["m1"]), "q"));
        assert_eq!(storage_of("local", &json!({})), None);
    }
}
