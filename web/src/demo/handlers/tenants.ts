// Tenants and what belongs to running them: quotas, grants, sign-in
// (domains, identity providers, the second-factor policy), people (users,
// role bindings, their second factors and API tokens), and the audit log.
// As the controller's tenants.rs, users.rs, tokens.rs, two_factor.rs,
// oidc.rs and audit.rs answer.
import type { components } from '../../api/schema'
import { audit, authorize, authorizeInfra, emit, lineage, listScope, may, me, root, subtree, type Perm } from '../ctx'
import { badRequest, conflict, created, forbidden, HttpError, none, notFound, route } from '../http'
import { db, find, newId, now, remove, type BindingRec, type TenantRec, type UserRec } from '../store'

type S = components['schemas']
const GIB = 2 ** 30

// ------------------------------------------------------------- helpers

/** Like the controller's `authorize_resource`: 403 where the caller can
 *  see the tenant, 404 where they cannot. */
function authorizeResource(perm: Perm, tenantId: string, kind: string) {
  if (may(perm, tenantId)) return
  throw may('tenant.read', tenantId) ? forbidden() : notFound(kind)
}

function loadTenant(id: string, perm: Perm): TenantRec {
  const t = db.tenants.find((x) => x.id === id)
  if (!t) throw notFound('tenant')
  authorizeResource(perm, t.id, 'tenant')
  return t
}

function parentOf(t: TenantRec): string {
  if (!t.parent_id) throw badRequest('the root tenant has no parent to manage it from')
  return t.parent_id
}

function validSlug(s: string): boolean {
  return /^[a-z0-9-]{1,63}$/.test(s) && !s.startsWith('-') && !s.endsWith('-')
}

function validName(s: string, max = 128): string {
  const n = s.trim()
  if (!n || n.length > max) throw badRequest(`name must be 1-${max} characters`)
  return n
}

function validDomain(domain: string): string {
  const d = domain.trim().replace(/\.+$/, '').toLowerCase()
  const label = (l: string) => /^[a-z0-9-]{1,63}$/.test(l) && !l.startsWith('-') && !l.endsWith('-')
  if (d.length > 253 || !d.split('.').every(label)) {
    throw badRequest('a domain is a host name such as mst.example.com, without scheme, port or path')
  }
  return d
}

const ROLES = ['admin', 'operator', 'viewer'] as const
function validRole(r: string | null | undefined): string {
  if (!r || !(ROLES as readonly string[]).includes(r)) throw badRequest(`unknown role ${r}: admin, operator or viewer`)
  return r
}

/** The permissions a role holds (mirrors mst-authz). */
const ROLE_PERMS: Record<string, Perm[]> = {
  admin: [
    'tenant.read',
    'tenant.write',
    'user.read',
    'user.write',
    'quota.write',
    'grant.write',
    'vm.read',
    'vm.write',
    'vm.console',
    'image.read',
    'image.write',
    'network.read',
    'network.write',
    'policy.write',
    'task.read',
    'audit.read',
    'infra.read',
    'infra.write',
  ],
  operator: ['tenant.read', 'user.read', 'vm.read', 'vm.write', 'vm.console', 'image.read', 'image.write', 'network.read', 'network.write', 'task.read', 'infra.read'],
  viewer: ['tenant.read', 'user.read', 'vm.read', 'image.read', 'network.read', 'task.read', 'infra.read'],
}

const randomFrom = (alphabet: string, n: number) =>
  [...crypto.getRandomValues(new Uint8Array(n))].map((b) => alphabet[b % alphabet.length]).join('')
const hex = (bytes: number) => [...crypto.getRandomValues(new Uint8Array(bytes))].map((b) => b.toString(16).padStart(2, '0')).join('')

// -------------------------------------------------------------- tenants

/** Every tenant the caller can see, parents before children. */
route.get('/api/v1/tenants', () => {
  const u = me()
  const visible = new Set<string>()
  for (const b of db.bindings.filter((b) => b.user_id === u.id)) for (const id of subtree(b.tenant_id)) visible.add(id)
  return db.tenants.filter((t) => visible.has(t.id)).sort((a, b) => a.depth - b.depth || a.slug_path.localeCompare(b.slug_path))
})

route.get('/api/v1/tenants/{id}', ({ params }) => loadTenant(params.id, 'tenant.read'))

