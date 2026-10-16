import { describe, expect, it } from 'vitest'
import { mount } from '@vue/test-utils'
import StatusLed from './components/StatusLed.vue'
import { mib, powerNote, vmState } from './state'

const vm = (status: string, power: string, extra: Partial<{ active_task: string | null; host_connected: boolean }> = {}) => ({
  status,
  power,
  host_connected: true,
  active_task: null,
  ...extra,
})

describe('vmState', () => {
  it('lifecycle beats power state', () => {
    expect(vmState(vm('creating', 'unknown'))).toEqual({ tone: 'busy', text: 'Creating' })
    expect(vmState(vm('deleting', 'running'))).toEqual({ tone: 'busy', text: 'Deleting' })
    expect(vmState(vm('error', 'stopped'))).toEqual({ tone: 'fault', text: 'Failed' })
  })

  it('shows what the host reports, never a guessed transition', () => {
    expect(vmState(vm('ready', 'running'))).toEqual({ tone: 'on', text: 'Running' })
    // A guest that powered itself off is simply stopped.
    expect(vmState(vm('ready', 'stopped'))).toEqual({ tone: 'off', text: 'Stopped' })
    expect(vmState(vm('ready', 'crashed'))).toEqual({ tone: 'fault', text: 'Crashed' })
    expect(vmState(vm('ready', 'missing'))).toEqual({ tone: 'fault', text: 'Missing on host' })
  })

  it('shows transitions only while hypermon is doing the work', () => {
    expect(vmState(vm('ready', 'stopped', { active_task: 'vm.start' }))).toEqual({ tone: 'busy', text: 'Starting' })
    expect(vmState(vm('ready', 'running', { active_task: 'vm.start' }))).toEqual({ tone: 'on', text: 'Running' })
    expect(vmState(vm('ready', 'running', { active_task: 'vm.stop' }))).toEqual({ tone: 'busy', text: 'Shutting down' })
  })

  it('does not present stale state as current', () => {
    expect(vmState(vm('ready', 'running', { host_connected: false }))).toEqual({ tone: 'unknown', text: 'Host unreachable' })
  })
})

describe('powerNote', () => {
  const base = {
    status: 'ready',
    power: 'stopped',
    power_reason: 'shutdown',
    power_observed_at: new Date().toISOString(),
    desired_power: 'running',
    host_connected: true,
    host_name: 'hm-host1',
    active_task: null,
  }
  it('explains why a VM stopped', () => {
    expect(powerNote(base)).toBe('The guest OS shut down · just now')
    expect(powerNote({ ...base, power_reason: null })).toBe('Stopped without a request from hypermon · just now')
    expect(powerNote({ ...base, power_reason: null, desired_power: 'stopped' })).toBeNull()
  })
  it('flags an ignored shutdown request and an unreachable host', () => {
    expect(powerNote({ ...base, power: 'running', power_reason: null, desired_power: 'stopped' })).toMatch(/still running/)
    expect(powerNote({ ...base, host_connected: false })).toBe('hm-host1 isn’t reporting, so this may be out of date. Last known state: stopped.')
  })
})

describe('formatting', () => {
  it('formats memory', () => {
    expect(mib(512)).toBe('512 MiB')
    expect(mib(1536)).toBe('1.5 GiB')
  })
})

describe('StatusLed', () => {
  it('renders the tone class and label', () => {
    const w = mount(StatusLed, { props: { tone: 'fault', label: 'Crashed' } })
    expect(w.find('.led').classes()).toContain('fault')
    expect(w.text()).toBe('Crashed')
  })
})
