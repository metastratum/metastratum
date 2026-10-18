import type { Backup } from './api/client'
import { ago } from './state'

/**
 * How a backup was taken, as a tag: whether it builds on another (and
 * which), or stands alone, and why it was taken in full when it was asked
 * to be incremental.
 */
export function backupKind(b: Backup, all: Backup[]): { label: string; title: string } {
  if (b.parent_id) {
    const parent = all.find((p) => p.id === b.parent_id)
    return {
      label: 'Incremental',
      title: `Holds what changed since the backup of ${parent ? ago(parent.created_at) : 'before'}, which it builds on. Restoring it gives the whole VM.`,
    }
  }
  if (b.kind === 'incremental') {
    return {
      label: 'Full',
      title: 'Taken incremental; the backups it built on were merged into it since, so it stands alone.',
    }
  }
  return {
    label: 'Full',
    title: b.full_reason ? `Taken in full: ${b.full_reason}.` : 'Every disk copied whole.',
  }
}

/** A backup's status light. */
export function backupTone(b: Backup): 'fault' | 'busy' | 'on' {
  if (b.state === 'failed') return 'fault'
  return b.state === 'running' || b.state === 'deleting' ? 'busy' : 'on'
}

/** The backup that builds on `b` (merged with it when `b` is deleted). */
export function builtOn(b: Backup, all: Backup[]): Backup | undefined {
  return all.find((c) => c.parent_id === b.id && c.state !== 'failed')
}
