//! Security groups (stateful allow rules on NICs) and guardrail policies
//! (deny rules a tenant sets for its whole subtree).

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use hm_core::ids::{PolicyId, SecurityGroupId, SecurityRuleId, TenantId};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::vpcs::check_name;
use crate::authz::{Perm, Principal, audit, authorize, authorize_resource};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_groups, create_group))
        .routes(routes!(update_group, delete_group))
        .routes(routes!(add_rule))
        .routes(routes!(delete_rule))
        .routes(routes!(list_policies, create_policy))
        .routes(routes!(delete_policy))
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    /// Defaults to the caller's tenant.
    tenant_id: Option<TenantId>,
    #[serde(default)]
    descendants: bool,
}

/// Traffic to match. Ports apply to tcp and udp only.
#[derive(Deserialize, ToSchema)]
pub struct RuleSpec {
    /// `ingress` (to the VM) or `egress` (from it).
    direction: String,
    /// `any`, `tcp`, `udp` or `icmp`.
    protocol: String,
    port_min: Option<u16>,
    /// Defaults to `port_min`.
    port_max: Option<u16>,
    /// The other end, e.g. `0.0.0.0/0`. For rules, give this or
    /// `remote_group_id`; for policies it defaults to anywhere.
    remote_cidr: Option<String>,
    /// Members of this security group (same tenant).
    remote_group_id: Option<SecurityGroupId>,
    #[serde(default)]
    description: String,
}

struct Checked {
    direction: &'static str,
    protocol: &'static str,
    ports: Option<(i32, i32)>,
    remote_cidr: Option<String>,
}

fn check_rule(r: &RuleSpec) -> ApiResult<Checked> {
    let direction = match r.direction.as_str() {
        "ingress" => "ingress",
        "egress" => "egress",
        _ => return Err(ApiError::bad_request("direction must be ingress or egress")),
    };
    let protocol = match r.protocol.as_str() {
        "any" => "any",
        "tcp" => "tcp",
        "udp" => "udp",
        "icmp" => "icmp",
        _ => {
            return Err(ApiError::bad_request(
                "protocol must be any, tcp, udp or icmp",
            ));
        }
    };
    let ports = match (r.port_min, r.port_max) {
        (None, None) => None,
        (Some(a), b) => {
            let b = b.unwrap_or(a);
            if a == 0 || b < a {
                return Err(ApiError::bad_request("ports must be 1-65535, lowest first"));
            }
            if !matches!(protocol, "tcp" | "udp") {
                return Err(ApiError::bad_request("ports apply to tcp and udp only"));
            }
            Some((i32::from(a), i32::from(b)))
        }
        (None, Some(_)) => {
            return Err(ApiError::bad_request(
                "give port_min (and optionally port_max)",
            ));
        }
    };
    let remote_cidr = match &r.remote_cidr {
        Some(c) => Some(
            hm_netplan::Cidr::parse(c.trim())
                .ok_or_else(|| {
                    ApiError::bad_request("remote_cidr must be an IPv4 address or range")
                })?
                .to_string(),
        ),
        None => None,
    };
    if r.description.len() > 256 {
        return Err(ApiError::bad_request(
            "descriptions are at most 256 characters",
        ));
    }
    Ok(Checked {
        direction,
        protocol,
        ports,
        remote_cidr,
    })
}

#[derive(Serialize, ToSchema)]
pub struct RuleView {
    id: SecurityRuleId,
    direction: String,
    protocol: String,
    port_min: Option<i32>,
    port_max: Option<i32>,
    remote_cidr: Option<String>,
    remote_group_id: Option<SecurityGroupId>,
    remote_group_name: Option<String>,
    description: String,
}

#[derive(Serialize, ToSchema)]
pub struct SecurityGroupView {
    id: SecurityGroupId,
    tenant_id: TenantId,
    name: String,
    description: String,
    /// What the group allows; anything else is dropped. Replies to allowed
    /// traffic are always allowed.
    rules: Vec<RuleView>,
    /// NICs in the group.
    members: i64,
    created_at: DateTime<Utc>,
}

