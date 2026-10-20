// What every handler needs: who is signed in, what they may do where
// (roles inherited down the tenant tree, as mst-authz decides), which
// tenants a list covers, and the audit log.
import { emit } from './events'
import { forbidden, unauthorized } from './http'
import { db, find, type TenantRec, type UserRec } from './store'

export type Perm =
  | 'tenant.read'
  | 'tenant.write'
  | 'user.read'
  | 'user.write'
  | 'quota.write'
  | 'grant.write'
  | 'vm.read'
  | 'vm.write'
  | 'vm.console'
  | 'image.read'
  | 'image.write'
  | 'network.read'
  | 'network.write'
  | 'policy.write'
  | 'task.read'
  | 'audit.read'
  | 'infra.read'
  | 'infra.write'

const OPERATOR = new Set<Perm>([
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
])
const VIEWER = new Set<Perm>(['tenant.read', 'user.read', 'vm.read', 'image.read', 'network.read', 'task.read', 'infra.read'])

function allows(role: string, perm: Perm): boolean {
  return role === 'admin' || (role === 'operator' ? OPERATOR : role === 'viewer' ? VIEWER : new Set()).has(perm)
}

/** The signed-in user, or 401. */
export function me(): UserRec {
  const s = db.session
  const u = s && db.users.find((x) => x.id === s.user_id && !x.disabled)
  if (!u) throw unauthorized()
  return u
}

/** The signed-in user's tenant (where lists and new things default to). */
export function homeTenant(): string {
  me()
  return db.session!.tenant_id
}

/** The tenant and its ancestors, root first. */
export function lineage(id: string): TenantRec[] {
  const out: TenantRec[] = []
  for (let t = db.tenants.find((x) => x.id === id); t; t = db.tenants.find((x) => x.id === t!.parent_id)) out.unshift(t)
  return out
}

/** The tenant's ID and its descendants'. */
export function subtree(id: string): Set<string> {
  const out = new Set([id])
  let grew = true
  while (grew) {
    grew = false
    for (const t of db.tenants) {
      if (t.parent_id && out.has(t.parent_id) && !out.has(t.id)) {
        out.add(t.id)
        grew = true
      }
    }
  }
  return out
}

/** Whether the signed-in user may do `perm` at a tenant. */
export function may(perm: Perm, tenantId: string): boolean {
  const u = me()
  const above = new Set(lineage(tenantId).map((t) => t.id))
  return db.bindings.some((b) => b.user_id === u.id && above.has(b.tenant_id) && allows(b.role, perm))
}

/** Refuses (403) unless the signed-in user may do `perm` at the tenant. */
export function authorize(perm: Perm, tenantId: string) {
  if (!may(perm, tenantId)) throw forbidden(`you may not ${perm.replace('.', ' ')} here`)
}

/** Refuses unless the user may administer the infrastructure (the root's). */
export function authorizeInfra(perm: 'infra.read' | 'infra.write' = 'infra.write') {
  authorize(perm, root().id)
}

export const root = () => db.tenants.find((t) => !t.parent_id)!

/**
 * The tenants a list covers: `tenant_id` (the user's own by default), and
 * with `descendants` those below it too. Checks the user may read there.
 */
export function listScope(q: { tenant_id?: string | null; descendants?: boolean }, perm: Perm): Set<string> {
  const tenant = q.tenant_id ?? homeTenant()
  find(db.tenants, tenant, 'tenant')
  authorize(perm, tenant)
  return q.descendants ? subtree(tenant) : new Set([tenant])
}

export const tenantName = (id: string) => db.tenants.find((t) => t.id === id)?.name ?? ''
export const hostName = (id: string | null | undefined) => (id ? (db.hosts.find((h) => h.id === id)?.name ?? null) : null)

let auditSeq = 0
/** Records a change in the audit log, as every mutating call does. */
export function audit(
  action: string,
  target: [kind: string, id: string] | null,
  details: Record<string, unknown> = {},
  tenantId?: string,
  outcome = 'success',
) {
  const u = db.session ? db.users.find((x) => x.id === db.session!.user_id) : undefined
  auditSeq = Math.max(auditSeq, ...db.audit.slice(0, 1).map((a) => a.id)) + 1
  db.audit.unshift({
    id: auditSeq,
    at: new Date().toISOString(),
    action,
    outcome,
    details,
    target_kind: target?.[0] ?? null,
    target_id: target?.[1] ?? null,
    tenant_id: tenantId ?? db.session?.tenant_id ?? null,
    user_id: u?.id ?? null,
    username: u?.username ?? null,
  })
  if (db.audit.length > 1000) db.audit.length = 1000
}

export { emit }
