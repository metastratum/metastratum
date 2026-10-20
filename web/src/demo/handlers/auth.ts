// Signing in and out, and who is signed in. Passwords are checked as they
// are (everyone's is "demo" in the seed); a user with a second factor is
// asked for it, and any six digits pass for an authenticator's code.
import { audit, lineage, root } from '../ctx'
import { badRequest, HttpError, none, route } from '../http'
import { db, newId } from '../store'

/** Resolves a slug path (`northwind/dev`) from the provider down. */
function tenantByPath(path: string | null | undefined) {
  let t = root()
  for (const slug of (path ?? '').split('/').filter(Boolean)) {
    const child = db.tenants.find((x) => x.parent_id === t.id && x.slug === slug)
    if (!child) return null
    t = child
  }
  return t
}

const pendingSecondFactor = new Map<string, string>()

function signIn(userId: string, tenantId: string, method: string) {
  db.session = { user_id: userId, tenant_id: tenantId }
  audit('auth.login', null, { method }, tenantId)
}

route.get('/api/v1/auth/site', () => ({
  tenant_name: null,
  sub_tenants: db.tenants.some((t) => t.parent_id === root().id && t.domains.length === 0),
}))

route.get('/api/v1/auth/methods', () => ({ oidc: null }))

route.post('/api/v1/auth/login', ({ body }) => {
  const tenant = tenantByPath(body.tenant)
  const user = tenant && db.users.find((u) => u.tenant_id === tenant.id && u.username.toLowerCase() === body.username.toLowerCase())
  if (!user || user.disabled || user.password !== body.password) {
    audit('auth.login', null, { tenant: body.tenant ?? null }, user?.tenant_id, 'failure')
    throw new HttpError(401, 'unauthenticated', 'invalid username or password')
  }
  const factors = db.factors.filter((f) => f.user_id === user.id)
  if (factors.length) {
    const token = newId()
    pendingSecondFactor.set(token, user.id)
    const methods = [...new Set(factors.map((f) => f.kind))]
    if ((db.backupCodes[user.id] ?? 0) > 0) methods.push('backup_code')
    return { second_factor: { token, methods, webauthn: null } }
  }
  signIn(user.id, user.tenant_id, 'password')
  return none()
})

route.post('/api/v1/auth/login/second-factor', ({ body }) => {
  const userId = pendingSecondFactor.get(body.token)
  const user = userId && db.users.find((u) => u.id === userId)
  if (!user) throw new HttpError(401, 'unauthenticated', 'the sign-in has expired; start again')
  const code = (body.code ?? '').replace(/\s/g, '')
  if (body.webauthn) throw badRequest('security keys need a real controller; use a code in the demo')
  if (!/^\d{6}$/.test(code) && !/^[a-z0-9]{8,}$/i.test(code)) {
    throw new HttpError(401, 'unauthenticated', 'that code is not right')
  }
  if (!/^\d{6}$/.test(code)) db.backupCodes[user.id] = Math.max(0, (db.backupCodes[user.id] ?? 0) - 1)
  pendingSecondFactor.delete(body.token)
  signIn(user.id, user.tenant_id, 'second_factor')
  return none()
})

route.post('/api/v1/auth/logout', () => {
  db.session = null
  return none()
})

route.get('/api/v1/auth/me', () => {
  const s = db.session
  const u = s && db.users.find((x) => x.id === s.user_id && !x.disabled)
  if (!u || !s) throw new HttpError(401, 'unauthenticated', 'not signed in')
  const tenants = lineage(s.tenant_id)
  const pathOf = (id: string) =>
    lineage(id)
      .slice(1)
      .map((t) => t.slug)
      .join('/')
  const roles = db.bindings
    .filter((b) => b.user_id === u.id)
    .map((b) => ({
      role: b.role,
      tenant: pathOf(b.tenant_id),
      tenant_id: b.tenant_id,
      tenant_name: db.tenants.find((t) => t.id === b.tenant_id)?.name ?? '',
    }))
  const twoFactor = db.factors.some((f) => f.user_id === u.id)
  const required = tenants.some((t) => db.twoFactorRequired.includes(t.id))
  return {
    user_id: u.id,
    username: u.username,
    display_name: u.display_name,
    local: u.local,
    two_factor: twoFactor,
    two_factor_needed: required && !twoFactor,
    roles,
    tenant_id: s.tenant_id,
    tenant: pathOf(s.tenant_id),
    tenant_name: tenants.at(-1)?.name ?? '',
    provider_admin: roles.some((r) => r.tenant_id === root().id && r.role === 'admin'),
  }
})
