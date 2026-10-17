import { defineStore } from 'pinia'
import { onMounted, onUnmounted, watch } from 'vue'
import { useSession } from './session'

/** Kinds of things the controller announces changes to. */
export type Kind =
  | 'vm'
  | 'task'
  | 'host'
  | 'image'
  | 'tenant'
  | 'cluster'
  | 'storage_pool'
  | 'vpc'
  | 'network'
  | 'security_group'
  | 'external_ip'
  | 'network_policy'
  | 'external_network'
  | 'load_balancer'
  | 'backup'
  | 'snapshot'
  | 'backup_repository'
const KINDS: Kind[] = [
  'vm',
  'task',
  'host',
  'image',
  'tenant',
  'cluster',
  'storage_pool',
  'vpc',
  'network',
  'security_group',
  'external_ip',
  'network_policy',
  'external_network',
  'load_balancer',
  'backup',
  'snapshot',
  'backup_repository',
]

/** A change: which one, and whether it was inserted, updated or deleted. */
export type Notice = { kind: Kind; id: string; op: 'insert' | 'update' | 'delete' }

/** `null` means changes may have been missed: refetch everything. */
type Listener = (n: Notice | null) => void
const listeners = new Set<{ kinds: Set<Kind>; fn: Listener }>()

function dispatch(n: Notice | null) {
  for (const l of listeners) if (!n || l.kinds.has(n.kind)) l.fn(n)
}

/** Calls `fn` for each change of these kinds, and on resync. */
export function onLive(kinds: Kind[], fn: Listener): () => void {
  const l = { kinds: new Set(kinds), fn }
  listeners.add(l)
  return () => listeners.delete(l)
}

let source: EventSource | null = null
let retryTimer = 0
let attempts = 0

/**
 * Live updates from `/api/v1/events`. The controller only says what changed;
 * views refetch it through the API.
 */
export const useLive = defineStore('live', {
  state: () => ({ connected: false }),
  actions: {
    start() {
      if (source) return
      const es = new EventSource('/api/v1/events')
      source = es
      es.onopen = () => {
        this.connected = true
        attempts = 0
      }
      es.addEventListener('resync', () => dispatch(null))
      for (const kind of KINDS) {
        es.addEventListener(kind, (e) => {
          try {
            dispatch({ kind, ...JSON.parse((e as MessageEvent<string>).data) })
          } catch {
            // Ignore a malformed event.
          }
        })
      }
      es.onerror = () => {
        this.connected = false
        // The browser retries dropped connections itself; it gives up when
        // the server refuses, e.g. because the session ended.
        if (es.readyState !== EventSource.CLOSED) return
        es.close()
        source = null
        const delay = Math.min(30_000, 1000 * 2 ** attempts++)
        retryTimer = window.setTimeout(async () => {
          // Signed out? Then the app returns to the sign-in page instead.
          if (await useSession().load()) this.start()
        }, delay)
      }
    },
    stop() {
      clearTimeout(retryTimer)
      source?.close()
      source = null
      this.connected = false
    },
  },
})

/**
 * Keeps a view current: runs `load` when something of these kinds changes
 * (and `filter` agrees), at most once per burst. Without live updates, it
 * polls slowly instead.
 */
export function useLiveRefresh(kinds: Kind[], load: () => unknown, filter: (n: Notice) => boolean = () => true) {
  const live = useLive()
  let pending = 0
  const soon = () => {
    clearTimeout(pending)
    pending = window.setTimeout(() => void load(), 150)
  }
  let unlisten = () => {}
  let poll = 0
  onMounted(() => {
    unlisten = onLive(kinds, (n) => (!n || filter(n)) && soon())
    poll = window.setInterval(() => !live.connected && void load(), 15_000)
  })
  onUnmounted(() => {
    unlisten()
    clearTimeout(pending)
    clearInterval(poll)
  })
  // Catch up after a reconnect.
  watch(
    () => live.connected,
    (now, before) => now && before === false && soon(),
  )
}
