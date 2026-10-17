import { api, ok, type Disk } from './api/client'

/** What a disk is called: its name, or what it is to its VM. */
export function diskLabel(d: Disk): string {
  if (d.name) return d.name
  if (d.boot) return `Boot disk of ${d.vm_name ?? 'a VM'}`
  return `Disk of ${d.vm_name ?? 'a VM'}`
}

/** Where a disk is: its pool, and its host for local storage. */
export function diskWhere(d: Pick<Disk, 'storage_pool_kind' | 'storage_pool_name' | 'host_name'>): string {
  return d.storage_pool_kind === 'local' ? `Local on ${d.host_name ?? 'no host'}` : d.storage_pool_name
}

/** Whether attaching the disk to a VM on `host` moves it there first. */
export function movesTo(d: Disk, host: string | null | undefined): boolean {
  return d.storage_pool_kind === 'local' && !!host && d.host_id !== host
}

/**
 * Attaches the disk to the VM (a task). A disk on another host's local
 * storage moves to the VM's host first, then attaches.
 */
export async function attachDisk(d: Disk, vm: { id: string; host_id?: string | null }): Promise<void> {
  if (movesTo(d, vm.host_id)) {
    await ok(api.POST('/api/v1/disks/{id}/move', { params: { path: { id: d.id } }, body: { vm_id: vm.id } }))
  } else {
    await ok(api.POST('/api/v1/vms/{id}/disks', { params: { path: { id: vm.id } }, body: { disk_id: d.id } }))
  }
}

export const DISK_STATES: Record<string, string> = {
  creating: 'Making',
  attaching: 'Attaching',
  detaching: 'Detaching',
  moving: 'Moving',
  growing: 'Growing',
  deleting: 'Deleting',
  failed: 'Failed',
}
