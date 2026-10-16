//! Tenant hierarchy primitives.
//!
//! Tenants form a tree rooted at the provider tenant. Each tenant's position is
//! materialised as a Postgres `ltree` path whose labels are the tenant IDs
//! (hex, without dashes). Using IDs rather than names keeps paths stable when a
//! tenant is renamed.

use std::fmt;

use crate::ids::TenantId;

/// The `ltree` label for a tenant.
pub fn path_label(id: TenantId) -> String {
    id.0.simple().to_string()
}

/// A tenant's materialised path, from the root down to the tenant itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TenantPath(Vec<TenantId>);

impl TenantPath {
    pub fn root(id: TenantId) -> Self {
        Self(vec![id])
    }

    pub fn child(&self, id: TenantId) -> Self {
        let mut v = self.0.clone();
        v.push(id);
        Self(v)
    }

    /// Parses an `ltree` text representation produced by [`fmt::Display`].
    pub fn parse(s: &str) -> Option<Self> {
        s.split('.')
            .map(|label| uuid::Uuid::try_parse(label).ok().map(TenantId))
            .collect::<Option<Vec<_>>>()
            .filter(|v| !v.is_empty())
            .map(Self)
    }

    pub fn tenant(&self) -> TenantId {
        *self.0.last().expect("tenant path is never empty")
    }

    pub fn depth(&self) -> usize {
        self.0.len()
    }

    /// Ancestors including the tenant itself, root first.
    pub fn lineage(&self) -> &[TenantId] {
        &self.0
    }

    /// True if `self` is `other` or one of its ancestors.
    pub fn is_ancestor_or_self_of(&self, other: &TenantPath) -> bool {
        other.0.starts_with(&self.0)
    }
}

impl fmt::Display for TenantPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, id) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(".")?;
            }
            f.write_str(&path_label(*id))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_roundtrips_and_answers_ancestry() {
        let root = TenantPath::root(TenantId::new());
        let a = root.child(TenantId::new());
        let a1 = a.child(TenantId::new());
        let b = root.child(TenantId::new());

        assert_eq!(TenantPath::parse(&a1.to_string()).unwrap(), a1);
        assert_eq!(a1.depth(), 3);
        assert!(root.is_ancestor_or_self_of(&a1));
        assert!(a.is_ancestor_or_self_of(&a1));
        assert!(a1.is_ancestor_or_self_of(&a1));
        assert!(!a1.is_ancestor_or_self_of(&a));
        assert!(!b.is_ancestor_or_self_of(&a1));
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(TenantPath::parse("").is_none());
        assert!(TenantPath::parse("not-a-uuid").is_none());
    }
}