async fn group_views(
    state: &AppState,
    groups: Vec<hm_db::netsec::SecurityGroup>,
) -> ApiResult<Vec<SecurityGroupView>> {
    let ids: Vec<_> = groups.iter().map(|g| g.id).collect();
    let rules = hm_db::netsec::rules_of(&state.db, &ids).await?;
    let names: HashMap<SecurityGroupId, String> = {
        let remote: Vec<_> = rules.iter().filter_map(|r| r.remote_group_id).collect();
        let mut m = HashMap::new();
        for id in remote {
            if let std::collections::hash_map::Entry::Vacant(e) = m.entry(id)
                && let Ok(g) = hm_db::netsec::get_group(&state.db, id).await
            {
                e.insert(g.name);
            }
        }
        m
    };
    let members: HashMap<_, _> = hm_db::netsec::member_counts(&state.db)
        .await?
        .into_iter()
        .collect();
    Ok(groups
        .into_iter()
        .map(|g| SecurityGroupView {
            rules: rules
                .iter()
                .filter(|r| r.security_group_id == g.id)
                .map(|r| RuleView {
                    id: r.id,
                    direction: r.direction.clone(),
                    protocol: r.protocol.clone(),
                    port_min: r.port_min,
                    port_max: r.port_max,
                    remote_cidr: r.remote_cidr.clone(),
                    remote_group_id: r.remote_group_id,
                    remote_group_name: r.remote_group_id.and_then(|id| names.get(&id).cloned()),
                    description: r.description.clone(),
                })
                .collect(),
            members: members.get(&g.id).copied().unwrap_or(0),
            id: g.id,
            tenant_id: g.tenant_id,
            name: g.name,
            description: g.description,
            created_at: g.created_at,
        })
        .collect())
}

#[utoipa::path(get, path = "/security-groups", tag = "networks", params(ListQuery),
    responses((status = 200, body = Vec<SecurityGroupView>)))]
async fn list_groups(
    State(state): State<AppState>,
    p: Principal,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<SecurityGroupView>>> {
    let tenant = q.tenant_id.unwrap_or(p.tenant_id);
    authorize(&state.db, &p, Perm::NetworkRead, tenant).await?;
    let groups = hm_db::netsec::list_groups(&state.db, tenant, q.descendants).await?;
    Ok(Json(group_views(&state, groups).await?))
}

#[derive(Deserialize, ToSchema)]
pub struct CreateGroup {
    tenant_id: Option<TenantId>,
    name: String,
    #[serde(default)]
    description: String,
}

#[utoipa::path(post, path = "/security-groups", tag = "networks", request_body = CreateGroup,
    responses((status = 201, body = SecurityGroupView), (status = 400), (status = 403), (status = 409)))]
async fn create_group(
    State(state): State<AppState>,
    p: Principal,
    Json(req): Json<CreateGroup>,
) -> ApiResult<(StatusCode, Json<SecurityGroupView>)> {
    let tenant = req.tenant_id.unwrap_or(p.tenant_id);
    authorize(&state.db, &p, Perm::NetworkWrite, tenant).await?;
    let name = check_name(&req.name)?;
    let g = hm_db::netsec::create_group(
        &state.db,
        SecurityGroupId::new(),
        tenant,
        name,
        req.description.trim(),
    )
    .await?;
    audit(
        &state.db,
        Some(&p),
        Some(tenant),
        "security_group.create",
        Some(("security_group", g.id.0)),
        json!({"name": name}),
    )
    .await;
    state.net.kick();
    Ok((
        StatusCode::CREATED,
        Json(group_views(&state, vec![g]).await?.remove(0)),
    ))
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateGroup {
    name: Option<String>,
    description: Option<String>,
}

#[utoipa::path(patch, path = "/security-groups/{id}", tag = "networks", params(("id" = SecurityGroupId, Path)),
    request_body = UpdateGroup, responses((status = 200, body = SecurityGroupView), (status = 403), (status = 404)))]
async fn update_group(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<SecurityGroupId>,
    Json(req): Json<UpdateGroup>,
) -> ApiResult<Json<SecurityGroupView>> {
    let g = hm_db::netsec::get_group(&state.db, id).await?;
    authorize_resource(
        &state.db,
        &p,
        Perm::NetworkWrite,
        g.tenant_id,
        "security group",
    )
    .await?;
    let name = match &req.name {
        Some(n) => check_name(n)?.to_owned(),
        None => g.name.clone(),
    };
    let description = req
        .description
        .as_deref()
        .map(str::trim)
        .unwrap_or(&g.description);
    hm_db::netsec::update_group(&state.db, id, &name, description).await?;
    audit(
        &state.db,
        Some(&p),
        Some(g.tenant_id),
        "security_group.update",
        Some(("security_group", id.0)),
        json!({"name": name}),
    )
    .await;
    let g = hm_db::netsec::get_group(&state.db, id).await?;
    Ok(Json(group_views(&state, vec![g]).await?.remove(0)))
}

