//! Authorization hook.
//!
//! Every handler calls [`authorize`] with the permission it needs and the
//! tenant that owns the resource. M1 has no roles yet: a user may act on
//! their home tenant and everything below it, and provider-only operations
//! (hosts) require a home tenant at the root. M2 replaces the body with role
//! bindings inherited down the tree; call sites stay the same.

use hm_core::ids::TenantId;
use hm_core::tenant::TenantPath;
use hm_db::sqlx::PgPool;

use crate::error::{ApiError, ApiResult};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Perm {
    VmRead,
    VmWrite,
    VmConsole,
    ImageRead,
    ImageWrite,
    TaskRead,
    /// Provider-level: hypervisor hosts and enrollment.
    HostAdmin,
}

#[derive(Clone, Debug)]
pub struct Principal {
    pub user_id: hm_core::ids::UserId,
    pub tenant_id: TenantId,
    pub tenant_path: TenantPath,
}

pub async fn authorize(
    db: &PgPool,
    p: &Principal,
    perm: Perm,
    tenant_id: TenantId,
) -> ApiResult<()> {
    if perm == Perm::HostAdmin {
        return if p.tenant_path.depth() == 1 {
            Ok(())
        } else {
            Err(ApiError::forbidden())
        };
    }
    if p.tenant_id == tenant_id {
        return Ok(());
    }
    let target = hm_db::tenants::get(db, tenant_id).await?;
    if p.tenant_path.is_ancestor_or_self_of(&target.path) {
        Ok(())
    } else {
        Err(ApiError::forbidden())
    }
}

/// Like [`authorize`], but reports a denial as "not found" so callers cannot
/// probe for resources in tenants they cannot see.
pub async fn authorize_resource(
    db: &PgPool,
    p: &Principal,
    perm: Perm,
    tenant_id: TenantId,
    kind: &str,
) -> ApiResult<()> {
    authorize(db, p, perm, tenant_id).await.map_err(|e| {
        if e.code == "permission_denied" {
            ApiError::not_found(kind)
        } else {
            e
        }
    })
}