route.post('/api/v1/tenants', ({ body }) => {
  authorize('tenant.write', body.parent_id)
  if (!validSlug(body.slug)) {
    throw badRequest('slug must be 1-63 lowercase letters, digits or hyphens, not starting or ending with a hyphen')
  }
  const parent = find(db.tenants, body.parent_id, 'tenant')
  const name = validName(body.name)
  if (db.tenants.some((t) => t.parent_id === parent.id && t.slug === body.slug)) throw conflict('tenant already exists')
  const t: TenantRec = {
    id: newId(),
    parent_id: parent.id,
    slug: body.slug,
    name,
    slug_path: parent.slug_path ? `${parent.slug_path}/${body.slug}` : body.slug,
    depth: parent.depth + 1,
    domains: [],
    created_at: now(),
  }
  db.tenants.push(t)
  // Every tenant starts with a default security group for its VMs.
  const g = {
    id: newId(),
    tenant_id: t.id,
    name: 'default',
    description: 'Allows all outgoing traffic, and incoming traffic from other members',
    created_at: now(),
  }
  db.groups.push(g)
  db.rules.push(
    { id: newId(), group_id: g.id, direction: 'egress', protocol: 'any', port_min: null, port_max: null, remote_cidr: '0.0.0.0/0', remote_group_id: null, description: '' },
    { id: newId(), group_id: g.id, direction: 'ingress', protocol: 'any', port_min: null, port_max: null, remote_cidr: null, remote_group_id: g.id, description: '' },
  )
  audit('tenant.create', ['tenant', t.id], { slug: t.slug, name: t.name }, t.id)
  emit('tenant', t.id, 'insert')
  emit('security_group', g.id, 'insert')
  return created(t)
})

route.patch('/api/v1/tenants/{id}', ({ params, body }) => {
  const t = loadTenant(params.id, 'tenant.write')
  const name = validName(body.name)
  t.name = name
  audit('tenant.rename', ['tenant', t.id], { name }, t.id)
  emit('tenant', t.id)
  return t
})

route.delete('/api/v1/tenants/{id}', ({ params }) => {
  const t = loadTenant(params.id, 'tenant.read')
  authorize('tenant.write', parentOf(t))
  const contents: [string, number][] = [
    ['sub-tenants', db.tenants.filter((x) => x.parent_id === t.id).length],
    ['VMs', db.vms.filter((v) => v.tenant_id === t.id).length],
    ['images', db.images.filter((i) => i.tenant_id === t.id).length],
    ['backups', db.backups.filter((b) => b.tenant_id === t.id).length],
  ]
  const what = contents.filter(([, n]) => n > 0).map(([k, n]) => `${n} ${k}`)
  if (what.length) throw conflict(`${t.name} still has ${what.join(', ')}`)
  // Its users, role bindings, quotas, grants and network configuration go
  // with it.
  const groups = db.groups.filter((g) => g.tenant_id === t.id).map((g) => g.id)
  const users = db.users.filter((u) => u.tenant_id === t.id).map((u) => u.id)
  for (const lb of db.loadBalancers.filter((x) => x.tenant_id === t.id)) emit('load_balancer', lb.id, 'delete')
  for (const ip of db.floatingIps.filter((x) => x.tenant_id === t.id)) emit('external_ip', ip.id, 'delete')
  for (const n of db.networks.filter((x) => x.tenant_id === t.id)) emit('network', n.id, 'delete')
  for (const v of db.vpcs.filter((x) => x.tenant_id === t.id)) emit('vpc', v.id, 'delete')
  for (const g of groups) emit('security_group', g, 'delete')
  for (const p of db.policies.filter((x) => x.tenant_id === t.id)) emit('network_policy', p.id, 'delete')
  remove(db.loadBalancers, (x) => x.tenant_id === t.id)
  remove(db.floatingIps, (x) => x.tenant_id === t.id)
  remove(db.networks, (x) => x.tenant_id === t.id)
  remove(db.vpcs, (x) => x.tenant_id === t.id)
  remove(db.rules, (r) => groups.includes(r.group_id))
  remove(db.groups, (g) => g.tenant_id === t.id)
  remove(db.policies, (x) => x.tenant_id === t.id)
  remove(db.bindings, (b) => b.tenant_id === t.id || users.includes(b.user_id))
  remove(db.tokens, (x) => users.includes(x.user_id))
  remove(db.factors, (x) => users.includes(x.user_id))
  for (const u of users) delete db.backupCodes[u]
  remove(db.users, (u) => u.tenant_id === t.id)
  delete db.quotas[t.id]
  delete db.grants[t.id]
  delete db.identityProviders[t.id]
  remove(db.twoFactorRequired, (x) => x === t.id)
  remove(db.tenants, (x) => x.id === t.id)
  audit('tenant.delete', ['tenant', t.id], { slug: t.slug, name: t.name }, t.parent_id ?? undefined)
  emit('tenant', t.id, 'delete')
  return none()
})

// ------------------------------------------------------------- domains

route.post('/api/v1/tenants/{id}/domains', ({ params, body }) => {
  authorizeInfra('infra.write')
  const domain = validDomain(body.domain)
  const t = find(db.tenants, params.id, 'tenant')
  if (db.tenants.some((x) => x.domains.includes(domain))) throw conflict('a binding for that domain already exists')
  t.domains.push(domain)
  t.domains.sort()
  audit('tenant.domain.bind', ['tenant', t.id], { domain }, t.id)
  emit('tenant', t.id)
  return t
})

route.delete('/api/v1/tenants/{id}/domains/{domain}', ({ params }) => {
  authorizeInfra('infra.write')
  const domain = validDomain(params.domain)
  const t = db.tenants.find((x) => x.id === params.id)
  if (!t || !t.domains.includes(domain)) throw notFound('domain')
  t.domains = t.domains.filter((d) => d !== domain)
  audit('tenant.domain.unbind', ['tenant', t.id], { domain }, t.id)
  emit('tenant', t.id)
  return none()
})

