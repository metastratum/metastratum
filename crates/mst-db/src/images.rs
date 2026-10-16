use chrono::{DateTime, Utc};
use mst_core::ids::{ImageId, TenantId};
use sqlx::PgExecutor;

use crate::{Result, conflict_on_unique};

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Image {
    pub id: ImageId,
    pub tenant_id: TenantId,
    pub name: String,
    pub source_url: String,
    pub format: String,
    pub shared: bool,
    pub created_at: DateTime<Utc>,
}

macro_rules! image_columns {
    () => {
        "i.id, i.tenant_id, i.name, i.source_url, i.format, i.shared, i.created_at"
    };
}

pub struct NewImage<'a> {
    pub tenant_id: TenantId,
    pub name: &'a str,
    pub source_url: &'a str,
    pub format: &'a str,
    pub shared: bool,
}

pub async fn create(db: impl PgExecutor<'_>, new: NewImage<'_>) -> Result<Image> {
    sqlx::query_as(concat!(
        "INSERT INTO images AS i (id, tenant_id, name, source_url, format, shared)
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING ",
        image_columns!()
    ))
    .bind(ImageId::new())
    .bind(new.tenant_id)
    .bind(new.name)
    .bind(new.source_url)
    .bind(new.format)
    .bind(new.shared)
    .fetch_one(db)
    .await
    .map_err(|e| conflict_on_unique(e, "image"))
}

pub async fn get(db: impl PgExecutor<'_>, id: ImageId) -> Result<Image> {
    sqlx::query_as(concat!(
        "SELECT ",
        image_columns!(),
        " FROM images i WHERE i.id = $1"
    ))
    .bind(id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| {
        mst_core::Error::NotFound {
            kind: "image",
            id: id.to_string(),
        }
        .into()
    })
}

/// Images a tenant may use: its own, plus those its ancestors share.
pub async fn list_usable(db: impl PgExecutor<'_>, tenant_id: TenantId) -> Result<Vec<Image>> {
    Ok(sqlx::query_as(concat!(
        "SELECT ",
        image_columns!(),
        " FROM images i JOIN tenants t ON t.id = i.tenant_id
         WHERE i.tenant_id = $1
            OR (i.shared AND t.path @> (SELECT path FROM tenants WHERE id = $1))
         ORDER BY i.name"
    ))
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// Whether `tenant_id` may use the image (see [`list_usable`]).
pub async fn is_usable_by(
    db: impl PgExecutor<'_>,
    id: ImageId,
    tenant_id: TenantId,
) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM images i JOIN tenants t ON t.id = i.tenant_id
             WHERE i.id = $1 AND (i.tenant_id = $2
                OR (i.shared AND t.path @> (SELECT path FROM tenants WHERE id = $2))))",
    )
    .bind(id)
    .bind(tenant_id)
    .fetch_one(db)
    .await?)
}

/// Deletes an image no volume is built on.
pub async fn delete(db: &mut sqlx::PgConnection, id: ImageId) -> Result<()> {
    let users: i64 = sqlx::query_scalar("SELECT count(*) FROM volumes WHERE image_id = $1")
        .bind(id)
        .fetch_one(&mut *db)
        .await?;
    if users > 0 {
        let which = if users == 1 {
            "a VM disk is".to_owned()
        } else {
            format!("{users} VM disks are")
        };
        return Err(mst_core::Error::Conflict(format!(
            "{which} built on this image; delete those VMs first"
        ))
        .into());
    }
    sqlx::query("DELETE FROM images WHERE id = $1")
        .bind(id)
        .execute(&mut *db)
        .await
        .map_err(|e| -> crate::Error {
            match &e {
                // A volume created meanwhile (RESTRICT: 23001).
                sqlx::Error::Database(d)
                    if d.is_foreign_key_violation() || d.code().as_deref() == Some("23001") =>
                {
                    mst_core::Error::Conflict("VM disks are built on this image".into()).into()
                }
                _ => e.into(),
            }
        })?;
    Ok(())
}

/// Every image's ID (for agents to tell which cached copies to keep).
pub async fn all_ids(db: impl PgExecutor<'_>) -> Result<Vec<ImageId>> {
    Ok(sqlx::query_scalar("SELECT id FROM images")
        .fetch_all(db)
        .await?)
}
