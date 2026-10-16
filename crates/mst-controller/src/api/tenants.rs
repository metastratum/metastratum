//! Tenants, their quotas and the provider resources granted to them.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use mst_core::ids::TenantId;
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use uuid::Uuid;

use crate::authz::{Perm, Principal, audit, authorize, authorize_resource};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_tenants, create_tenant))
        .routes(routes!(get_tenant, update_tenant, delete_tenant))
        .routes(routes!(get_quotas, set_quotas))
        .routes(routes!(get_grants, set_grants))
        .routes(routes!(bind_domain))
        .routes(routes!(unbind_domain))
}

#[derive(Serialize, ToSchema)]
pub struct TenantView {
    id: TenantId,
    parent_id: Option<TenantId>,
    slug: String,
    name: String,
    /// Slugs from below the root down to this tenant, e.g. `acme/dev`; what
    /// users of this tenant enter at sign-in. Empty for the root.
    slug_path: String,
    /// 1 for the root.
    depth: usize,
    /// Domain names bound to the tenant: signing in at one starts here.
    domains: Vec<String>,
    created_at: DateTime<Utc>,
}

async fn views(
    state: &AppState,
    tenants: Vec<mst_db::tenants::Tenant>,
) -> ApiResult<Vec<TenantView>> {
    let ids: Vec<_> = tenants.iter().map(|t| t.id).collect();
    let paths = mst_db::tenants::slug_paths(&state.db, &ids).await?;
    let mut domains = mst_db::tenants::domains(&state.db, &ids).await?;
    Ok(tenants
        .into_iter()
        .map(|t| TenantView {
            slug_path: paths.get(&t.id).cloned().unwrap_or_default(),
            domains: domains.remove(&t.id).unwrap_or_default(),
            depth: t.path.depth(),
            id: t.id,
            parent_id: t.parent_id,
            slug: t.slug,
            name: t.name,
            created_at: t.created_at,
        })
        .collect())
}

async fn load(
    state: &AppState,
    p: &Principal,
    id: TenantId,
    perm: Perm,
) -> ApiResult<mst_db::tenants::Tenant> {
    let t = mst_db::tenants::get(&state.db, id).await?;
    authorize_resource(&state.db, p, perm, t.id, "tenant").await?;
    Ok(t)
}

/// The parent, for permissions governed from above. The root has none.
fn parent_of(t: &mst_db::tenants::Tenant) -> ApiResult<TenantId> {
    t.parent_id
        .ok_or_else(|| ApiError::bad_request("the root tenant has no parent to manage it from"))
}

/// Every tenant the caller can see, parents before children.
#[utoipa::path(get, path = "/tenants", tag = "tenants", responses((status = 200, body = Vec<TenantView>)))]
async fn list_tenants(
    State(state): State<AppState>,
    p: Principal,
) -> ApiResult<Json<Vec<TenantView>>> {
    let roots = p.scopes(Perm::TenantRead);
    let tenants = mst_db::tenants::in_subtrees(&state.db, &roots).await?;
    Ok(Json(views(&state, tenants).await?))
}

#[utoipa::path(get, path = "/tenants/{id}", tag = "tenants", params(("id" = TenantId, Path)),
    responses((status = 200, body = TenantView), (status = 404)))]
async fn get_tenant(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<TenantId>,
) -> ApiResult<Json<TenantView>> {
    let t = load(&state, &p, id, Perm::TenantRead).await?;
    Ok(Json(views(&state, vec![t]).await?.remove(0)))
}

#[derive(Deserialize, ToSchema)]
pub struct CreateTenant {
    parent_id: TenantId,
    /// Lowercase letters, digits and hyphens; unique among siblings.
    slug: String,
    name: String,
}

fn valid_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}

fn valid_name(s: &str) -> ApiResult<&str> {
    let s = s.trim();
    if s.is_empty() || s.len() > 128 {
        return Err(ApiError::bad_request("name must be 1-128 characters"));
    }
    Ok(s)
}

#[utoipa::path(post, path = "/tenants", tag = "tenants", request_body = CreateTenant,
    responses((status = 201, body = TenantView), (status = 400), (status = 403), (status = 409)))]