/** Where the tenant's people sign in: at the nearest domain bound at or
 *  above it, with the path below it, or else here. */
function signIn(id: string): S['SignInView'] {
  const t = find(db.tenants, id, 'tenant')
  const line = lineage(t.id)
  let nearest: { domain: string; depth: number } | null = null
  for (const a of line) if (a.domains[0]) nearest = { domain: a.domains[0], depth: a.depth }
  const slugs = t.slug_path.split('/').filter(Boolean)
  if (nearest) {
    const base = `${location.protocol}//${nearest.domain}`
    const below = slugs.slice(Math.max(0, nearest.depth - 1)).join('/')
    return {
      page: below ? `${base}/login?tenant=${below}` : `${base}/login`,
      redirect_uri: `${base}/api/v1/auth/oidc/callback`,
      domain: nearest.domain,
    }
  }
  // The demo is static files routed in the fragment: its own sign-in page.
  const here = `${location.origin}${location.pathname}`
  const path = slugs.join('/')
  return {
    page: path ? `${here}#/login?tenant=${path}` : `${here}#/login`,
    redirect_uri: `${location.origin}/api/v1/auth/oidc/callback`,
    domain: null,
  }
}

route.get('/api/v1/tenants/{id}/sign-in', ({ params }) => {
  authorizeResource('tenant.read', params.id, 'tenant')
  return signIn(params.id)
})

// --------------------------------------------------------------- quotas

const RESOURCES = ['vms', 'vcpus', 'memory_mib', 'disk_gib', 'floating_ips', 'backup_gib', 'load_balancers', 'vpcs', 'snapshot_gib', 'image_gib']

/** What a tenant and its sub-tenants use now. */
export function quotaUsage(tenantId: string): Record<string, number> {
  const sub = subtree(tenantId)
  const vms = db.vms.filter((v) => sub.has(v.tenant_id))
  const ceilGib = (bytes: number) => Math.ceil(bytes / GIB)
  return {
    vms: vms.length,
    vcpus: vms.reduce((n, v) => n + Math.max(v.vcpus, v.max_vcpus ?? 0), 0),
    memory_mib: vms.reduce((n, v) => n + Math.max(v.memory_mib, v.max_memory_mib ?? 0), 0),
    disk_gib: db.disks.filter((d) => sub.has(d.tenant_id)).reduce((n, d) => n + d.size_gib, 0),
    floating_ips: db.floatingIps.filter((f) => sub.has(f.tenant_id)).length,
    backup_gib: ceilGib(
      db.backups
        .filter((b) => b.state !== 'failed' && sub.has(b.tenant_id))
        .reduce((n, b) => n + (b.state === 'running' ? b.size_bytes : (b.stored_bytes ?? b.size_bytes)), 0),
    ),
    load_balancers: db.loadBalancers.filter((l) => sub.has(l.tenant_id)).length,
    vpcs: db.vpcs.filter((v) => sub.has(v.tenant_id)).length,
    // Snapshots keep no size of their own in the demo.
    snapshot_gib: 0,
    image_gib: ceilGib(db.images.filter((i) => i.state === 'ready' && sub.has(i.tenant_id)).reduce((n, i) => n + (i.size_bytes ?? 0), 0)),
  }
}

/** The tightest limit each ancestor (not the tenant itself) imposes. */
function inheritedLimits(tenantId: string): Record<string, S['InheritedLimit']> {
  const out: Record<string, S['InheritedLimit']> = {}
  for (const t of lineage(tenantId).slice(0, -1).reverse()) {
    for (const [r, max] of Object.entries(db.quotas[t.id] ?? {})) {
      const e = out[r]
      if (!e || max < e.max) out[r] = { max, tenant_name: t.name }
    }
  }
  return out
}

function quotas(id: string): S['Quotas'] {
  return { limits: { ...(db.quotas[id] ?? {}) }, inherited: inheritedLimits(id), usage: quotaUsage(id) }
}

route.get('/api/v1/tenants/{id}/quotas', ({ params }) => {
  loadTenant(params.id, 'tenant.read')
  return quotas(params.id)
})

route.put('/api/v1/tenants/{id}/quotas', ({ params, body }) => {
  const t = loadTenant(params.id, 'tenant.read')
  authorize('quota.write', parentOf(t))
  const limits = { ...(db.quotas[t.id] ?? {}) }
  const inherited = inheritedLimits(t.id)
  for (const [resource, max] of Object.entries(body ?? {})) {
    if (!RESOURCES.includes(resource)) throw badRequest(`unknown quota resource ${resource}`)
    if (max === null || max === undefined) {
      delete limits[resource]
      continue
    }
    if (max < 0) throw badRequest('quota limits cannot be negative')
    const above = inherited[resource]
    if (above && max > above.max) throw badRequest(`${resource} limit ${max} exceeds ${above.tenant_name}'s limit of ${above.max}`)
    const child = db.tenants
      .filter((x) => x.id !== t.id && subtree(t.id).has(x.id))
      .map((x) => ({ name: x.name, max: db.quotas[x.id]?.[resource] }))
      .filter((x): x is { name: string; max: number } => x.max !== undefined && x.max > max)
      .sort((a, b) => b.max - a.max)[0]
    if (child) throw badRequest(`sub-tenant ${child.name} has a ${resource} limit of ${child.max}; lower it first`)
    limits[resource] = max
  }
  db.quotas[t.id] = limits
  audit('tenant.quotas', ['tenant', t.id], body as Record<string, unknown>, t.id)
  emit('tenant', t.id)
  return quotas(t.id)
})

