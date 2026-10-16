use sqlx::PgExecutor;

use crate::Result;

/// Returns `(cert_pem, key_pem)` for a named key pair.
pub async fn get(db: impl PgExecutor<'_>, name: &str) -> Result<Option<(String, String)>> {
    Ok(
        sqlx::query_as("SELECT cert_pem, key_pem FROM pki WHERE name = $1")
            .bind(name)
            .fetch_optional(db)
            .await?,
    )
}

/// Stores a key pair unless one with this name exists. Returns whether this
/// call stored it; concurrent replicas racing to create the CA converge on
/// whichever insert won.
pub async fn insert_if_absent(
    db: impl PgExecutor<'_>,
    name: &str,
    cert_pem: &str,
    key_pem: &str,
) -> Result<bool> {
    let res = sqlx::query(
        "INSERT INTO pki (name, cert_pem, key_pem) VALUES ($1, $2, $3) ON CONFLICT (name) DO NOTHING",
    )
    .bind(name)
    .bind(cert_pem)
    .bind(key_pem)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Replaces a key pair's key (e.g. with its sealed form).
pub async fn set_key(db: impl PgExecutor<'_>, name: &str, key_pem: &str) -> Result<()> {
    sqlx::query("UPDATE pki SET key_pem = $2 WHERE name = $1")
        .bind(name)
        .bind(key_pem)
        .execute(db)
        .await?;
    Ok(())
}
