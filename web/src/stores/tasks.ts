import { defineStore } from 'pinia'
import { api, type Task } from '../api/client'

const ACTIVE = new Set(['pending', 'running'])

/**
 * Recent tasks for the task tray. Polls quickly while anything is in flight
 * and slowly otherwise; `poke()` after starting work refreshes immediately.
 */
export const useTasks = defineStore('tasks', {
  state: () => ({ tasks: [] as Task[], timer: 0 as number, listeners: new Set<() => void>() }),
  getters: {
    active: (s) => s.tasks.filter((t) => ACTIVE.has(t.state)),
  },
  actions: {
    async refresh() {
      const before = new Map(this.tasks.map((t) => [t.id, t.state]))
      const { data } = await api.GET('/api/v1/tasks', { params: { query: { limit: 20 } } })
      if (!data) return
      this.tasks = data
      // Tell views when something they show may have changed.
      if (data.some((t) => before.get(t.id) !== t.state)) this.listeners.forEach((fn) => fn())
    },
    start() {
      if (this.timer) return
      const tick = async () => {
        await this.refresh().catch(() => {})
        this.timer = window.setTimeout(tick, this.active.length ? 1500 : 8000)
      }
      void tick()
    },
    stop() {
      clearTimeout(this.timer)
      this.timer = 0
    },
    poke() {
      this.stop()
      this.start()
    },
    onChange(fn: () => void) {
      this.listeners.add(fn)
      return () => this.listeners.delete(fn)
    },
  },
})

const LABELS: Record<string, string> = {
  'vm.create': 'Create VM',
  'vm.start': 'Start',
  'vm.stop': 'Shut down',
  'vm.reboot': 'Reboot',
  'vm.delete': 'Delete VM',
}

export function taskLabel(t: Task): string {
  return LABELS[t.kind] ?? t.kind
}
