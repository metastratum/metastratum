//! Authorization: every handler states the permission it needs on the tenant
//! that owns the resource. The rules live in `hm-authz`; this module loads
//! what they need.

use hm_authz::Binding;
use hm_core::ids::{TenantId, UserId};
use hm_core::tenant::TenantPath;
use hm_db::sqlx::PgPool;

use crate::error::{ApiError, ApiResult};

pub use hm_authz::{Perm, Role};

#[derive(Clone, Debug)]
pub struct Principal {
    pub user_id: UserId,
    pub username: String,
    /// The tenant the user belongs to (and signs in to).
    pub tenant_id: TenantId,
    pub tenant_path: TenantPath,
    pub bindings: Vec<Binding>,
}

impl Principal {
    pub async fn load(db: &PgPool, user_id: UserId) -> ApiResult<Self> {
        let user = hm_db::users::get(db, user_id).await?;
        let tenant = hm_db::tenants::get(db, user.tenant_id).await?;
        let bindings = hm_db::rbac::grants_of(db, user_id)
            .await?
            .into_iter()
            .filter_map(|(tenant, role)| {
                Some(Binding {
                    tenant,
                    role: role.parse().ok()?,
                })
            })
            .collect();
        Ok(Self {
            user_id,
            username: user.username,
            tenant_id: tenant.id,
            tenant_path: tenant.path,
            bindings,
        })
    }

    pub fn can(&self, perm: Perm, target: &TenantPath) -> bool {
        hm_authz::allows(&self.bindings, perm, target)
    }

    /// Whether the principal administers the provider (root-level infra).
    pub fn is_provider_admin(&self) -> bool {
        self.bindings
            .iter()
            .any(|b| b.tenant.depth() == 1 && b.role.allows(Perm::InfraWrite))
    }

    /// Outermost tenants where the principal holds `perm`.
    pub fn scopes(&self, perm: Perm) -> Vec<TenantId> {
        hm_authz::scopes(&self.bindings, perm)
            .into_iter()
            .map(|p| p.tenant())
            .collect()
    }
}

async fn path_of(db: &PgPool, p: &Principal, tenant_id: TenantId) -> ApiResult<TenantPath> {
    if tenant_id == p.tenant_id {
        return Ok(p.tenant_path.clone());
    }
    Ok(hm_db::tenants::get(db, tenant_id).await?.path)
}

/// Requires `perm` on `tenant_id`. Infrastructure permissions are checked on
/// the root tenant regardless of `tenant_id`.
pub async fn authorize(
    db: &PgPool,
    p: &Principal,
    perm: Perm,
    tenant_id: TenantId,
) -> ApiResult<()> {
    let target = if matches!(perm, Perm::InfraRead | Perm::InfraWrite) {
        let root = hm_db::tenants::root(db)
            .await?
            .ok_or_else(ApiError::forbidden)?;
        root.path
    } else {
        path_of(db, p, tenant_id).await?
    };
    if p.can(perm, &target) {
        Ok(())
    } else {
        Err(ApiError::forbidden())
    }
}

/// Like [`authorize`], but reports a denial as "not found" so callers cannot
/// probe for resources in tenants they cannot see. Holders of a weaker
/// permission that lets them see the resource get a plain 403 instead.
pub async fn authorize_resource(
    db: &PgPool,
    p: &Principal,
    perm: Perm,
    tenant_id: TenantId,
    kind: &str,
) -> ApiResult<()> {
    let path = path_of(db, p, tenant_id).await?;
    if p.can(perm, &path) {
        return Ok(());
    }
    let visible = p.can(Perm::TenantRead, &path);
    Err(if visible {
        ApiError::forbidden()
    } else {
        ApiError::not_found(kind)
    })
}

/// Records an audit entry. Failures are logged, never surfaced: auditing must
/// not turn a completed change into an error response.
pub async fn audit(
    db: &PgPool,
    p: Option<&Principal>,
    tenant_id: Option<TenantId>,
    action: &str,
    target: Option<(&str, uuid::Uuid)>,
    details: serde_json::Value,
) {
    let entry = hm_db::audit::Entry {
        user_id: p.map(|p| p.user_id),
        username: p.map(|p| p.username.as_str()),
        tenant_id,
        action,
        target,
        success: true,
        details,
    };
    if let Err(e) = hm_db::audit::record(db, entry).await {
        tracing::error!(error = %e, action, "writing audit log");
    }
}
