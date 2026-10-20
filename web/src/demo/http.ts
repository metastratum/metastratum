// The demo's request handling: routes typed against the API's own schema
// (a handler answering with the wrong shape does not compile), an error
// type that answers as the controller does, and the `fetch` the app's API
// client is given.
import type { paths } from '../api/schema'

type Method = 'get' | 'post' | 'put' | 'patch' | 'delete'
type Op<P extends keyof paths, M extends Method> = NonNullable<paths[P][M]>

type Json<R> = R extends { content: { 'application/json': infer B } } ? B : undefined
type Success<O> = O extends { responses: infer R }
  ? { [S in keyof R]: S extends 200 | 201 | 202 | 204 ? Json<R[S]> : never }[keyof R]
  : never
type Params<O, K extends 'path' | 'query'> = O extends { parameters: { [P in K]?: infer X } }
  ? NonNullable<X>
  : Record<string, never>
type Body<O> = O extends { requestBody?: { content: { 'application/json': infer B } } }
  ? B
  : O extends { requestBody?: { content: { 'application/octet-stream': unknown } } }
    ? Uint8Array
    : undefined

export type Req<P extends keyof paths, M extends Method> = {
  params: Params<Op<P, M>, 'path'>
  query: Partial<Params<Op<P, M>, 'query'>>
  body: Body<Op<P, M>>
}

/** A reply with a status other than 200 (or with no body). */
export class Reply<B> {
  constructor(
    readonly status: number,
    readonly body: B,
  ) {}
}
export const created = <B>(b: B) => new Reply(201, b)
export const accepted = <B>(b: B) => new Reply(202, b)
export const none = () => new Reply(204, undefined)

/** An error answer, as the controller gives them: `{error, message}`. */
export class HttpError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
  ) {
    super(message)
  }
}
export const badRequest = (m: string) => new HttpError(400, 'bad_request', m)
export const unauthorized = (m = 'not signed in') => new HttpError(401, 'unauthorized', m)
export const forbidden = (m = 'not allowed') => new HttpError(403, 'forbidden', m)
export const notFound = (what: string) => new HttpError(404, 'not_found', `${what} not found`)
export const conflict = (m: string) => new HttpError(409, 'conflict', m)
export const unavailable = (m: string) => new HttpError(503, 'unavailable', m)

type Out<O> = Success<O> | Reply<Success<O>>
type Handler<P extends keyof paths, M extends Method> = (req: Req<P, M>) => Out<Op<P, M>> | Promise<Out<Op<P, M>>>

type Route = { method: string; pattern: RegExp; names: string[]; handler: (req: Req<never, never>) => unknown }
const routes: Route[] = []

function add(method: Method, path: string, handler: unknown) {
  const names: string[] = []
  const pattern = new RegExp(
    `^${path.replace(/\{(\w+)\}/g, (_, n: string) => {
      names.push(n)
      return '([^/]+)'
    })}$`,
  )
  routes.push({ method: method.toUpperCase(), pattern, names, handler: handler as Route['handler'] })
}

/** Registers handlers, by the API's paths (`/api/v1/...`). */
export const route = {
  get: <P extends keyof paths>(p: P, h: Handler<P, 'get'>) => add('get', p, h),
  post: <P extends keyof paths>(p: P, h: Handler<P, 'post'>) => add('post', p, h),
  put: <P extends keyof paths>(p: P, h: Handler<P, 'put'>) => add('put', p, h),
  patch: <P extends keyof paths>(p: P, h: Handler<P, 'patch'>) => add('patch', p, h),
  delete: <P extends keyof paths>(p: P, h: Handler<P, 'delete'>) => add('delete', p, h),
}

function json(status: number, body: unknown): Response {
  if (status === 204 || body === undefined) return new Response(null, { status })
  return new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } })
}

/** Query values as the controller reads them: numbers and booleans typed. */
function query(params: URLSearchParams): Record<string, unknown> {
  const out: Record<string, unknown> = {}
  for (const [k, v] of params) {
    out[k] = v === 'true' ? true : v === 'false' ? false : /^\d+$/.test(v) && k !== 'tenant' ? Number(v) : v
  }
  return out
}

let afterEach: () => void = () => {}
/** Runs after every request (to save what changed). */
export function onRequestDone(fn: () => void) {
  afterEach = fn
}

/** Answers a request to the API, as the controller would. */
export async function handle(request: Request): Promise<Response> {
  const url = new URL(request.url)
  const path = url.pathname
  for (const r of routes) {
    if (r.method !== request.method) continue
    const m = r.pattern.exec(path)
    if (!m) continue
    const params: Record<string, string> = {}
    r.names.forEach((n, i) => (params[n] = decodeURIComponent(m[i + 1]!)))
    let body: unknown = undefined
    const type = request.headers.get('content-type') ?? ''
    if (request.method !== 'GET') {
      if (type.includes('json')) {
        const text = await request.text()
        body = text ? JSON.parse(text) : undefined
      } else if (request.method !== 'DELETE') {
        body = new Uint8Array(await request.arrayBuffer())
      }
    }
    // A moment, as over a network.
    await new Promise((res) => setTimeout(res, 40 + Math.random() * 120))
    try {
      const out = await r.handler({ params, query: query(url.searchParams), body } as never)
      return out instanceof Reply ? json(out.status, out.body) : json(200, out)
    } catch (e) {
      if (e instanceof HttpError) return json(e.status, { error: e.code, message: e.message })
      console.error('[demo]', request.method, path, e)
      return json(500, { error: 'internal', message: `The demo failed: ${String(e)}` })
    } finally {
      afterEach()
    }
  }
  return json(404, { error: 'not_found', message: `${request.method} ${path} is not in the demo` })
}
