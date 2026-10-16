use chrono::{DateTime, Utc};
use hm_core::ids::{TenantId, UserId};
use sqlx::PgExecutor;

use crate::{Result, conflict_on_unique};

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct User {
    pub id: UserId,
    pub tenant_id: TenantId,
    pub username: String,
    pub display_name: String,
    pub email: Option<String>,
    pub password_hash: Option<String>,
    pub disabled: bool,
    pub created_at: DateTime<Utc>,
}

macro_rules! user_columns {
    () => {
        "id, tenant_id, username, display_name, email, password_hash, disabled, created_at"
    };
}

pub struct NewUser<'a> {
    pub tenant_id: TenantId,
    pub username: &'a str,
    pub display_name: &'a str,
    pub email: Option<&'a str>,
    pub password_hash: Option<&'a str>,
}

pub async fn create(db: impl PgExecutor<'_>, new: NewUser<'_>) -> Result<User> {
    sqlx::query_as(concat!(
        "INSERT INTO users (id, tenant_id, username, display_name, email, password_hash)
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING ",
        user_columns!()
    ))
    .bind(UserId::new())
    .bind(new.tenant_id)
    .bind(new.username)
    .bind(new.display_name)
    .bind(new.email)
    .bind(new.password_hash)
    .fetch_one(db)
    .await
    .map_err(|e| conflict_on_unique(e, "user"))
}

pub async fn get(db: impl PgExecutor<'_>, id: UserId) -> Result<User> {
    sqlx::query_as(concat!(
        "SELECT ",
        user_columns!(),
        " FROM users WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| {
        hm_core::Error::NotFound {
            kind: "user",
            id: id.to_string(),
        }
        .into()
    })
}

pub async fn find_by_username(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    username: &str,
) -> Result<Option<User>> {
    Ok(sqlx::query_as(concat!(
        "SELECT ",
        user_columns!(),
        " FROM users WHERE tenant_id = $1 AND username = $2"
    ))
    .bind(tenant_id)
    .bind(username)
    .fetch_optional(db)
    .await?)
}