// --------------------------------------------------------------- grants

type GrantKey = 'clusters' | 'storage_pools' | 'external_networks' | 'backup_repositories'
const KIND_OF: Record<GrantKey, string> = {
  clusters: 'cluster',
  storage_pools: 'storage_pool',
  external_networks: 'external_network',
  backup_repositories: 'backup_repository',
}

/** Whether a tenant may use something: the root everything, others what
 *  they were granted. */
export function mayUse(tenantId: string, key: GrantKey, id: string): boolean {
  const t = db.tenants.find((x) => x.id === tenantId)
  if (!t) return false
  return !t.parent_id || (db.grants[tenantId]?.[key] ?? []).includes(id)
}

function grants(id: string): S['Grants'] {
  const g = db.grants[id]
  return {
    clusters: [...(g?.clusters ?? [])],
    storage_pools: [...(g?.storage_pools ?? [])],
    external_networks: [...(g?.external_networks ?? [])],
    backup_repositories: [...(g?.backup_repositories ?? [])],
  }
}

route.get('/api/v1/tenants/{id}/grants', ({ params }) => {
  loadTenant(params.id, 'tenant.read')
  return grants(params.id)
})

route.put('/api/v1/tenants/{id}/grants', ({ params, body }) => {
  const t = loadTenant(params.id, 'tenant.read')
  const parent = parentOf(t)
  authorize('grant.write', parent)
  const want: Record<GrantKey, string[]> = {
    clusters: [...new Set(body.clusters ?? [])],
    storage_pools: [...new Set(body.storage_pools ?? [])],
    external_networks: [...new Set(body.external_networks ?? [])],
    backup_repositories: [...new Set(body.backup_repositories ?? [])],
  }
  for (const c of want.clusters) find(db.clusters, c, 'cluster')
  for (const p of want.storage_pools) {
    if (find(db.pools, p, 'storage pool').kind === 'local') {
      throw badRequest('local storage is available to every tenant; it needs no grant')
    }
  }
  for (const e of want.external_networks) find(db.externalNetworks, e, 'external network')
  for (const r of want.backup_repositories) find(db.repositories, r, 'backup repository')
  for (const key of Object.keys(want) as GrantKey[]) {
    for (const r of want[key]) {
      if (!mayUse(parent, key, r)) throw badRequest(`the parent tenant may not use ${KIND_OF[key]} ${r}`)
    }
  }
  // Revoked here is revoked from the whole subtree too.
  for (const id of subtree(t.id)) {
    const g = db.grants[id]
    if (!g || id === t.id) continue
    for (const key of Object.keys(want) as GrantKey[]) g[key] = (g[key] ?? []).filter((r) => want[key].includes(r))
  }
  db.grants[t.id] = want
  audit('tenant.grants', ['tenant', t.id], want, t.id)
  emit('tenant', t.id)
  return grants(t.id)
})

route.get('/api/v1/tenants/{id}/clusters', ({ params }) => {
  authorizeResource('vm.read', params.id, 'tenant')
  find(db.tenants, params.id, 'tenant')
  return db.clusters.filter((c) => mayUse(params.id, 'clusters', c.id)).map((c) => ({ id: c.id, name: c.name }))
})

route.get('/api/v1/tenants/{id}/storage-pools', ({ params }) => {
  authorizeResource('vm.read', params.id, 'tenant')
  find(db.tenants, params.id, 'tenant')
  return db.pools
    .filter((p) => p.kind === 'local' || mayUse(params.id, 'storage_pools', p.id))
    .map((p) => ({ id: p.id, name: p.name, kind: p.kind }))
})

route.get('/api/v1/tenants/{id}/external-networks', ({ params }) => {
  authorizeResource('network.read', params.id, 'tenant')
  find(db.tenants, params.id, 'tenant')
  return db.externalNetworks
    .filter((n) => mayUse(params.id, 'external_networks', n.id))
    .map((n) => ({ id: n.id, name: n.name, cidr: n.cidr }))
})

route.get('/api/v1/tenants/{id}/backup-repositories', ({ params }) => {
  authorizeResource('vm.read', params.id, 'tenant')
  find(db.tenants, params.id, 'tenant')
  return db.repositories.filter((r) => mayUse(params.id, 'backup_repositories', r.id)).map((r) => ({ id: r.id, name: r.name }))
})

// ------------------------------------------------- identity providers

type StoredProvider = S['ProviderView'] & { client_secret?: string }

