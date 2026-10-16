//! Hierarchical quotas.
//!
//! A tenant's quota limits what its whole subtree may allocate. Allocation
//! checks every limit on the way to the root. Usage is computed from the VMs
//! and volumes themselves rather than kept in counters, so it cannot drift.

use std::collections::BTreeMap;

use hm_core::ids::TenantId;
use sqlx::{PgConnection, PgExecutor};

use crate::Result;

pub const RESOURCES: [&str; 4] = ["vms", "vcpus", "memory_mib", "disk_gib"];
const GIB: i64 = 1 << 30;

/// Amounts per resource (`vms`, `vcpus`, `memory_mib`, `disk_gib`).
pub type Amounts = BTreeMap<String, i64>;

/// What a tenant's subtree currently uses.
pub async fn usage(db: impl PgExecutor<'_>, tenant_id: TenantId) -> Result<Amounts> {
    let (vms, vcpus, memory, disk): (i64, i64, i64, i64) = sqlx::query_as(
        "WITH sub AS (SELECT id FROM tenants WHERE path <@ (SELECT path FROM tenants WHERE id = $1))
         SELECT
           (SELECT count(*) FROM vms WHERE tenant_id IN (SELECT id FROM sub)),
           (SELECT coalesce(sum((spec->>'vcpus')::bigint), 0)::bigint FROM vms WHERE tenant_id IN (SELECT id FROM sub)),
           (SELECT coalesce(sum((spec->>'memory_mib')::bigint), 0)::bigint FROM vms WHERE tenant_id IN (SELECT id FROM sub)),
           (SELECT coalesce(sum(size_bytes), 0)::bigint FROM volumes WHERE tenant_id IN (SELECT id FROM sub))",
    )
    .bind(tenant_id)
    .fetch_one(db)
    .await?;
    Ok([
        ("vms", vms),
        ("vcpus", vcpus),
        ("memory_mib", memory),
        ("disk_gib", (disk + GIB - 1) / GIB),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect())
}

/// A tenant's own limits.
pub async fn limits(db: impl PgExecutor<'_>, tenant_id: TenantId) -> Result<Amounts> {
    let rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT resource, max FROM quotas WHERE tenant_id = $1")
            .bind(tenant_id)
            .fetch_all(db)
            .await?;
    Ok(rows.into_iter().collect())
}

/// The tightest limit per resource among a tenant's strict ancestors, with the
/// ancestor's name.
async fn inherited_limits(
    db: &mut PgConnection,
    tenant_id: TenantId,
) -> Result<BTreeMap<String, (i64, String)>> {
    let rows: Vec<(String, i64, String)> = sqlx::query_as(
        "SELECT q.resource, q.max, t.name FROM quotas q JOIN tenants t ON t.id = q.tenant_id
         WHERE t.path @> (SELECT path FROM tenants WHERE id = $1) AND t.id <> $1
         ORDER BY q.max",
    )
    .bind(tenant_id)
    .fetch_all(&mut *db)
    .await?;
    let mut out = BTreeMap::new();
    for (r, max, name) in rows {
        out.entry(r).or_insert((max, name));
    }
    Ok(out)
}

/// Sets (or with `None` clears) one of a tenant's limits. A limit may not
/// exceed what an ancestor allows, nor fall below a descendant's own limit.
pub async fn set_limit(
    db: &mut PgConnection,
    tenant_id: TenantId,
    resource: &str,
    max: Option<i64>,
) -> Result<()> {
    if !RESOURCES.contains(&resource) {
        return Err(
            hm_core::Error::InvalidArgument(format!("unknown quota resource {resource}")).into(),
        );
    }
    let Some(max) = max else {
        sqlx::query("DELETE FROM quotas WHERE tenant_id = $1 AND resource = $2")
            .bind(tenant_id)
            .bind(resource)
            .execute(&mut *db)
            .await?;
        return Ok(());
    };
    if max < 0 {
        return Err(
            hm_core::Error::InvalidArgument("quota limits cannot be negative".into()).into(),
        );
    }
    if let Some((parent_max, name)) = inherited_limits(db, tenant_id).await?.remove(resource)
        && max > parent_max
    {
        return Err(hm_core::Error::InvalidArgument(format!(
            "{resource} limit {max} exceeds {name}'s limit of {parent_max}"
        ))
        .into());
    }
    let above: Option<(String, i64)> = sqlx::query_as(
        "SELECT t.name, q.max FROM quotas q JOIN tenants t ON t.id = q.tenant_id
         WHERE t.path <@ (SELECT path FROM tenants WHERE id = $1) AND t.id <> $1
           AND q.resource = $2 AND q.max > $3
         ORDER BY q.max DESC LIMIT 1",
    )
    .bind(tenant_id)
    .bind(resource)
    .bind(max)
    .fetch_optional(&mut *db)
    .await?;
    if let Some((name, child_max)) = above {
        return Err(hm_core::Error::InvalidArgument(format!(
            "sub-tenant {name} has a {resource} limit of {child_max}; lower it first"
        ))
        .into());
    }
    sqlx::query(
        "INSERT INTO quotas (tenant_id, resource, max) VALUES ($1, $2, $3)
         ON CONFLICT (tenant_id, resource) DO UPDATE SET max = EXCLUDED.max",
    )
    .bind(tenant_id)
    .bind(resource)
    .bind(max)
    .execute(&mut *db)
    .await?;
    Ok(())
}

/// Checks that allocating `delta` in `tenant_id` stays within every quota on
/// the way to the root. Call inside the transaction that allocates: it locks
/// the tenant's lineage (root first, so concurrent allocations cannot
/// deadlock) and holds the locks until commit, so two allocations cannot both
/// squeeze into the same remaining space.
pub async fn reserve(db: &mut PgConnection, tenant_id: TenantId, delta: &Amounts) -> Result<()> {
    let lineage: Vec<(TenantId, String)> = sqlx::query_as(
        "SELECT id, name FROM tenants WHERE path @> (SELECT path FROM tenants WHERE id = $1)
         ORDER BY nlevel(path) FOR UPDATE",
    )
    .bind(tenant_id)
    .fetch_all(&mut *db)
    .await?;
    for (id, name) in lineage {
        let limits = limits(&mut *db, id).await?;
        if limits.is_empty() {
            continue;
        }
        let used = usage(&mut *db, id).await?;
        for (resource, max) in &limits {
            let want = used.get(resource).copied().unwrap_or(0)
                + delta.get(resource).copied().unwrap_or(0);
            if delta.get(resource).copied().unwrap_or(0) > 0 && want > *max {
                return Err(hm_core::Error::QuotaExceeded {
                    resource: format!("{resource} ({want} > {max})"),
                    tenant: name,
                }
                .into());
            }
        }
    }
    Ok(())
}
