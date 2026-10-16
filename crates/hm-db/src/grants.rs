//! Provider resources (clusters, storage pools, external networks) granted to
//! tenants.
//!
//! The root tenant may use everything. Any other tenant may use what is
//! granted to it, and a tenant's parent can only grant what it may use
//! itself. Revoking a grant also revokes it from the tenant's subtree.

use hm_core::ids::TenantId;
use sqlx::{PgConnection, PgExecutor};
use uuid::Uuid;

use crate::Result;

pub const KINDS: [&str; 3] = ["cluster", "storage_pool", "external_network"];

pub async fn list(db: impl PgExecutor<'_>, tenant_id: TenantId, kind: &str) -> Result<Vec<Uuid>> {
    Ok(sqlx::query_scalar(
        "SELECT ref_id FROM tenant_grants WHERE tenant_id = $1 AND kind = $2 ORDER BY ref_id",
    )
    .bind(tenant_id)
    .bind(kind)
    .fetch_all(db)
    .await?)
}

/// Whether `tenant_id` may use the resource.
pub async fn may_use(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    kind: &str,
    ref_id: Uuid,
) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM tenants WHERE id = $1 AND parent_id IS NULL)
             OR EXISTS (SELECT 1 FROM tenant_grants WHERE tenant_id = $1 AND kind = $2 AND ref_id = $3)",
    )
    .bind(tenant_id)
    .bind(kind)
    .bind(ref_id)
    .fetch_one(db)
    .await?)
}

/// Replaces a tenant's grants of one kind. Each must be usable by the parent.
/// Grants removed here are removed from the whole subtree too.
pub async fn set(
    db: &mut PgConnection,
    tenant_id: TenantId,
    kind: &str,
    refs: &[Uuid],
) -> Result<()> {
    if !KINDS.contains(&kind) {
        return Err(hm_core::Error::InvalidArgument(format!("unknown grant kind {kind}")).into());
    }
    let parent: Option<TenantId> =
        sqlx::query_scalar("SELECT parent_id FROM tenants WHERE id = $1")
            .bind(tenant_id)
            .fetch_optional(&mut *db)
            .await?
            .ok_or_else(|| hm_core::Error::NotFound {
                kind: "tenant",
                id: tenant_id.to_string(),
            })?;
    let Some(parent) = parent else {
        return Err(hm_core::Error::InvalidArgument(
            "the root tenant may use everything already".into(),
        )
        .into());
    };
    for r in refs {
        if !may_use(&mut *db, parent, kind, *r).await? {
            return Err(hm_core::Error::InvalidArgument(format!(
                "the parent tenant may not use {kind} {r}"
            ))
            .into());
        }
    }
    sqlx::query(
        "DELETE FROM tenant_grants
         WHERE kind = $2 AND NOT (ref_id = ANY($3))
           AND tenant_id IN (SELECT id FROM tenants WHERE path <@ (SELECT path FROM tenants WHERE id = $1))",
    )
    .bind(tenant_id)
    .bind(kind)
    .bind(refs)
    .execute(&mut *db)
    .await?;
    sqlx::query(
        "INSERT INTO tenant_grants (tenant_id, kind, ref_id) SELECT $1, $2, unnest($3::uuid[])
         ON CONFLICT DO NOTHING",
    )
    .bind(tenant_id)
    .bind(kind)
    .bind(refs)
    .execute(&mut *db)
    .await?;
    Ok(())
}
