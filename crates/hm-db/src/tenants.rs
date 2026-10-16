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