function providerView(id: string): S['ProviderView'] {
  const p = db.identityProviders[id] as StoredProvider | undefined
  if (!p) throw notFound('identity provider')
  const { client_secret: _secret, ...view } = p
  return { ...view, role_mappings: (view.role_mappings ?? []).map((m) => ({ ...m })), redirect_uri: signIn(id).redirect_uri }
}

route.get('/api/v1/tenants/{id}/identity-provider', ({ params }) => {
  authorizeResource('user.read', params.id, 'tenant')
  return providerView(params.id)
})

route.put('/api/v1/tenants/{id}/identity-provider', ({ params, body }) => {
  authorizeResource('user.write', params.id, 'tenant')
  find(db.tenants, params.id, 'tenant')
  const existing = db.identityProviders[params.id] as StoredProvider | undefined
  const secret = (body as StoredProvider).client_secret?.trim()
  if (!secret && !existing) throw badRequest('a client secret is required')
  const name = body.name.trim()
  if (!name || name.length > 64) throw badRequest('name must be 1-64 characters')
  if (!/^https?:\/\//.test(body.issuer) || !body.client_id.trim()) {
    throw badRequest('an http(s) issuer URL and a client ID are required')
  }
  const stored: StoredProvider = {
    name,
    issuer: body.issuer,
    client_id: body.client_id.trim(),
    client_secret: '(set)',
    scopes: body.scopes?.trim() || 'openid profile email',
    username_claim: body.username_claim || 'preferred_username',
    groups_claim: body.groups_claim || 'groups',
    default_role: body.default_role ? validRole(body.default_role) : null,
    role_mappings: (body.role_mappings ?? []).map((m) => ({ group: m.group, role: validRole(m.role) })),
    redirect_uri: '',
  }
  db.identityProviders[params.id] = stored
  audit('tenant.identity_provider', ['tenant', params.id], { issuer: stored.issuer, client_id: stored.client_id }, params.id)
  emit('tenant', params.id)
  return providerView(params.id)
})

route.delete('/api/v1/tenants/{id}/identity-provider', ({ params }) => {
  authorizeResource('user.write', params.id, 'tenant')
  delete db.identityProviders[params.id]
  audit('tenant.identity_provider.delete', ['tenant', params.id], {}, params.id)
  emit('tenant', params.id)
  return none()
})

// ------------------------------------------------- two-factor policy

function policy(id: string): S['TwoFactorPolicy'] {
  const t = find(db.tenants, id, 'tenant')
  const above = lineage(t.id)
    .slice(0, -1)
    .reverse()
    .find((a) => db.twoFactorRequired.includes(a.id))
  return { required: db.twoFactorRequired.includes(t.id), required_by: above ? { id: above.id, name: above.name } : null }
}

route.get('/api/v1/tenants/{id}/two-factor-policy', ({ params }) => {
  authorize('user.read', params.id)
  return policy(params.id)
})

route.put('/api/v1/tenants/{id}/two-factor-policy', ({ params, body }) => {
  authorize('user.write', params.id)
  find(db.tenants, params.id, 'tenant')
  remove(db.twoFactorRequired, (x) => x === params.id)
  if (body.required) db.twoFactorRequired.push(params.id)
  audit('tenant.two_factor_policy', ['tenant', params.id], { required: body.required }, params.id)
  emit('tenant', params.id)
  return policy(params.id)
})

// ---------------------------------------------------------------- users

const hasFactor = (userId: string) => db.factors.some((f) => f.user_id === userId)

export function userView(u: UserRec): S['UserView'] {
  const { password: _pw, ...rest } = u
  return { ...rest, two_factor: hasFactor(u.id) }
}

function bindingView(b: BindingRec): S['BindingView'] {
  return {
    ...b,
    username: db.users.find((u) => u.id === b.user_id)?.username ?? '',
    tenant_name: db.tenants.find((t) => t.id === b.tenant_id)?.name ?? '',
  }
}

function checkPassword(pw: string) {
  if ([...pw].length < 10) throw badRequest('passwords need at least 10 characters')
}

/** Refuses changes that would leave the provider without an administrator. */
function keepARootAdmin(losing: string) {
  const r = root().id
  const isRootAdmin = db.bindings.some((b) => b.user_id === losing && b.tenant_id === r && b.role === 'admin')
  const count = db.bindings.filter((b) => b.tenant_id === r && b.role === 'admin').length
  if (isRootAdmin && count <= 1) throw conflict('this is the last provider administrator')
}

function confirmPassword(u: UserRec, password: string | null | undefined) {
  if (u.password !== password) throw new HttpError(403, 'wrong_password', 'that is not your password')
}

route.get('/api/v1/users', ({ query }) => {
  const tenants = listScope(query, 'user.read')
  return db.users.filter((u) => tenants.has(u.tenant_id)).map(userView)
})

