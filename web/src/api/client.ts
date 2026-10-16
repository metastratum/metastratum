import createClient, { type Middleware } from 'openapi-fetch'
import type { components, paths } from './schema'

export type Schemas = components['schemas']
export type Vm = Schemas['VmView']
export type Image = Schemas['ImageView']
export type Host = Schemas['HostView']
export type Task = Schemas['TaskView']
export type Me = Schemas['Me']

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
    if (response.status === 401 && !request.url.endsWith('/auth/login') && !request.url.endsWith('/auth/me')) {
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

/** WebSocket URL for a path on this origin. */
export function wsUrl(path: string): string {
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:'
  return `${proto}//${location.host}${path}`
}
