//! hypermon's permission model.
//!
//! A user holds roles at tenants. A role bound at a tenant applies to it and to
//! every tenant below it. Provider resources (hosts, clusters, storage pools)
//! belong to the root tenant, so only roles bound at the root reach them.
//!
//! Some permissions concern a tenant's relationship with its parent and are
//! checked on the parent: a tenant cannot raise its own quota, grant itself a
//! cluster, or delete itself. [`Perm::checked_on_parent`] lists them.

use std::fmt;
use std::str::FromStr;

use hm_core::tenant::TenantPath;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Perm {
    /// See the tenant and its sub-tenants.
    TenantRead,
    /// Create, rename and delete sub-tenants.
    TenantWrite,
    /// See users and role bindings.
    UserRead,
    /// Create and change users, and bind roles.
    UserWrite,
    /// Set a tenant's quotas. Checked on the parent.
    QuotaWrite,
    /// Grant clusters and storage pools to a tenant. Checked on the parent.
    GrantWrite,
    VmRead,
    VmWrite,
    VmConsole,
    ImageRead,
    ImageWrite,
    /// See VPCs, networks, security groups and floating IPs.
    NetworkRead,
    /// Change them.
    NetworkWrite,
    /// Set guardrail policies for the tenant's subtree.
    PolicyWrite,
    TaskRead,
    AuditRead,
    /// Hosts, clusters and storage pools. Checked on the root tenant.
    InfraRead,
    InfraWrite,
}

impl Perm {
    pub const ALL: [Perm; 18] = [
        Perm::TenantRead,
        Perm::TenantWrite,
        Perm::UserRead,
        Perm::UserWrite,
        Perm::QuotaWrite,
        Perm::GrantWrite,
        Perm::VmRead,
        Perm::VmWrite,
        Perm::VmConsole,
        Perm::ImageRead,
        Perm::ImageWrite,
        Perm::NetworkRead,
        Perm::NetworkWrite,
        Perm::PolicyWrite,
        Perm::TaskRead,
        Perm::AuditRead,
        Perm::InfraRead,
        Perm::InfraWrite,
    ];

    /// Permissions governing a tenant from above.
    pub fn checked_on_parent(self) -> bool {
        matches!(self, Perm::QuotaWrite | Perm::GrantWrite)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Everything, including tenants, users and (at the root) infrastructure.
    Admin,
    /// Day-to-day work with VMs and images; read access to the rest.
    Operator,
    /// Read-only.
    Viewer,
}

impl Role {
    pub const ALL: [Role; 3] = [Role::Admin, Role::Operator, Role::Viewer];