async fn create_tenant(
    State(state): State<AppState>,
    p: Principal,
    Json(req): Json<CreateTenant>,
) -> ApiResult<(StatusCode, Json<TenantView>)> {
    authorize(&state.db, &p, Perm::TenantWrite, req.parent_id).await?;
    if !valid_slug(&req.slug) {
        return Err(ApiError::bad_request(
            "slug must be 1-63 lowercase letters, digits or hyphens, not starting or ending with a hyphen",
        ));
    }
    let mut tx = state.db.begin().await?;
    let t = mst_db::tenants::create(
        &mut *tx,
        Some(req.parent_id),
        &req.slug,
        valid_name(&req.name)?,
    )
    .await?;
    // Every tenant starts with a default security group for its VMs.
    mst_db::netsec::default_group(&mut tx, t.id).await?;
    tx.commit().await?;
    audit(
        &state.db,
        Some(&p),
        Some(t.id),
        "tenant.create",
        Some(("tenant", t.id.0)),
        json!({"slug": t.slug, "name": t.name}),
    )
    .await;
    Ok((
        StatusCode::CREATED,
        Json(views(&state, vec![t]).await?.remove(0)),
    ))
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateTenant {
    name: String,
}

#[utoipa::path(patch, path = "/tenants/{id}", tag = "tenants", params(("id" = TenantId, Path)), request_body = UpdateTenant,
    responses((status = 200, body = TenantView), (status = 403), (status = 404)))]
async fn update_tenant(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<TenantId>,
    Json(req): Json<UpdateTenant>,
) -> ApiResult<Json<TenantView>> {
    load(&state, &p, id, Perm::TenantWrite).await?;
    let name = valid_name(&req.name)?;
    mst_db::tenants::rename(&state.db, id, name).await?;
    audit(
        &state.db,
        Some(&p),
        Some(id),
        "tenant.rename",
        Some(("tenant", id.0)),
        json!({"name": name}),
    )
    .await;
    let t = mst_db::tenants::get(&state.db, id).await?;
    Ok(Json(views(&state, vec![t]).await?.remove(0)))
}

#[derive(Deserialize, ToSchema)]
pub struct BindDomain {
    /// Host name people reach the controllers at for this tenant, e.g.
    /// `mst.acme.com`. Its DNS, and the HTTPS proxy's certificate, must lead
    /// to the controllers.
    domain: String,
}

/// A domain name in the form it is bound in: lower case, without trailing
/// dot, labels of letters, digits and inner hyphens.
fn valid_domain(domain: &str) -> ApiResult<String> {
    let d = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    let label = |l: &str| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    };
    if d.len() > 253 || !d.split('.').all(label) {
        return Err(ApiError::bad_request(
            "a domain is a host name such as mst.example.com, without scheme, port or path",
        ));
    }
    Ok(d)
}

/// Only the provider's administrators bind domains: they decide which names
/// lead to the controllers, and a tenant must not claim another's (or the
/// provider's) sign-in page.
async fn authorize_domains(state: &AppState, p: &Principal) -> ApiResult<()> {
    let root = mst_db::tenants::root(&state.db)
        .await?
        .ok_or_else(|| ApiError::not_found("tenant"))?;
    authorize(&state.db, p, Perm::InfraWrite, root.id).await
}

/// Binds a domain name to a tenant. Signing in at that name starts from the
/// tenant: people of the tenant leave the tenant empty, and those of its
/// sub-tenants enter a path below it.
#[utoipa::path(post, path = "/tenants/{id}/domains", tag = "tenants", params(("id" = TenantId, Path)), request_body = BindDomain,
    responses((status = 200, body = TenantView), (status = 400), (status = 403), (status = 404),
        (status = 409, description = "The domain is bound already")))]
async fn bind_domain(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<TenantId>,
    Json(req): Json<BindDomain>,
) -> ApiResult<Json<TenantView>> {
    authorize_domains(&state, &p).await?;
    let domain = valid_domain(&req.domain)?;
    mst_db::tenants::get(&state.db, id).await?;
    mst_db::tenants::bind_domain(&state.db, id, &domain).await?;
    audit(
        &state.db,
        Some(&p),
        Some(id),
        "tenant.domain.bind",
        Some(("tenant", id.0)),
        json!({"domain": domain}),
    )
    .await;
    let t = mst_db::tenants::get(&state.db, id).await?;
    Ok(Json(views(&state, vec![t]).await?.remove(0)))
}