route.post('/api/v1/users', ({ body }) => {
  authorize('user.write', body.tenant_id)
  checkPassword(body.password)
  const role = validRole(body.role ?? 'viewer')
  find(db.tenants, body.tenant_id, 'tenant')
  const username = body.username.trim()
  if (!/^[A-Za-z0-9._@-]{1,128}$/.test(username)) {
    throw badRequest('usernames are 1-128 letters, digits, dots, hyphens, underscores or @')
  }
  if (db.users.some((u) => u.tenant_id === body.tenant_id && u.username === username)) throw conflict('user already exists')
  const u: UserRec = {
    id: newId(),
    tenant_id: body.tenant_id,
    username,
    display_name: (body.display_name ?? '').trim(),
    email: body.email?.trim() || null,
    disabled: false,
    local: true,
    created_at: now(),
    password: body.password,
  }
  db.users.push(u)
  db.bindings.push({ id: newId(), user_id: u.id, tenant_id: body.tenant_id, role, source: 'manual', created_at: now() })
  audit('user.create', ['user', u.id], { username: u.username, role }, u.tenant_id)
  return created(userView(u))
})

route.patch('/api/v1/users/{id}', ({ params, body }) => {
  const u = find(db.users, params.id, 'user')
  const isSelf = u.id === me().id
  if (!isSelf || (body.disabled !== undefined && body.disabled !== null)) authorizeResource('user.write', u.tenant_id, 'user')
  if (body.disabled === true) {
    if (isSelf) throw badRequest('you cannot disable your own account')
    keepARootAdmin(u.id)
  }
  if (body.password) {
    checkPassword(body.password)
    if (isSelf) {
      if (!u.local) throw badRequest('this account signs in through an identity provider and has no password')
      if (!body.current_password) throw badRequest('give your current password too')
      confirmPassword(u, body.current_password)
    }
  }
  const emailGiven = 'email' in body
  if (body.display_name !== undefined && body.display_name !== null) u.display_name = body.display_name.trim()
  if (emailGiven) u.email = body.email?.trim() || null
  if (body.password) u.password = body.password
  if (body.disabled !== undefined && body.disabled !== null) u.disabled = body.disabled
  let revoked = 0
  if (body.revoke_tokens && body.password && !isSelf) {
    revoked = db.tokens.filter((t) => t.user_id === u.id).length
    remove(db.tokens, (t) => t.user_id === u.id)
  }
  const changed = (
    [
      ['display_name', body.display_name != null],
      ['email', emailGiven],
      ['password', !!body.password],
      ['disabled', body.disabled != null],
    ] as const
  )
    .filter(([, v]) => v)
    .map(([k]) => k)
  audit('user.update', ['user', u.id], { changed, disabled: body.disabled ?? null, tokens_revoked: revoked }, u.tenant_id)
  return userView(u)
})

route.delete('/api/v1/users/{id}', ({ params }) => {
  const u = find(db.users, params.id, 'user')
  authorizeResource('user.write', u.tenant_id, 'user')
  if (u.id === me().id) throw badRequest('you cannot delete your own account')
  keepARootAdmin(u.id)
  remove(db.bindings, (b) => b.user_id === u.id)
  remove(db.tokens, (t) => t.user_id === u.id)
  remove(db.factors, (f) => f.user_id === u.id)
  delete db.backupCodes[u.id]
  remove(db.users, (x) => x.id === u.id)
  audit('user.delete', ['user', u.id], { username: u.username }, u.tenant_id)
  return none()
})

// -------------------------------------------------------- role bindings

route.get('/api/v1/users/{id}/role-bindings', ({ params }) => {
  const u = find(db.users, params.id, 'user')
  if (u.id !== me().id) authorizeResource('user.read', u.tenant_id, 'user')
  return db.bindings.filter((b) => b.user_id === u.id).map(bindingView)
})

route.get('/api/v1/tenants/{id}/role-bindings', ({ params }) => {
  authorizeResource('user.read', params.id, 'tenant')
  return db.bindings.filter((b) => b.tenant_id === params.id).map(bindingView)
})

route.post('/api/v1/role-bindings', ({ body }) => {
  authorize('user.write', body.tenant_id)
  const role = validRole(body.role)
  const u = find(db.users, body.user_id, 'user')
  authorizeResource('user.read', u.tenant_id, 'user')
  const home = find(db.tenants, u.tenant_id, 'tenant')
  const at = find(db.tenants, body.tenant_id, 'tenant')
  if (!subtree(home.id).has(at.id)) {
    throw badRequest(`${u.username} belongs to ${home.name}: they can hold roles there and below, not at ${at.name}`)
  }
  if (db.bindings.some((b) => b.user_id === u.id && b.tenant_id === at.id && b.role === role)) {
    throw conflict('this role binding already exists')
  }
  const b: BindingRec = { id: newId(), user_id: u.id, tenant_id: at.id, role, source: 'manual', created_at: now() }
  db.bindings.push(b)
  audit('role.bind', ['user', u.id], { username: u.username, role }, at.id)
  return created(bindingView(b))
})

