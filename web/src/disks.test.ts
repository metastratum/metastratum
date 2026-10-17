import { describe, expect, it } from 'vitest'
import type { Disk } from './api/client'
import { diskLabel, diskWhere, movesTo } from './disks'

const disk = (d: Partial<Disk>): Disk => ({
  id: '01a14b00-0000-7000-8000-000000000001',
  tenant_id: 't',
  size_gib: 1,
  storage_pool_id: 'local',
  storage_pool_name: 'local',
  storage_pool_kind: 'local',
  boot: false,
  state: 'ready',
  created_at: '2026-10-17T00:00:00Z',
  ...d,
})

describe('disks', () => {
  it('are named, or named after what they are to their VM', () => {
    expect(diskLabel(disk({ name: 'data' }))).toBe('data')
    expect(diskLabel(disk({ boot: true, vm_name: 'web' }))).toBe('Boot disk of web')
    expect(diskLabel(disk({ vm_name: 'web' }))).toBe('Disk of web')
  })

  it('say where they are', () => {
    expect(diskWhere(disk({ host_name: 'h1' }))).toBe('Local on h1')
    expect(diskWhere(disk({ storage_pool_kind: 'nfs', storage_pool_name: 'shared' }))).toBe('shared')
  })

  it('move only from another host’s local storage', () => {
    expect(movesTo(disk({ host_id: 'h1' }), 'h2')).toBe(true)
    expect(movesTo(disk({ host_id: 'h1' }), 'h1')).toBe(false)
    expect(movesTo(disk({ storage_pool_kind: 'nfs' }), 'h2')).toBe(false)
    expect(movesTo(disk({ host_id: 'h1' }), null)).toBe(false)
  })
})
