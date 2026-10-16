import { describe, expect, it } from 'vitest'
import { mount } from '@vue/test-utils'
import StatusLed from './components/StatusLed.vue'
import { mib, vmState } from './state'

const vm = (status: string, power: string, desired_power = 'running') => ({ status, power, desired_power })

describe('vmState', () => {
  it('lifecycle beats power state', () => {
    expect(vmState(vm('creating', 'unknown'))).toEqual({ tone: 'busy', text: 'Creating' })
    expect(vmState(vm('deleting', 'running'))).toEqual({ tone: 'busy', text: 'Deleting' })
    expect(vmState(vm('error', 'stopped'))).toEqual({ tone: 'fault', text: 'Failed' })
  })

  it('reflects power, including transitions toward the desired state', () => {
    expect(vmState(vm('ready', 'running'))).toEqual({ tone: 'on', text: 'Running' })
    expect(vmState(vm('ready', 'stopped', 'stopped'))).toEqual({ tone: 'off', text: 'Stopped' })
    expect(vmState(vm('ready', 'running', 'stopped'))).toEqual({ tone: 'busy', text: 'Shutting down' })
    expect(vmState(vm('ready', 'stopped', 'running'))).toEqual({ tone: 'busy', text: 'Starting' })
    expect(vmState(vm('ready', 'crashed'))).toEqual({ tone: 'fault', text: 'Crashed' })
    expect(vmState(vm('ready', 'missing'))).toEqual({ tone: 'fault', text: 'Missing on host' })
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