    pub fn allows(self, perm: Perm) -> bool {
        use Perm::*;
        match self {
            Role::Admin => true,
            Role::Operator => matches!(
                perm,
                TenantRead
                    | UserRead
                    | VmRead
                    | VmWrite
                    | VmConsole
                    | ImageRead
                    | ImageWrite
                    | NetworkRead
                    | NetworkWrite
                    | TaskRead
                    | InfraRead
            ),
            Role::Viewer => matches!(
                perm,
                TenantRead | UserRead | VmRead | ImageRead | NetworkRead | TaskRead | InfraRead
            ),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Operator => "operator",
            Role::Viewer => "viewer",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Role {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        Role::ALL
            .into_iter()
            .find(|r| r.as_str() == s)
            .ok_or_else(|| format!("unknown role {s:?}"))
    }
}

/// A role held at a tenant (identified by its path).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    pub tenant: TenantPath,
    pub role: Role,
}

/// Whether `bindings` allow `perm` on the tenant at `target`.
pub fn allows(bindings: &[Binding], perm: Perm, target: &TenantPath) -> bool {
    bindings
        .iter()
        .any(|b| b.role.allows(perm) && b.tenant.is_ancestor_or_self_of(target))
}

/// The tenants whose subtrees `bindings` grant `perm` on, outermost only (a
/// binding below another one adds nothing).
pub fn scopes(bindings: &[Binding], perm: Perm) -> Vec<&TenantPath> {
    let mut paths: Vec<&TenantPath> = bindings
        .iter()
        .filter(|b| b.role.allows(perm))
        .map(|b| &b.tenant)
        .collect();
    paths.sort_by_key(|p| p.depth());
    let mut out: Vec<&TenantPath> = Vec::new();
    for p in paths {
        if !out.iter().any(|o| o.is_ancestor_or_self_of(p)) {
            out.push(p);
        }
    }
    out
}

/// What two sets of bindings both allow, as bindings: where a binding of
/// each covers a common subtree, the deeper tenant with the weaker role.
/// Roles are nested (admin ⊇ operator ⊇ viewer), so this is exact:
/// `allows(intersect(a, b), p, t) == allows(a, p, t) && allows(b, p, t)`.
pub fn intersect(a: &[Binding], b: &[Binding]) -> Vec<Binding> {
    let mut out = vec![];
    for x in a {
        for y in b {
            let tenant = if x.tenant.is_ancestor_or_self_of(&y.tenant) {
                &y.tenant
            } else if y.tenant.is_ancestor_or_self_of(&x.tenant) {
                &x.tenant
            } else {
                continue;
            };
            // Role's order runs from most to least powerful.
            out.push(Binding {
                tenant: tenant.clone(),
                role: x.role.max(y.role),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use hm_core::ids::TenantId;

    struct Tree {
        root: TenantPath,
        acme: TenantPath,
        dev: TenantPath,
        globex: TenantPath,
    }

    fn tree() -> Tree {
        let root = TenantPath::root(TenantId::new());
        let acme = root.child(TenantId::new());
        let dev = acme.child(TenantId::new());
        let globex = root.child(TenantId::new());
        Tree {
            root,
            acme,
            dev,
            globex,
        }
    }

    #[test]
    fn roles_are_inherited_down_but_not_up_or_sideways() {
        let t = tree();
        let b = [Binding {
            tenant: t.acme.clone(),
            role: Role::Operator,
        }];
        assert!(allows(&b, Perm::VmWrite, &t.acme));
        assert!(allows(&b, Perm::VmWrite, &t.dev));
        assert!(!allows(&b, Perm::VmRead, &t.root));
        assert!(!allows(&b, Perm::VmRead, &t.globex));
    }

    #[test]
    fn roles_differ_in_what_they_allow() {
        let t = tree();
        let viewer = [Binding {
            tenant: t.acme.clone(),
            role: Role::Viewer,
        }];
        let operator = [Binding {
            tenant: t.acme.clone(),
            role: Role::Operator,
        }];
        let admin = [Binding {
            tenant: t.acme.clone(),
            role: Role::Admin,
        }];
        assert!(allows(&viewer, Perm::VmRead, &t.dev) && !allows(&viewer, Perm::VmWrite, &t.dev));
        assert!(
            allows(&operator, Perm::VmConsole, &t.dev)
                && !allows(&operator, Perm::UserWrite, &t.dev)
        );
        assert!(
            allows(&admin, Perm::TenantWrite, &t.dev) && allows(&admin, Perm::AuditRead, &t.dev)
        );
    }

    #[test]
    fn infrastructure_needs_a_binding_at_the_root() {
        let t = tree();
        let tenant_admin = [Binding {
            tenant: t.acme.clone(),
            role: Role::Admin,
        }];
        let provider_admin = [Binding {
            tenant: t.root.clone(),
            role: Role::Admin,
        }];
        assert!(!allows(&tenant_admin, Perm::InfraWrite, &t.root));
        assert!(allows(&provider_admin, Perm::InfraWrite, &t.root));
    }

    #[test]
    fn scopes_keep_only_outermost_tenants() {
        let t = tree();
        let b = [
            Binding {
                tenant: t.dev.clone(),
                role: Role::Viewer,
            },
            Binding {
                tenant: t.acme.clone(),
                role: Role::Operator,
            },
            Binding {
                tenant: t.globex.clone(),
                role: Role::Viewer,
            },
        ];
        let s = scopes(&b, Perm::VmRead);
        assert_eq!(s.len(), 2);
        assert!(s.contains(&&t.acme) && s.contains(&&t.globex));
        assert_eq!(scopes(&b, Perm::VmWrite), vec![&t.acme]);
    }

    #[test]
    fn role_names_roundtrip() {
        for r in Role::ALL {
            assert_eq!(r.as_str().parse::<Role>().unwrap(), r);
        }
        assert!("root".parse::<Role>().is_err());
    }

    #[test]
    fn intersections_allow_what_both_allow() {
        let t = tree();
        let all = [
            t.root.clone(),
            t.acme.clone(),
            t.dev.clone(),
            t.globex.clone(),
        ];
        let b = |tenant: &TenantPath, role| Binding {
            tenant: tenant.clone(),
            role,
        };
        let user = [b(&t.root, Role::Operator), b(&t.dev, Role::Admin)];
        let token = [b(&t.acme, Role::Admin), b(&t.globex, Role::Viewer)];
        let both = intersect(&user, &token);
        let perms = [
            Perm::TenantRead,
            Perm::UserWrite,
            Perm::VmWrite,
            Perm::VmRead,
            Perm::InfraRead,
            Perm::InfraWrite,
        ];
        for target in &all {
            for perm in perms {
                assert_eq!(
                    allows(&both, perm, target),
                    allows(&user, perm, target) && allows(&token, perm, target),
                    "{perm:?} at {target:?}"
                );
            }
        }
        // Disjoint subtrees leave nothing.
        assert!(intersect(&[b(&t.acme, Role::Admin)], &[b(&t.globex, Role::Admin)]).is_empty());
    }
}