#[utoipa::path(delete, path = "/security-groups/{id}", tag = "networks", params(("id" = SecurityGroupId, Path)),
    responses((status = 204), (status = 403), (status = 404), (status = 409, description = "In use")))]
async fn delete_group(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<SecurityGroupId>,
) -> ApiResult<StatusCode> {
    let g = hm_db::netsec::get_group(&state.db, id).await?;
    authorize_resource(
        &state.db,
        &p,
        Perm::NetworkWrite,
        g.tenant_id,
        "security group",
    )
    .await?;
    hm_db::netsec::delete_group(&state.db, id).await?;
    audit(
        &state.db,
        Some(&p),
        Some(g.tenant_id),
        "security_group.delete",
        Some(("security_group", id.0)),
        json!({"name": g.name}),
    )
    .await;
    state.net.kick();
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(post, path = "/security-groups/{id}/rules", tag = "networks", params(("id" = SecurityGroupId, Path)),
    request_body = RuleSpec, responses((status = 201, body = SecurityGroupView), (status = 400), (status = 403), (status = 404)))]
async fn add_rule(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<SecurityGroupId>,
    Json(req): Json<RuleSpec>,
) -> ApiResult<(StatusCode, Json<SecurityGroupView>)> {
    let g = hm_db::netsec::get_group(&state.db, id).await?;
    authorize_resource(
        &state.db,
        &p,
        Perm::NetworkWrite,
        g.tenant_id,
        "security group",
    )
    .await?;
    let c = check_rule(&req)?;
    match (&c.remote_cidr, req.remote_group_id) {
        (Some(_), None) => {}
        (None, Some(r)) => {
            let remote = hm_db::netsec::get_group(&state.db, r).await?;
            if remote.tenant_id != g.tenant_id {
                return Err(ApiError::bad_request(
                    "a rule can only refer to groups of the same tenant",
                ));
            }
        }
        _ => {
            return Err(ApiError::bad_request(
                "give either remote_cidr or remote_group_id",
            ));
        }
    }
    let rule = hm_db::netsec::add_rule(
        &state.db,
        SecurityRuleId::new(),
        id,
        &hm_db::netsec::RuleSpec {
            direction: c.direction,
            protocol: c.protocol,
            ports: c.ports,
            remote_cidr: c.remote_cidr.as_deref(),
            remote_group_id: req.remote_group_id,
            description: req.description.trim(),
        },
    )
    .await?;
    audit(
        &state.db,
        Some(&p),
        Some(g.tenant_id),
        "security_group.rule.add",
        Some(("security_group", id.0)),
        json!({"rule": rule.id, "direction": rule.direction, "protocol": rule.protocol,
               "ports": [rule.port_min, rule.port_max], "remote_cidr": rule.remote_cidr,
               "remote_group_id": rule.remote_group_id}),
    )
    .await;
    state.net.kick();
    Ok((
        StatusCode::CREATED,
        Json(group_views(&state, vec![g]).await?.remove(0)),
    ))
}

#[utoipa::path(delete, path = "/security-group-rules/{id}", tag = "networks", params(("id" = SecurityRuleId, Path)),
    responses((status = 204), (status = 403), (status = 404)))]
async fn delete_rule(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<SecurityRuleId>,
) -> ApiResult<StatusCode> {
    let rule = hm_db::netsec::get_rule(&state.db, id).await?;
    let g = hm_db::netsec::get_group(&state.db, rule.security_group_id).await?;
    authorize_resource(
        &state.db,
        &p,
        Perm::NetworkWrite,
        g.tenant_id,
        "security group",
    )
    .await?;
    hm_db::netsec::delete_rule(&state.db, id).await?;
    audit(
        &state.db,
        Some(&p),
        Some(g.tenant_id),
        "security_group.rule.delete",
        Some(("security_group", g.id.0)),
        json!({"rule": id}),
    )
    .await;
    state.net.kick();
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize, ToSchema)]
pub struct PolicyView {
    id: PolicyId,
    /// The tenant that set it; it covers that tenant's whole subtree.
    tenant_id: TenantId,
    tenant_name: String,
    /// Set by a tenant above this one: it cannot be changed from here.
    inherited: bool,
    direction: String,
    protocol: String,
    port_min: Option<i32>,
    port_max: Option<i32>,
    remote_cidr: String,
    description: String,
    created_at: DateTime<Utc>,
}

