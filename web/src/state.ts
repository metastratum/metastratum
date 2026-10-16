import type { Vm } from './api/client'

export type Tone = 'on' | 'busy' | 'fault' | 'off' | 'unknown'

type VmLike = Pick<Vm, 'status' | 'power' | 'host_connected' | 'active_task'>

/**
 * What a VM's status LED shows, following hardware conventions: green when
 * running, amber while something is in progress, red when broken, dark when
 * off, hollow when we cannot currently know.
 *
 * Only observed facts are shown: what the host reports, and work hypermon is
 * actually doing. The requested power state is not used to guess at
 * transitions; see `powerNote` for when it differs.
 */
export function vmState(vm: VmLike): { tone: Tone; text: string } {
  if (vm.status === 'creating') return { tone: 'busy', text: 'Creating' }
  if (vm.status === 'deleting') return { tone: 'busy', text: 'Deleting' }
  if (vm.status === 'error') return { tone: 'fault', text: 'Failed' }
  if (!vm.host_connected) return { tone: 'unknown', text: 'Host unreachable' }

  switch (vm.active_task) {
    case 'vm.start':
      if (vm.power !== 'running') return { tone: 'busy', text: 'Starting' }
      break
    case 'vm.stop':
      if (vm.power === 'running' || vm.power === 'shutting_down') return { tone: 'busy', text: 'Shutting down' }
      break
    case 'vm.reboot':
      return { tone: 'busy', text: 'Rebooting' }
  }

  switch (vm.power) {
    case 'running':
      return { tone: 'on', text: 'Running' }
    case 'shutting_down':
      return { tone: 'busy', text: 'Shutting down' }
    case 'stopped':
      return { tone: 'off', text: 'Stopped' }
    case 'paused':
      return { tone: 'busy', text: 'Paused' }
    case 'crashed':
      return { tone: 'fault', text: 'Crashed' }
    case 'missing':
      return { tone: 'fault', text: 'Missing on host' }
    default:
      return { tone: 'unknown', text: 'Unknown' }
  }
}

const REASONS: Record<string, string> = {
  // Also covers a shutdown request the guest honoured.
  shutdown: 'The guest OS shut down',
  destroyed: 'Powered off',
  crashed: 'The guest crashed',
  failed: 'Failed to start',
  daemon: 'Stopped by libvirt on the host',
  saved: 'Saved to disk',
  migrated: 'Moved to another host',
  from_snapshot: 'Restored from a snapshot',
}

/**
 * A sentence explaining the current power state when there is more to say
 * than the LED label, or `null`.
 */
export function powerNote(vm: Pick<Vm, 'status' | 'power' | 'power_reason' | 'power_observed_at' | 'desired_power' | 'host_connected' | 'host_name' | 'active_task'>): string | null {
  if (vm.status !== 'ready' || vm.active_task) return null
  if (!vm.host_connected) {
    return `${vm.host_name ?? 'The host'} isn’t reporting, so this may be out of date. Last known state: ${vm.power.replace('_', ' ')}.`
  }
  if (vm.power === 'running' && vm.desired_power === 'stopped') {
    return 'A shutdown was requested, but the guest is still running. If it ignores the request, use Power off.'
  }
  if (vm.power === 'stopped' || vm.power === 'crashed') {
    const reason = vm.power_reason ? REASONS[vm.power_reason] : null
    if (reason) return `${reason} · ${ago(vm.power_observed_at)}`
    if (vm.desired_power === 'running') return `Stopped without a request from hypermon · ${ago(vm.power_observed_at)}`
  }
  return null
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
