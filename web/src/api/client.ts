import createClient, { type Middleware } from 'openapi-fetch'
import type { components, paths } from './schema'

export type Schemas = components['schemas']
export type Vm = Schemas['VmView']
export type Image = Schemas['ImageView']
export type Host = Schemas['HostView']
export type Task = Schemas['TaskView']
export type Me = Schemas['Me']
export type Tenant = Schemas['TenantView']
export type User = Schemas['UserView']
export type Binding = Schemas['BindingView']
export type Cluster = Schemas['ClusterView']
export type Pool = Schemas['PoolView']
export type AuditEntry = Schemas['AuditEntry']
export type Token = Schemas['TokenView']
export type Vpc = Schemas['VpcView']
export type Network = Schemas['NetworkView']
export type SecurityGroup = Schemas['SecurityGroupView']
export type Rule = Schemas['RuleView']
export type FloatingIp = Schemas['FloatingIpView']
export type LoadBalancer = Schemas['LoadBalancerView']
export type Policy = Schemas['PolicyView']
export type ExternalNetwork = Schemas['ExternalNetworkView']
export type Backup = Schemas['BackupView']
export type BackupRepository = Schemas['RepositoryView']
export type Snapshot = Schemas['SnapshotView']
export type SecondFactor = Schemas['FactorView']
export type SecondFactorChallenge = Schemas['SecondFactorChallenge']

/** Raised for any non-2xx API response, carrying the server's message. */
export class ApiError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
  ) {
    super(message)
  }
}

let onUnauthenticated: () => void = () => {}
export function setUnauthenticatedHandler(fn: () => void) {
  onUnauthenticated = fn
}

const authRedirect: Middleware = {
  onResponse({ response, request }) {
    if (response.status === 401 && !request.url.includes('/auth/login') && !request.url.endsWith('/auth/me')) {
      onUnauthenticated()
    }
  },
}

export const api = createClient<paths>({ baseUrl: '', credentials: 'same-origin' })
api.use(authRedirect)

/** Unwraps an openapi-fetch result, throwing `ApiError` on failure. */
export async function ok<T>(
  req: Promise<{ data?: T; error?: unknown; response: Response }>,
): Promise<T> {
  const { data, error, response } = await req
  if (!response.ok) {
    const e = (error ?? {}) as { error?: string; message?: string }
    throw new ApiError(response.status, e.error ?? 'error', e.message ?? `Request failed (${response.status})`)
  }
  return data as T
}

/** The message to show for a failed request. */
export function errorText(e: unknown): string {
  return e instanceof ApiError ? e.message : 'The controller isn’t responding. Try again.'
}

/** WebSocket URL for a path on this origin. */
export function wsUrl(path: string): string {
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:'
  return `${proto}//${location.host}${path}`
}