/// Guardrails that apply to a tenant's VMs: its own and its ancestors'.
/// They deny, and no security group can allow what they deny.
#[utoipa::path(get, path = "/tenants/{id}/network-policies", tag = "networks", params(("id" = TenantId, Path)),
    responses((status = 200, body = Vec<PolicyView>), (status = 404)))]
async fn list_policies(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<TenantId>,
) -> ApiResult<Json<Vec<PolicyView>>> {
    authorize_resource(&state.db, &p, Perm::NetworkRead, id, "tenant").await?;
    let lineage = hm_db::tenants::lineage(&state.db, id).await?;
    let names: HashMap<_, _> = lineage.iter().map(|t| (t.id, t.name.clone())).collect();
    Ok(Json(
        hm_db::netsec::policies_for(&state.db, id)
            .await?
            .into_iter()
            .map(|x| PolicyView {
                tenant_name: names.get(&x.tenant_id).cloned().unwrap_or_default(),
                inherited: x.tenant_id != id,
                id: x.id,
                tenant_id: x.tenant_id,
                direction: x.direction,
                protocol: x.protocol,
                port_min: x.port_min,
                port_max: x.port_max,
                remote_cidr: x.remote_cidr,
                description: x.description,
                created_at: x.created_at,
            })
            .collect(),
    ))
}

#[utoipa::path(post, path = "/tenants/{id}/network-policies", tag = "networks", params(("id" = TenantId, Path)),
    request_body = RuleSpec, responses((status = 201, body = Vec<PolicyView>), (status = 400), (status = 403)))]
async fn create_policy(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<TenantId>,
    Json(req): Json<RuleSpec>,
) -> ApiResult<(StatusCode, Json<Vec<PolicyView>>)> {
    authorize_resource(&state.db, &p, Perm::PolicyWrite, id, "tenant").await?;
    if req.remote_group_id.is_some() {
        return Err(ApiError::bad_request(
            "policies match addresses (remote_cidr), not groups",
        ));
    }
    let c = check_rule(&req)?;
    let policy = hm_db::netsec::create_policy(
        &state.db,
        PolicyId::new(),
        id,
        &hm_db::netsec::RuleSpec {
            direction: c.direction,
            protocol: c.protocol,
            ports: c.ports,
            remote_cidr: c.remote_cidr.as_deref(),
            remote_group_id: None,
            description: req.description.trim(),
        },
        p.user_id,
    )
    .await?;
    audit(
        &state.db,
        Some(&p),
        Some(id),
        "network_policy.create",
        Some(("network_policy", policy.id.0)),
        json!({"direction": policy.direction, "protocol": policy.protocol,
               "ports": [policy.port_min, policy.port_max], "remote_cidr": policy.remote_cidr}),
    )
    .await;
    state.net.kick();
    let all = list_policies(State(state), p, Path(id)).await?;
    Ok((StatusCode::CREATED, all))
}

#[utoipa::path(delete, path = "/network-policies/{id}", tag = "networks", params(("id" = PolicyId, Path)),
    responses((status = 204), (status = 403), (status = 404)))]
async fn delete_policy(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<PolicyId>,
) -> ApiResult<StatusCode> {
    let policy = hm_db::netsec::get_policy(&state.db, id).await?;
    authorize_resource(&state.db, &p, Perm::PolicyWrite, policy.tenant_id, "policy").await?;
    hm_db::netsec::delete_policy(&state.db, id).await?;
    audit(
        &state.db,
        Some(&p),
        Some(policy.tenant_id),
        "network_policy.delete",
        Some(("network_policy", id.0)),
        json!({}),
    )
    .await;
    state.net.kick();
    Ok(StatusCode::NO_CONTENT)
}
