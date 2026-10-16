//! Authorization: every handler states the permission it needs on the tenant
//! that owns the resource. The rules live in `mst-authz`; this module loads
//! what they need.

use mst_authz::Binding;
use mst_core::ids::{TenantId, UserId};
use mst_core::tenant::TenantPath;
use mst_db::sqlx::PgPool;

use crate::error::{ApiError, ApiResult};

pub use mst_authz::{Perm, Role};

#[derive(Clone, Debug)]
pub struct Principal {
    pub user_id: UserId,
    pub username: String,
    /// The tenant the user belongs to (and signs in to).
    pub tenant_id: TenantId,
    pub tenant_path: TenantPath,
    /// What the principal may do: its user's roles, narrowed to the token's
    /// scopes when it signed in with a scoped token.
    pub bindings: Vec<Binding>,
    /// Signed in with a scoped token: it may not act as its user beyond
    /// those scopes, so not change the account or make tokens either.
    pub limited: bool,
}

impl Principal {
    pub async fn load(db: &PgPool, user_id: UserId) -> ApiResult<Self> {
        let user = mst_db::users::get(db, user_id).await?;
        let tenant = mst_db::tenants::get(db, user.tenant_id).await?;
        let bindings = mst_db::rbac::grants_of(db, user_id)
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
            limited: false,
        })
    }

    /// Narrows the principal to an API token's scopes. Scopes at tenants
    /// that no longer exist grant nothing.
    pub async fn limit_to(
        mut self,
        db: &PgPool,
        scopes: &[mst_db::rbac::TokenScope],
    ) -> ApiResult<Self> {
        let ids: Vec<TenantId> = scopes.iter().map(|s| s.tenant_id).collect();
        let paths = mst_db::rbac::tenant_paths(db, &ids).await?;
        let scope: Vec<Binding> = scopes
            .iter()
            .filter_map(|s| {
                Some(Binding {
                    tenant: paths.iter().find(|(id, _)| *id == s.tenant_id)?.1.clone(),
                    role: s.role.parse().ok()?,
                })
            })
            .collect();
        self.bindings = mst_authz::intersect(&self.bindings, &scope);
        self.limited = true;
        Ok(self)
    }

    /// Requires a principal that may act as its user: not a scoped token.
    pub fn require_full(&self) -> ApiResult<()> {
        if self.limited {
            Err(ApiError::new(
                axum::http::StatusCode::FORBIDDEN,
                "forbidden",
                "a scoped API token cannot do this; sign in, or use an unscoped token",
            ))
        } else {
            Ok(())
        }
    }

    pub fn can(&self, perm: Perm, target: &TenantPath) -> bool {
        mst_authz::allows(&self.bindings, perm, target)
    }

    /// Whether the principal administers the provider (root-level infra).
    pub fn is_provider_admin(&self) -> bool {
        self.bindings
            .iter()
            .any(|b| b.tenant.depth() == 1 && b.role.allows(Perm::InfraWrite))
    }

    /// Outermost tenants where the principal holds `perm`.
    pub fn scopes(&self, perm: Perm) -> Vec<TenantId> {
        mst_authz::scopes(&self.bindings, perm)
            .into_iter()
            .map(|p| p.tenant())
            .collect()
    }
}

async fn path_of(db: &PgPool, p: &Principal, tenant_id: TenantId) -> ApiResult<TenantPath> {
    if tenant_id == p.tenant_id {
        return Ok(p.tenant_path.clone());
    }
    Ok(mst_db::tenants::get(db, tenant_id).await?.path)
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
        let root = mst_db::tenants::root(db)
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
    let entry = mst_db::audit::Entry {
        user_id: p.map(|p| p.user_id),
        username: p.map(|p| p.username.as_str()),
        tenant_id,
        action,
        target,
        success: true,
        details,
    };
    if let Err(e) = mst_db::audit::record(db, entry).await {
        tracing::error!(error = %e, action, "writing audit log");
    }
}
