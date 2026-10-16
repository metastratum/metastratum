import type { Vm } from './api/client'

export type Tone = 'on' | 'busy' | 'fault' | 'off'

/**
 * What a VM's status LED shows, following hardware conventions: green when
 * running, amber while something is in progress, red when broken, dark when
 * off.
 */
export function vmState(vm: Pick<Vm, 'status' | 'power' | 'desired_power'>): { tone: Tone; text: string } {
  if (vm.status === 'creating') return { tone: 'busy', text: 'Creating' }
  if (vm.status === 'deleting') return { tone: 'busy', text: 'Deleting' }
  if (vm.status === 'error') return { tone: 'fault', text: 'Failed' }
  switch (vm.power) {
    case 'running':
      return vm.desired_power === 'stopped' ? { tone: 'busy', text: 'Shutting down' } : { tone: 'on', text: 'Running' }
    case 'shutting_down':
      return { tone: 'busy', text: 'Shutting down' }
    case 'stopped':
      return vm.desired_power === 'running' ? { tone: 'busy', text: 'Starting' } : { tone: 'off', text: 'Stopped' }
    case 'paused':
      return { tone: 'busy', text: 'Paused' }
    case 'crashed':
      return { tone: 'fault', text: 'Crashed' }
    case 'missing':
      return { tone: 'fault', text: 'Missing on host' }
    default:
      return { tone: 'off', text: 'Unknown' }
  }
}

export function gib(bytes: number): string {
  return `${Math.round((bytes / 2 ** 30) * 10) / 10} GiB`
}

export function mib(n: number): string {
  return n >= 1024 ? `${Math.round((n / 1024) * 10) / 10} GiB` : `${n} MiB`
}

export function ago(iso: string | null | undefined): string {
  if (!iso) return '—'
  const s = Math.max(0, (Date.now() - new Date(iso).getTime()) / 1000)
  if (s < 60) return 'just now'
  if (s < 3600) return `${Math.floor(s / 60)} min ago`
  if (s < 86400) return `${Math.floor(s / 3600)} h ago`
  return new Date(iso).toLocaleDateString()
}