/// Removes a domain name from a tenant; signing in there starts from the
/// provider again.
#[utoipa::path(delete, path = "/tenants/{id}/domains/{domain}", tag = "tenants",
    params(("id" = TenantId, Path), ("domain" = String, Path)),
    responses((status = 204), (status = 403), (status = 404)))]
async fn unbind_domain(
    State(state): State<AppState>,
    p: Principal,
    Path((id, domain)): Path<(TenantId, String)>,
) -> ApiResult<StatusCode> {
    authorize_domains(&state, &p).await?;
    let domain = valid_domain(&domain)?;
    if !mst_db::tenants::unbind_domain(&state.db, id, &domain).await? {
        return Err(ApiError::not_found("domain"));
    }
    audit(
        &state.db,
        Some(&p),
        Some(id),
        "tenant.domain.unbind",
        Some(("tenant", id.0)),
        json!({"domain": domain}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// Deletes an empty tenant (no sub-tenants, VMs or images), with its users and
/// networks.
#[utoipa::path(delete, path = "/tenants/{id}", tag = "tenants", params(("id" = TenantId, Path)),
    responses((status = 204), (status = 403), (status = 404), (status = 409, description = "Not empty")))]
async fn delete_tenant(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<TenantId>,
) -> ApiResult<StatusCode> {
    let t = load(&state, &p, id, Perm::TenantRead).await?;
    authorize(&state.db, &p, Perm::TenantWrite, parent_of(&t)?).await?;
    let contents = mst_db::tenants::contents(&state.db, id).await?;
    if !contents.is_empty() {
        let what: Vec<_> = contents.iter().map(|(k, n)| format!("{n} {k}")).collect();
        return Err(ApiError::conflict(format!(
            "{} still has {}",
            t.name,
            what.join(", ")
        )));
    }
    let mut tx = state.db.begin().await?;
    mst_db::tenants::delete(&mut tx, id).await?;
    tx.commit().await?;
    // Its networks leave OVN in the next pass.
    state.net.kick();
    audit(
        &state.db,
        Some(&p),
        t.parent_id,
        "tenant.delete",
        Some(("tenant", id.0)),
        json!({"slug": t.slug, "name": t.name}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize, ToSchema)]
pub struct InheritedLimit {
    max: i64,
    /// The ancestor whose limit this is.
    tenant_name: String,
}

#[derive(Serialize, ToSchema)]
pub struct Quotas {
    /// This tenant's own limits per resource (`vms`, `vcpus`, `memory_mib`,
    /// `disk_gib`); absent means no limit of its own.
    limits: BTreeMap<String, i64>,
    /// The tightest limit an ancestor imposes, where one does.
    inherited: BTreeMap<String, InheritedLimit>,
    /// What the tenant and its sub-tenants use now.
    usage: BTreeMap<String, i64>,
}

#[utoipa::path(get, path = "/tenants/{id}/quotas", tag = "tenants", params(("id" = TenantId, Path)),
    responses((status = 200, body = Quotas), (status = 404)))]
async fn get_quotas(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<TenantId>,
) -> ApiResult<Json<Quotas>> {
    load(&state, &p, id, Perm::TenantRead).await?;
    let limits = mst_db::quotas::limits(&state.db, id).await?;
    let usage = mst_db::quotas::usage(&state.db, id).await?;
    let mut inherited = BTreeMap::new();
    let lineage = mst_db::tenants::lineage(&state.db, id).await?;
    for t in lineage.iter().rev().skip(1) {
        for (r, max) in mst_db::quotas::limits(&state.db, t.id).await? {
            let entry = inherited.entry(r).or_insert(InheritedLimit {
                max,
                tenant_name: t.name.clone(),
            });
            if max < entry.max {
                *entry = InheritedLimit {
                    max,
                    tenant_name: t.name.clone(),
                };
            }
        }
    }
    Ok(Json(Quotas {
        limits,
        inherited,
        usage,
    }))
}

/// Changes limits. Keys present are set (a number) or cleared (`null`);
/// absent keys are left alone. Set by the parent tenant's administrators.
#[utoipa::path(put, path = "/tenants/{id}/quotas", tag = "tenants", params(("id" = TenantId, Path)),
    request_body = BTreeMap<String, Option<i64>>, responses((status = 200, body = Quotas), (status = 400), (status = 403)))]
