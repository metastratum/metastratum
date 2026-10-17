import { defineStore } from 'pinia'
import { api, ok, type Tenant } from '../api/client'
import { useSession } from './session'

export type Role = 'admin' | 'operator' | 'viewer'

/** What each built-in role may do; mirrors mst-authz. */
const ROLE_PERMS: Record<Role, ReadonlySet<string>> = {
  admin: new Set(['*']),
  operator: new Set([
    'tenant.read',
    'user.read',
    'vm.read',
    'vm.write',
    'vm.console',
    'image.read',
    'image.write',
    'network.read',
    'network.write',
    'task.read',
    'infra.read',
  ]),
  viewer: new Set(['tenant.read', 'user.read', 'vm.read', 'image.read', 'network.read', 'task.read', 'infra.read']),
}
const RANK: Record<Role, number> = { admin: 3, operator: 2, viewer: 1 }

const KEY = 'metastratum.scope'

function remembered(): string | null {
  try {
    return localStorage.getItem(KEY)
  } catch {
    return null
  }
}

/**
 * The tenants the user can see, and the one they are working in. Views list
 * that tenant and everything below it; new resources are created there.
 */
export const useScope = defineStore('scope', {
  state: () => ({ tenants: [] as Tenant[], current: '' }),
  getters: {
    byId: (s) => new Map(s.tenants.map((t) => [t.id, t])),
    tenant(): Tenant | undefined {
      return this.byId.get(this.current)
    },
    /**
     * The installation is the provider alone, as far as this user can see:
     * tenancy (pickers, quotas, guardrails, tenant columns) stays out of the
     * way until there is a second tenant.
     */
    solo: (s) => s.tenants.length === 1 && !s.tenants[0]!.parent_id,
  },
  actions: {
    async load() {
      this.tenants = sortTree(await ok(api.GET('/api/v1/tenants')))
      const me = useSession().me
      const want = this.current || remembered()
      this.current = this.byId.has(want ?? '') ? want! : this.home(me?.tenant_id)
    },
    /** The user's own tenant if they can see it, else the top of what they can. */
    home(own?: string): string {
      return this.tenants.find((t) => t.id === own)?.id ?? this.tenants[0]?.id ?? ''
    },
    select(id: string) {
      this.current = id
      try {
        localStorage.setItem(KEY, id)
      } catch {
        // Not remembered; fine.
      }
    },
    /** The tenant and its ancestors that the user can see, root first. */
    lineage(id: string): Tenant[] {
      const out: Tenant[] = []
      for (let t = this.byId.get(id); t; t = t.parent_id ? this.byId.get(t.parent_id) : undefined) out.unshift(t)
      return out
    },
    /** The strongest role the user holds at a tenant, through inheritance. */
    roleAt(id: string): Role | null {
      const t = this.byId.get(id)
      const me = useSession().me
      if (!t || !me) return null
      let best: Role | null = null
      for (const b of me.roles) {
        const covers = b.tenant === '' || t.slug_path === b.tenant || t.slug_path.startsWith(`${b.tenant}/`)
        const role = b.role as Role
        if (covers && (!best || RANK[role] > RANK[best])) best = role
      }
      return best
    },
    /** Whether the user may do `perm` at a tenant (by default the current one). */
    can(perm: string, id?: string): boolean {
      const role = this.roleAt(id ?? this.current)
      return !!role && (ROLE_PERMS[role].has('*') || ROLE_PERMS[role].has(perm))
    },
  },
})

/** Depth-first order, siblings by name, so a flat list reads as a tree. */
export function sortTree(tenants: Tenant[]): Tenant[] {
  const kids = new Map<string | null, Tenant[]>()
  for (const t of tenants) {
    const p = t.parent_id && tenants.some((x) => x.id === t.parent_id) ? t.parent_id : null
    kids.set(p, [...(kids.get(p) ?? []), t])
  }
  const out: Tenant[] = []
  const walk = (p: string | null) => {
    for (const t of (kids.get(p) ?? []).sort((a, b) => a.name.localeCompare(b.name))) {
      out.push(t)
      walk(t.id)
    }
  }
  walk(null)
  return out
}
