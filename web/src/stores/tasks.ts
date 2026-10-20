import { defineStore } from 'pinia'
import { api, type Task } from '../api/client'
import { onLive, useLive } from './live'

const ACTIVE = new Set(['pending', 'running'])

let unlisten = () => {}
let pending = 0

/**
 * Recent tasks for the task tray, refreshed as tasks change. Without live
 * updates it polls instead. `poke()` after starting work refreshes at once.
 */
export const useTasks = defineStore('tasks', {
  state: () => ({ tasks: [] as Task[], timer: 0 as number }),
  getters: {
    active: (s) => s.tasks.filter((t) => ACTIVE.has(t.state)),
  },
  actions: {
    async refresh() {
      const { data } = await api.GET('/api/v1/tasks', { params: { query: { limit: 20 } } })
      if (data) this.tasks = data
    },
    start() {
      if (this.timer) return
      const live = useLive()
      void this.refresh().catch(() => {})
      unlisten = onLive(['task'], () => {
        // Progress comes in bursts; one refresh per burst is enough.
        clearTimeout(pending)
        pending = window.setTimeout(() => void this.refresh().catch(() => {}), 150)
      })
      this.timer = window.setInterval(() => {
        if (!live.connected) void this.refresh().catch(() => {})
      }, 10_000)
    },
    stop() {
      clearInterval(this.timer)
      clearTimeout(pending)
      unlisten()
      this.timer = 0
    },
    poke() {
      void this.refresh().catch(() => {})
    },
  },
})

const LABELS: Record<string, string> = {
  'vm.create': 'Create VM',
  'vm.start': 'Start',
  'vm.stop': 'Shut down',
  'vm.reboot': 'Reboot',
  'vm.delete': 'Delete VM',
  'vm.migrate': 'Migrate VM',
  'host.evacuate': 'Evacuate host',
  'host.recover': 'Recover host',
  'backup.create': 'Back up',
  'backup.delete': 'Delete backup',
  'snapshot.create': 'Take snapshot',
  'snapshot.revert': 'Revert to snapshot',
  'snapshot.delete': 'Delete snapshot',
  'disk.move': 'Move disk',
  'disk.attach': 'Attach disk',
  'disk.detach': 'Detach disk',
  'image.store': 'Store image',
}

export function taskLabel(t: Task): string {
  return LABELS[t.kind] ?? t.kind
}
