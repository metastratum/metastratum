// Live updates, as `/api/v1/events` sends them: named server-sent events
// (`vm`, `task`, ...) whose data says which record changed and how. The
// app's EventSource is replaced by one fed from here.
import type { Kind } from '../stores/live'

type Op = 'insert' | 'update' | 'delete'
const sources = new Set<FakeEventSource>()

/** Announces a change, as the controller's database triggers do. */
export function emit(kind: Kind, id: string, op: Op = 'update') {
  const data = JSON.stringify({ id, op })
  setTimeout(() => {
    for (const s of sources) s.deliver(kind, data)
  }, 0)
}

/** Stands in for `EventSource` on the API's event stream. */
export class FakeEventSource extends EventTarget {
  static readonly CONNECTING = 0
  static readonly OPEN = 1
  static readonly CLOSED = 2
  readonly CONNECTING = 0
  readonly OPEN = 1
  readonly CLOSED = 2
  readonly url: string
  readonly withCredentials = false
  readyState = 0
  onopen: ((e: Event) => void) | null = null
  onmessage: ((e: MessageEvent) => void) | null = null
  onerror: ((e: Event) => void) | null = null

  constructor(url: string | URL) {
    super()
    this.url = String(url)
    sources.add(this)
    setTimeout(() => {
      if (this.readyState === 2) return
      this.readyState = 1
      const e = new Event('open')
      this.onopen?.(e)
      this.dispatchEvent(e)
    }, 50)
  }

  deliver(kind: string, data: string) {
    if (this.readyState !== 1) return
    this.dispatchEvent(new MessageEvent(kind, { data }))
  }

  close() {
    this.readyState = 2
    sources.delete(this)
  }
}
