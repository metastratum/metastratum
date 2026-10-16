use chrono::{DateTime, Utc};
use hm_core::ids::TenantId;
use hm_core::tenant::TenantPath;
use sqlx::PgExecutor;

use crate::{Result, conflict_on_unique};

#[derive(Clone, Debug)]
pub struct Tenant {
    pub id: TenantId,
    pub parent_id: Option<TenantId>,
    pub slug: String,
    pub name: String,
    pub path: TenantPath,
    pub created_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct Row {
    id: TenantId,
    parent_id: Option<TenantId>,
    slug: String,
    name: String,
    path: String,
    created_at: DateTime<Utc>,
}

impl From<Row> for Tenant {
    fn from(r: Row) -> Self {
        Self {
            id: r.id,
            parent_id: r.parent_id,
            slug: r.slug,
            name: r.name,
            path: TenantPath::parse(&r.path).expect("tenant path maintained by trigger"),
            created_at: r.created_at,
        }
    }
}

macro_rules! tenant_columns {
    () => {
        "id, parent_id, slug, name, path::text AS path, created_at"
    };
}

pub async fn create(
    db: impl PgExecutor<'_>,
    parent_id: Option<TenantId>,
    slug: &str,
    name: &str,
) -> Result<Tenant> {
    let row: Row = sqlx::query_as(concat!(
        "INSERT INTO tenants (id, parent_id, slug, name) VALUES ($1, $2, $3, $4) RETURNING ",
        tenant_columns!()
    ))
    .bind(TenantId::new())
    .bind(parent_id)
    .bind(slug)
    .bind(name)
    .fetch_one(db)
    .await
    .map_err(|e| conflict_on_unique(e, "tenant"))?;
    Ok(row.into())
}

pub async fn get(db: impl PgExecutor<'_>, id: TenantId) -> Result<Tenant> {
    sqlx::query_as::<_, Row>(concat!(
        "SELECT ",
        tenant_columns!(),
        " FROM tenants WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(db)
    .await?
    .map(Into::into)
    .ok_or_else(|| {
        hm_core::Error::NotFound {
            kind: "tenant",
            id: id.to_string(),
        }
        .into()
    })
}

pub async fn root(db: impl PgExecutor<'_>) -> Result<Option<Tenant>> {
    Ok(sqlx::query_as::<_, Row>(concat!(
        "SELECT ",
        tenant_columns!(),
        " FROM tenants WHERE parent_id IS NULL"
    ))
    .fetch_optional(db)
    .await?
    .map(Into::into))
}

/// The tenant and all its descendants, ordered so parents precede children.
pub async fn subtree(db: impl PgExecutor<'_>, id: TenantId) -> Result<Vec<Tenant>> {
    let rows: Vec<Row> = sqlx::query_as(concat!(
        "SELECT ",
        tenant_columns!(),
        " FROM tenants
         WHERE path <@ (SELECT path FROM tenants WHERE id = $1)
         ORDER BY nlevel(path), slug"
    ))
    .bind(id)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// The tenant's ancestors and the tenant itself, root first.
pub async fn lineage(db: impl PgExecutor<'_>, id: TenantId) -> Result<Vec<Tenant>> {
    let rows: Vec<Row> = sqlx::query_as(concat!(
        "SELECT ",
        tenant_columns!(),
        " FROM tenants
         WHERE path @> (SELECT path FROM tenants WHERE id = $1)
         ORDER BY nlevel(path)"
    ))
    .bind(id)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// Looks up a direct child of `parent` (or the root, for `None`) by slug.
pub async fn child_by_slug(
    db: impl PgExecutor<'_>,
    parent: Option<TenantId>,
    slug: &str,
) -> Result<Option<Tenant>> {
    Ok(sqlx::query_as::<_, Row>(concat!(
        "SELECT ",
        tenant_columns!(),
        " FROM tenants WHERE parent_id IS NOT DISTINCT FROM $1 AND slug = $2"
    ))
    .bind(parent)
    .bind(slug)
    .fetch_optional(db)
    .await?
    .map(Into::into))
}

pub async fn rename(db: impl PgExecutor<'_>, id: TenantId, name: &str) -> Result<()> {
    sqlx::query("UPDATE tenants SET name = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(name)
        .execute(db)
        .await?;
    Ok(())
}

/// What still lives in a tenant (not its subtree), as `(kind, count)` for
/// kinds with a non-zero count. Tenants must be empty to be deleted.
pub async fn contents(db: impl PgExecutor<'_>, id: TenantId) -> Result<Vec<(String, i64)>> {
    let row: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM tenants WHERE parent_id = $1),
                (SELECT count(*) FROM vms WHERE tenant_id = $1),
                (SELECT count(*) FROM images WHERE tenant_id = $1)",
    )
    .bind(id)
    .fetch_one(db)
    .await?;
    Ok([("sub-tenants", row.0), ("VMs", row.1), ("images", row.2)]
        .into_iter()
        .filter(|(_, n)| *n > 0)
        .map(|(k, n)| (k.to_owned(), n))
        .collect())
}

/// Deletes a tenant with its users, role bindings, networks, quotas and
/// grants. Callers check it is empty first (see [`contents`]).
pub async fn delete(db: impl PgExecutor<'_>, id: TenantId) -> Result<()> {
    sqlx::query("DELETE FROM tenants WHERE id = $1")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// All tenants in the subtrees rooted at `roots`, parents before children.
pub async fn in_subtrees(db: impl PgExecutor<'_>, roots: &[TenantId]) -> Result<Vec<Tenant>> {
    let rows: Vec<Row> = sqlx::query_as(concat!(
        "SELECT ",
        tenant_columns!(),
        " FROM tenants
         WHERE path <@ ANY (SELECT path FROM tenants WHERE id = ANY($1))
         ORDER BY nlevel(path), slug"
    ))
    .bind(roots.iter().map(|t| t.0).collect::<Vec<_>>())
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// Slug paths below the root (`acme/dev`; empty for the root) of tenants.
pub async fn slug_paths(
    db: impl PgExecutor<'_>,
    ids: &[TenantId],
) -> Result<std::collections::HashMap<TenantId, String>> {
    let rows: Vec<(TenantId, String)> = sqlx::query_as(
        "SELECT t.id, coalesce((SELECT string_agg(a.slug, '/' ORDER BY nlevel(a.path)) FROM tenants a
                                WHERE a.path @> t.path AND a.parent_id IS NOT NULL), '')
         FROM tenants t WHERE t.id = ANY($1)",
    )
    .bind(ids.iter().map(|t| t.0).collect::<Vec<_>>())
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().collect())
}