async fn set_quotas(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<TenantId>,
    Json(req): Json<BTreeMap<String, Option<i64>>>,
) -> ApiResult<Json<Quotas>> {
    let t = load(&state, &p, id, Perm::TenantRead).await?;
    authorize(&state.db, &p, Perm::QuotaWrite, parent_of(&t)?).await?;
    let mut tx = state.db.begin().await?;
    for (resource, max) in &req {
        mst_db::quotas::set_limit(&mut tx, id, resource, *max).await?;
    }
    tx.commit().await?;
    audit(
        &state.db,
        Some(&p),
        Some(id),
        "tenant.quotas",
        Some(("tenant", id.0)),
        json!(req),
    )
    .await;
    get_quotas(State(state), p, Path(id)).await
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct Grants {
    clusters: Vec<Uuid>,
    storage_pools: Vec<Uuid>,
    /// Networks the tenant's VPCs may route to and take floating IPs from.
    #[serde(default)]
    external_networks: Vec<Uuid>,
}

#[utoipa::path(get, path = "/tenants/{id}/grants", tag = "tenants", params(("id" = TenantId, Path)),
    responses((status = 200, body = Grants), (status = 404)))]
async fn get_grants(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<TenantId>,
) -> ApiResult<Json<Grants>> {
    load(&state, &p, id, Perm::TenantRead).await?;
    Ok(Json(Grants {
        clusters: mst_db::grants::list(&state.db, id, "cluster").await?,
        storage_pools: mst_db::grants::list(&state.db, id, "storage_pool").await?,
        external_networks: mst_db::grants::list(&state.db, id, "external_network").await?,
    }))
}

/// Replaces the clusters and storage pools a tenant may use. Each must be
/// usable by the parent; revoking one also revokes it from sub-tenants.
#[utoipa::path(put, path = "/tenants/{id}/grants", tag = "tenants", params(("id" = TenantId, Path)), request_body = Grants,
    responses((status = 200, body = Grants), (status = 400), (status = 403)))]
async fn set_grants(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<TenantId>,
    Json(req): Json<Grants>,
) -> ApiResult<Json<Grants>> {
    let t = load(&state, &p, id, Perm::TenantRead).await?;
    authorize(&state.db, &p, Perm::GrantWrite, parent_of(&t)?).await?;
    for c in &req.clusters {
        mst_db::clusters::get(&state.db, mst_core::ids::ClusterId(*c)).await?;
    }
    for sp in &req.storage_pools {
        let pool = mst_db::storage_pools::get(&state.db, mst_core::ids::StoragePoolId(*sp)).await?;
        if pool.kind == "local" {
            return Err(ApiError::bad_request(
                "local storage is available to every tenant; it needs no grant",
            ));
        }
    }
    for e in &req.external_networks {
        mst_db::external::get(&state.db, mst_core::ids::ExternalNetworkId(*e)).await?;
    }
    let mut tx = state.db.begin().await?;
    mst_db::grants::set(&mut tx, id, "cluster", &req.clusters).await?;
    mst_db::grants::set(&mut tx, id, "storage_pool", &req.storage_pools).await?;
    mst_db::grants::set(&mut tx, id, "external_network", &req.external_networks).await?;
    tx.commit().await?;
    audit(
        &state.db,
        Some(&p),
        Some(id),
        "tenant.grants",
        Some(("tenant", id.0)),
        json!(req),
    )
    .await;
    get_grants(State(state), p, Path(id)).await
}

#[cfg(test)]
mod tests {
    #[test]
    fn domains_are_plain_host_names() {
        assert_eq!(
            super::valid_domain(" Mst.Acme.COM. ").unwrap(),
            "mst.acme.com"
        );
        assert_eq!(
            super::valid_domain("acme.localhost").unwrap(),
            "acme.localhost"
        );
        for bad in [
            "",
            "https://mst.acme.com",
            "mst.acme.com:443",
            "mst.acme.com/x",
            "-a.com",
            "a..com",
            "a_b.com",
            "*.acme.com",
        ] {
            assert!(super::valid_domain(bad).is_err(), "{bad:?}");
        }
    }

    use super::*;

    #[test]
    fn slugs_are_dns_labels() {
        assert!(valid_slug("acme-dev2"));
        for bad in ["", "Acme", "-a", "a-", "a_b", "a/b", &"x".repeat(64)] {
            assert!(!valid_slug(bad), "{bad:?}");
        }
    }
}