route.delete('/api/v1/role-bindings/{id}', ({ params }) => {
  const b = db.bindings.find((x) => x.id === params.id)
  if (!b) throw notFound('role binding')
  authorizeResource('user.write', b.tenant_id, 'role binding')
  const r = root().id
  if (b.tenant_id === r && b.role === 'admin' && db.bindings.filter((x) => x.tenant_id === r && x.role === 'admin').length <= 1) {
    throw conflict('this is the last provider administrator')
  }
  const view = bindingView(b)
  remove(db.bindings, (x) => x.id === b.id)
  audit('role.unbind', ['user', b.user_id], { username: view.username, role: b.role }, b.tenant_id)
  return none()
})

// ------------------------------------------------------- second factors

/** Adding a factor between its two steps: what the second step checks. */
const enrolling = new Map<string, { user: string; kind: string; secret?: string }>()

/** The caller themselves, with a password. */
function ownLocal(id: string): UserRec {
  const u = me()
  if (u.id !== id) throw new HttpError(403, 'forbidden', 'only the user can change their own second factors')
  if (!u.local) throw badRequest('this account signs in through an identity provider, which handles second factors')
  return u
}

function validFactorName(name: string): string {
  const n = name.trim()
  if (!n || [...n].length > 64) throw badRequest('give it a name of up to 64 characters')
  return n
}

function newBackupCodes(): string[] {
  return Array.from({ length: 10 }, () => {
    const c = randomFrom('23456789abcdefghjkmnpqrstuvwxyz', 12)
    return `${c.slice(0, 4)}-${c.slice(4, 8)}-${c.slice(8)}`
  })
}

/** Adds a factor; the first brings the backup codes, shown once. */
function addFactor(u: UserRec, kind: string, name: string, site: string | null = null): S['FactorAdded'] {
  const first = !hasFactor(u.id)
  const factor = { id: newId(), user_id: u.id, kind, name, site, created_at: now(), last_used_at: null }
  db.factors.push(factor)
  let codes: string[] | undefined
  if (first) {
    codes = newBackupCodes()
    db.backupCodes[u.id] = codes.length
  }
  audit('user.two_factor.add', ['user', u.id], { kind, name }, u.tenant_id)
  const { user_id: _u, ...view } = factor
  return { factor: view, backup_codes: codes }
}

route.get('/api/v1/users/{id}/two-factor', ({ params }) => {
  const u = find(db.users, params.id, 'user')
  if (u.id !== me().id) authorizeResource('user.read', u.tenant_id, 'user')
  return {
    factors: db.factors.filter((f) => f.user_id === u.id).map(({ user_id: _u, ...f }) => f),
    backup_codes_left: db.backupCodes[u.id] ?? 0,
    required: u.local && lineage(u.tenant_id).some((t) => db.twoFactorRequired.includes(t.id)),
    yubico_otp: true,
  }
})

route.post('/api/v1/users/{id}/two-factor/totp/start', ({ params, body }) => {
  const u = ownLocal(params.id)
  confirmPassword(u, body.password)
  const secret = randomFrom('ABCDEFGHIJKLMNOPQRSTUVWXYZ234567', 32)
  const token = hex(32)
  enrolling.set(token, { user: u.id, kind: 'totp', secret })
  const slugs = lineage(u.tenant_id)
    .slice(1)
    .map((t) => t.slug)
  const account = slugs.length ? `${u.username}@${slugs.join('/')}` : u.username
  const issuer = location.hostname || 'metastratum'
  const uri = `otpauth://totp/${encodeURIComponent(issuer)}:${encodeURIComponent(account)}?secret=${secret}&issuer=${encodeURIComponent(issuer)}&algorithm=SHA1&digits=6&period=30`
  return { token, secret, uri }
})

route.post('/api/v1/users/{id}/two-factor/totp', ({ params, body }) => {
  const u = ownLocal(params.id)
  const name = validFactorName(body.name)
  const e = enrolling.get(body.token)
  if (!e || e.user !== u.id || e.kind !== 'totp') throw badRequest('this took too long; start again')
  // Any six digits pass in the demo: there is no phone to check against.
  if (!/^\d{6}$/.test(body.code.replace(/\s/g, ''))) {
    throw badRequest("that code is not right; check the phone's clock, and try the next code")
  }
  enrolling.delete(body.token)
  return created(addFactor(u, 'totp', name))
})

route.post('/api/v1/users/{id}/two-factor/webauthn/start', ({ params }) => {
  ownLocal(params.id)
  throw badRequest('security keys need a real controller to check them; in the demo, add an authenticator app instead')
})

route.post('/api/v1/users/{id}/two-factor/webauthn', ({ params }) => {
  ownLocal(params.id)
  throw badRequest('security keys need a real controller to check them; in the demo, add an authenticator app instead')
})

route.post('/api/v1/users/{id}/two-factor/yubico-otp', ({ params, body }) => {
  const u = ownLocal(params.id)
  const name = validFactorName(body.name)
  confirmPassword(u, body.password)
  const otp = body.otp.trim()
  if (otp.length < 34 || otp.length > 48 || !/^[cbdefghijklnrtuv]+$/.test(otp)) {
    throw badRequest('that is not a YubiKey OTP; touch the key with the cursor in the field')
  }
  const publicId = otp.slice(0, otp.length - 32)
  if (db.factors.some((f) => f.user_id === u.id && f.kind === 'yubico_otp' && f.site === `yubikey:${publicId}`)) {
    throw conflict('that YubiKey is already registered')
  }
  const added = addFactor(u, 'yubico_otp', name)
  // Remembers which key it is (the view's `site` is for security keys).
  const rec = db.factors.find((f) => f.id === added.factor.id)!
  rec.site = `yubikey:${publicId}`
  added.factor.site = null
  return created(added)
})

