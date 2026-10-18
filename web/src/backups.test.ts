import { describe, expect, it } from 'vitest'
import type { Backup } from './api/client'
import { backupKind, backupTone, builtOn } from './backups'

const backup = (b: Partial<Backup>): Backup => ({
  id: 'b',
  tenant_id: 't',
  vm_name: 'web-1',
  repository_id: 'r',
  state: 'ready',
  scheduled: false,
  kind: 'full',
  vcpus: 1,
  memory_mib: 512,
  disks: [1 << 30],
  size_bytes: 1 << 30,
  created_at: '2026-10-17T00:00:00Z',
  ...b,
})

describe('backups', () => {
  it('say whether they build on another, and why they were taken in full', () => {
    const full = backup({ id: 'a', full_reason: 'there is no earlier backup of the VM in this repository to build on' })
    const incr = backup({ id: 'b', kind: 'incremental', parent_id: 'a' })
    const merged = backup({ id: 'c', kind: 'incremental' })
    const all = [incr, full, merged]
    expect(backupKind(incr, all).label).toBe('Incremental')
    expect(backupKind(full, all)).toEqual({
      label: 'Full',
      title: 'Taken in full: there is no earlier backup of the VM in this repository to build on.',
    })
    expect(backupKind(merged, all).label).toBe('Full')
    expect(backupKind(merged, all).title).toContain('merged')
    expect(builtOn(full, all)?.id).toBe('b')
    expect(builtOn(incr, all)).toBeUndefined()
    expect(backupTone(backup({ state: 'deleting' }))).toBe('busy')
  })
})
