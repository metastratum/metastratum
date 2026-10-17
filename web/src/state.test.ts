import { describe, expect, it } from 'vitest'
import { mount } from '@vue/test-utils'
import StatusLed from './components/StatusLed.vue'
import { mib, powerNote, ruleText, shortId, vmState } from './state'

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

  it('shows transitions only while metastratum is doing the work', () => {
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
    host_name: 'mst-host1',
    active_task: null,
  }
  it('explains why a VM stopped', () => {
    expect(powerNote(base)).toBe('The guest OS shut down · just now')
    expect(powerNote({ ...base, power_reason: null })).toBe('Stopped without a request from metastratum · just now')
    expect(powerNote({ ...base, power_reason: null, desired_power: 'stopped' })).toBeNull()
  })
  it('flags an ignored shutdown request and an unreachable host', () => {
    expect(powerNote({ ...base, power: 'running', power_reason: null, desired_power: 'stopped' })).toMatch(/still running/)
    expect(powerNote({ ...base, host_connected: false })).toBe('mst-host1 isn’t reporting, so this may be out of date. Last known state: stopped.')
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

describe('ruleText', () => {
  it('says what a rule allows in words', () => {
    expect(ruleText({ direction: 'ingress', protocol: 'tcp', port_min: 22, port_max: 22, remote_cidr: '0.0.0.0/0' })).toBe(
      'TCP 22 from anywhere',
    )
    expect(ruleText({ direction: 'egress', protocol: 'udp', port_min: 53, port_max: 54, remote_cidr: '10.0.0.0/8' })).toBe(
      'UDP 53–54 to 10.0.0.0/8',
    )
    expect(ruleText({ direction: 'ingress', protocol: 'any', remote_group_name: 'default' })).toBe(
      'All traffic from members of default',
    )
    expect(ruleText({ direction: 'ingress', protocol: 'icmp', remote_cidr: '192.0.2.7/32' })).toBe('ICMP from 192.0.2.7')
  })
})

describe('shortId', () => {
  it('keeps the random end of a UUIDv7, which tells IDs made together apart', () => {
    const a = '01a14a88-8fb7-77ea-bd5a-754d60d29ccc'
    const b = '01a14a88-9c02-7a10-8e3f-0b6a5c71e4d2'
    expect(shortId(a)).toBe('…60d29ccc')
    expect(shortId(a)).not.toBe(shortId(b))
  })
})