route.post('/api/v1/users/{id}/two-factor/backup-codes', ({ params, body }) => {
  const u = ownLocal(params.id)
  confirmPassword(u, body.password)
  if (!hasFactor(u.id)) throw badRequest('add a second factor first')
  const codes = newBackupCodes()
  db.backupCodes[u.id] = codes.length
  audit('user.two_factor.backup_codes', ['user', u.id], {}, u.tenant_id)
  return { codes }
})

route.delete('/api/v1/users/{id}/two-factor/factors/{factor_id}', ({ params, body }) => {
  const u = find(db.users, params.id, 'user')
  if (u.id === me().id) {
    // Own factors need the password; the demo's request handling passes
    // no bodies with DELETE, so it is checked when it comes.
    const password = (body as S['RemoveFactor'] | undefined)?.password
    if (password !== undefined && password !== null) confirmPassword(u, password)
  } else {
    authorizeResource('user.write', u.tenant_id, 'user')
  }
  if (!db.factors.some((f) => f.id === params.factor_id && f.user_id === u.id)) throw notFound('second factor')
  remove(db.factors, (f) => f.id === params.factor_id)
  if (!hasFactor(u.id)) delete db.backupCodes[u.id]
  audit('user.two_factor.remove', ['user', u.id], { factor: params.factor_id }, u.tenant_id)
  return none()
})

route.delete('/api/v1/users/{id}/two-factor', ({ params, query }) => {
  const u = find(db.users, params.id, 'user')
  if (u.id === me().id) throw badRequest('remove your own second factors one by one')
  authorizeResource('user.write', u.tenant_id, 'user')
  const n = db.factors.filter((f) => f.user_id === u.id).length
  remove(db.factors, (f) => f.user_id === u.id)
  delete db.backupCodes[u.id]
  let revoked = 0
  if (query.revoke_tokens) {
    revoked = db.tokens.filter((t) => t.user_id === u.id).length
    remove(db.tokens, (t) => t.user_id === u.id)
  }
  audit('user.two_factor.reset', ['user', u.id], { username: u.username, factors: n, tokens_revoked: revoked }, u.tenant_id)
  return none()
})

// --------------------------------------------------------------- tokens

route.get('/api/v1/tokens', () => {
  const u = me()
  return db.tokens.filter((t) => t.user_id === u.id).map(({ user_id: _u, ...t }) => t)
})

route.post('/api/v1/tokens', ({ body }) => {
  const u = me()
  const name = body.name.trim()
  if (!name || name.length > 128) throw badRequest('name must be 1-128 characters')
  let scopes: S['TokenScope'][] | null = null
  if (body.scopes) {
    if (body.scopes.length === 0 || body.scopes.length > 32) throw badRequest('give 1-32 scopes, or none')
    for (const s of body.scopes) {
      const role = validRole(s.role)
      find(db.tenants, s.tenant_id, 'tenant')
      // The scope must not reach beyond what the user holds.
      if (!ROLE_PERMS[role]!.every((p) => may(p, s.tenant_id))) throw badRequest(`you do not hold ${role} at tenant ${s.tenant_id}`)
    }
    scopes = body.scopes.map((s) => ({ tenant_id: s.tenant_id, role: s.role }))
  }
  const days = body.expires_in_days
  const t = {
    id: newId(),
    user_id: u.id,
    name,
    created_at: now(),
    expires_at: days ? new Date(Date.now() + Math.min(3650, Math.max(1, days)) * 86400_000).toISOString() : null,
    last_used_at: null,
    scopes,
  }
  db.tokens.push(t)
  audit('token.create', ['token', t.id], { name, scopes: body.scopes ?? null })
  const { user_id: _u, ...view } = t
  return created({ ...view, token: `msttok_${hex(32)}` })
})

route.delete('/api/v1/tokens/{id}', ({ params }) => {
  const u = me()
  if (!db.tokens.some((t) => t.id === params.id && t.user_id === u.id)) throw notFound('token')
  remove(db.tokens, (t) => t.id === params.id)
  audit('token.delete', ['token', params.id], {})
  return none()
})

// ---------------------------------------------------------------- audit

route.get('/api/v1/audit', ({ query }) => {
  const tenant = query.tenant_id ?? db.session?.tenant_id ?? ''
  me()
  find(db.tenants, tenant, 'tenant')
  authorize('audit.read', tenant)
  const sub = subtree(tenant)
  const limit = Math.min(500, Math.max(1, query.limit ?? 100))
  const before = query.before
  return db.audit
    .filter((a) => a.tenant_id && sub.has(a.tenant_id) && (before === undefined || before === null || a.id < before))
    .sort((a, b) => b.id - a.id)
    .slice(0, limit)
})
