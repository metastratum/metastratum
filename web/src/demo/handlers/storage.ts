// Disks of their own (made, grown, attached, detached, moved), snapshots of
// VMs' disks, and backups (taken, scheduled, restored, deleted), as the
// controller's api/{disks,snapshots,backups}.rs and their engine tasks
// handle them. VMs' boot disks are made and removed with their VMs
// (vms.ts); here they are listed with the rest.
import type { components } from '../../api/schema'
import { audit, authorize, emit, hostName, lineage, may, root, subtree } from '../ctx'
import { accepted, badRequest, conflict, created, none, notFound, route, unavailable } from '../http'
import { LOCAL_POOL } from '../seed'
import { db, find, newId, now, remove, type BackupRec, type DiskRec, type SnapshotRec, type VmRec } from '../store'
import { activeTask, driver, startTask } from '../tasks'

type S = components['schemas']

const GIB = 2 ** 30
const MAX_GIB = 16384
const MAX_DATA_DISKS = 16
const MAX_SNAPSHOTS = 16

// ---- Views ----------------------------------------------------------------

export function diskView(d: DiskRec): S['DiskView'] {
  const pool = db.pools.find((p) => p.id === d.storage_pool_id)
  return {
    ...d,
    guest_path: d.vm_id && !d.boot ? `/dev/disk/by-id/scsi-0QEMU_QEMU_HARDDISK_${d.id.replace(/-/g, '')}` : null,
    storage_pool_name: pool?.name ?? '',
    storage_pool_kind: pool?.kind ?? 'local',
    host_name: hostName(d.host_id),
    vm_name: d.vm_id ? (db.vms.find((v) => v.id === d.vm_id)?.name ?? null) : null,
  }
}

export const snapshotView = (s: SnapshotRec): S['SnapshotView'] => ({ ...s })
export const backupView = (b: BackupRec): S['BackupView'] => ({ ...b })

/** A VM as the API shows it (for a restore's answer). */
function vmView(v: VmRec): S['VmView'] {
  const host = db.hosts.find((h) => h.id === v.host_id)
  const { password: _p, ...rec } = v
  return {
    ...rec,
    host_name: host?.name ?? null,
    host_connected: !!host?.connected,
    active_task: activeTask(v.id)?.kind ?? null,
    nics: v.nics.map((n) => {
      const fip = db.floatingIps.find((f) => f.nic_id === n.id)
      return {
        ...n,
        network_name: db.networks.find((x) => x.id === n.network_id)?.name ?? null,
        floating_ip: fip?.ip ?? null,
        floating_ip_id: fip?.id ?? null,
      }
    }),
  }
}

// ---- Helpers --------------------------------------------------------------

const vmOf = (id: string) => find(db.vms, id, 'VM')

/** Tenants whose things a list without `tenant_id` covers: all the user may read. */
function readable(perm: 'vm.read'): Set<string> {
  return new Set(db.tenants.filter((t) => may(perm, t.id)).map((t) => t.id))
}

/** Whether a tenant may use a pool: local storage, or granted (the provider has all). */
function mayUsePool(tenant: string, poolId: string): boolean {
  if (poolId === LOCAL_POOL) return true
  for (const t of lineage(tenant).reverse()) {
    if (t.id === root().id) return true
    const g = db.grants[t.id]
    if (g) return g.storage_pools.includes(poolId)
  }
  return false
}

function mayUseRepository(tenant: string, repoId: string): boolean {
  for (const t of lineage(tenant).reverse()) {
    if (t.id === root().id) return true
    const g = db.grants[t.id]
    if (g) return (g.backup_repositories ?? []).includes(repoId)
  }
  return false
}

/** Refuses growing what the tenant (and those above it) may allocate past a quota. */
function reserve(tenant: string, resource: string, delta: number) {
  if (delta <= 0) return
  for (const t of lineage(tenant)) {
    const max = db.quotas[t.id]?.[resource]
    if (max === undefined) continue
    const below = subtree(t.id)
    let used = 0
    if (resource === 'disk_gib') used = db.disks.filter((d) => below.has(d.tenant_id)).reduce((n, d) => n + d.size_gib, 0)
    if (resource === 'backup_gib') {
      used = db.backups.filter((b) => below.has(b.tenant_id)).reduce((n, b) => n + Math.ceil((b.stored_bytes ?? b.size_bytes) / GIB), 0)
    }
    if (used + delta > max) throw conflict(`quota exceeded for ${resource} in tenant ${t.name}`)
  }
}

function checkName(name: string): string {
  const n = name.trim()
  // eslint-disable-next-line no-control-regex
  if (!n || [...n].length > 64 || /[\u0000-\u001f\u007f]/.test(n)) {
    throw badRequest("a disk's name is 1 to 64 characters, on one line")
  }
  return n
}

/** A connected, ready host reaching a shared pool. */
function hostReaching(poolId: string): string {
  const pool = find(db.pools, poolId, 'storage pool')
  const h = db.hosts.find((x) => x.connected && x.state === 'ready' && x.cluster_id && pool.cluster_ids.includes(x.cluster_id))
  if (!h) throw unavailable(`no connected host reaches ${pool.name}`)
  return h.id
}

function checkHost(hostId: string) {
  const h = find(db.hosts, hostId, 'host')
  if (h.state !== 'ready' && h.state !== 'maintenance') throw conflict(`${h.name} is ${h.state}`)
  return h
}

/** Refuses work on a VM's disks while a snapshot, backup, or disk change has them. */
function checkIdle(vm: VmRec) {
  const snap = db.snapshots.find((s) => s.vm_id === vm.id && ['creating', 'reverting', 'deleting'].includes(s.state))
  if (snap) {
    throw conflict(
      `a snapshot of the VM is being ${snap.state === 'creating' ? 'taken' : snap.state === 'reverting' ? 'reverted to' : 'deleted'}`,
    )
  }
  if (db.backups.some((b) => b.vm_id === vm.id && b.state === 'running')) throw conflict('a backup of the VM is being taken')
  const disk = db.disks.find((d) => d.vm_id === vm.id && ['attaching', 'detaching', 'growing'].includes(d.state))
  if (disk) {
    throw conflict(`a disk of the VM is being ${disk.state === 'attaching' ? 'attached' : disk.state === 'detaching' ? 'detached' : 'grown'}`)
  }
}

/** Refuses changing the VM's disks while it is busy, or has snapshots. */
function checkVm(vm: VmRec, what: string) {
  if (vm.status !== 'ready') throw conflict(`the VM is ${vm.status}: ${what} afterwards`)
  if (vm.power !== vm.desired_power && !(vm.power === 'suspended' && vm.desired_power === 'running')) {
    throw conflict(`the VM is starting or stopping: ${what} once it is done`)
  }
  checkIdle(vm)
  if (db.snapshots.some((s) => s.vm_id === vm.id)) {
    throw conflict(`the VM has snapshots, which cover the disks it has: delete them to ${what}`)
  }
}

/** The VM's first free SCSI unit for a data disk. */
function freeUnit(vm: VmRec): number {
  const taken = vm.disks.slice(1).flatMap((d) => (d.unit == null ? [] : [d.unit]))
  let u = 0
  while (taken.includes(u)) u++
  return u
}

function touched(vm: VmRec) {
  vm.updated_at = now()
  emit('vm', vm.id)
}

/** Starts attaching a ready disk to a VM: its definition has it now, a task plugs it in. */
function beginAttach(vm: VmRec, disk: DiskRec): string {
  if (disk.vm_id) {
    const other = db.vms.find((v) => v.id === disk.vm_id)?.name ?? 'another VM'
    throw conflict(`the disk is attached to ${other}: detach it first`)
  }
  if (disk.state !== 'ready') throw conflict(`the disk is ${disk.state}`)
  if (disk.tenant_id !== vm.tenant_id) throw conflict('a disk attaches to VMs of its own tenant')
  checkVm(vm, 'attach the disk')
  if (!vm.host_id) throw conflict('the VM is on no host')
  if (vm.disks.length > MAX_DATA_DISKS) {
    throw conflict(`the VM has ${MAX_DATA_DISKS} disks besides its boot disk, as many as it may`)
  }
  const pool = find(db.pools, disk.storage_pool_id, 'storage pool')
  if (pool.kind === 'local') {
    if (disk.host_id !== vm.host_id) {
      throw conflict(
        `the disk is on ${hostName(disk.host_id) ?? 'no host'}, the VM on another host: move it to the VM (it moves, then attaches)`,
      )
    }
  } else {
    const cluster = db.hosts.find((h) => h.id === vm.host_id)?.cluster_id
    if (!cluster || !pool.cluster_ids.includes(cluster)) throw conflict(`the VM's host does not reach ${pool.name}`)
  }
  vm.disks.push({ volume_id: disk.id, pool_id: disk.storage_pool_id, size_bytes: disk.size_gib * GIB, image_id: null, unit: freeUnit(vm) })
  disk.vm_id = vm.id
  disk.state = 'attaching'
  touched(vm)
  emit('disk', disk.id)
  return startTask({ kind: 'disk.attach', tenant_id: vm.tenant_id, target_kind: 'disk', target_id: disk.id, params: { vm: vm.id } }).id
}

// ---- Disk tasks -----------------------------------------------------------

driver('disk.attach', {
  steps: (t) => {
    const vm = db.vms.find((v) => v.id === t.params.vm)
    return [[vm?.power === 'running' ? 'plugging it in' : 'adding it to the VM', 1500]]
  },
  finish: (t) => {
    const d = find(db.disks, t.target_id!, 'disk')
    d.state = 'ready'
    d.error = null
    emit('disk', d.id)
    const vm = db.vms.find((v) => v.id === t.params.vm)
    if (vm) touched(vm)
  },
  failed: (t, error) => {
    const d = db.disks.find((x) => x.id === t.target_id)
    const vm = db.vms.find((v) => v.id === t.params.vm)
    if (vm && d) {
      vm.disks = vm.disks.filter((x) => x.volume_id !== d.id)
      touched(vm)
    }
    if (d) {
      d.vm_id = null
      d.state = 'ready'
      d.error = `attaching it failed: ${error}`
      emit('disk', d.id)
    }
  },
})

driver('disk.detach', {
  steps: (t) => {
    const vm = db.vms.find((v) => v.id === t.params.vm)
    return [[vm?.power === 'running' ? 'unplugging it' : 'taking it out of the VM', 2000]]
  },
  finish: (t) => {
    const d = find(db.disks, t.target_id!, 'disk')
    const vm = db.vms.find((v) => v.id === t.params.vm)
    if (vm) {
      vm.disks = vm.disks.filter((x) => x.volume_id !== d.id)
      touched(vm)
    }
    d.vm_id = null
    d.state = 'ready'
    d.error = null
    emit('disk', d.id)
  },
  failed: (t, error) => {
    const d = db.disks.find((x) => x.id === t.target_id)
    if (d) {
      d.state = 'ready'
      d.error = `detaching it failed: ${error}`
      emit('disk', d.id)
    }
  },
})

driver('disk.move', {
  steps: (t) => {
    const d = db.disks.find((x) => x.id === t.target_id)
    const pool = db.pools.find((p) => p.id === t.params.pool)
    const to = pool?.kind === 'local' ? (hostName(t.params.host as string) ?? 'its host') : (pool?.name ?? 'the pool')
    const secs = Math.min(8000, 2500 + (d?.size_gib ?? 10) * 15)
    const out: [string, number][] = [
      [`copying to ${to}`, secs],
      ['removing the old copy', 1000],
    ]
    if (t.params.vm) out.push(['attaching', 900])
    return out
  },
  finish: (t) => {
    const d = find(db.disks, t.target_id!, 'disk')
    const pool = find(db.pools, t.params.pool as string, 'storage pool')
    d.storage_pool_id = pool.id
    d.host_id = pool.kind === 'local' ? ((t.params.host as string | null) ?? null) : null
    d.state = 'ready'
    d.error = null
    emit('disk', d.id)
    const vm = t.params.vm ? db.vms.find((v) => v.id === t.params.vm) : undefined
    if (vm) {
      try {
        beginAttach(vm, d)
      } catch (e) {
        // Moved, but not attached: it stays, ready, saying why.
        d.error = `moved, but not attached: ${e instanceof Error ? e.message : String(e)}`
      }
    }
  },
  failed: (t, error) => {
    const d = db.disks.find((x) => x.id === t.target_id)
    if (d) {
      d.state = 'ready'
      d.error = `moving it failed: ${error}`
      emit('disk', d.id)
    }
  },
})

// ---- Disk routes ----------------------------------------------------------

route.get('/api/v1/disks', ({ query }) => {
  let scope: Set<string>
  if (query.tenant_id) {
    find(db.tenants, query.tenant_id, 'tenant')
    authorize('vm.read', query.tenant_id)
    scope = subtree(query.tenant_id)
  } else {
    scope = readable('vm.read')
  }
  return db.disks
    .filter((d) => scope.has(d.tenant_id))
    .sort((a, b) => b.created_at.localeCompare(a.created_at))
    .map(diskView)
})

route.get('/api/v1/disks/{id}', ({ params }) => {
  const d = find(db.disks, params.id, 'disk')
  authorize('vm.read', d.tenant_id)
  return diskView(d)
})

route.get('/api/v1/vms/{id}/disks', ({ params }) => {
  const vm = vmOf(params.id)
  authorize('vm.read', vm.tenant_id)
  const order = vm.disks.map((d) => d.volume_id)
  return db.disks
    .filter((d) => d.vm_id === vm.id)
    .sort((a, b) => (b.boot ? 1 : 0) - (a.boot ? 1 : 0) || order.indexOf(a.id) - order.indexOf(b.id))
    .map(diskView)
})

route.post('/api/v1/disks', ({ body }) => {
  const name = checkName(body.name)
  if (!(body.size_gib >= 1 && body.size_gib <= MAX_GIB)) throw badRequest('size_gib is 1 to 16384')
  const vm = body.vm_id ? vmOf(body.vm_id) : undefined
  if (vm) {
    authorize('vm.write', vm.tenant_id)
    if (body.tenant_id && body.tenant_id !== vm.tenant_id) throw badRequest('a disk is attached to VMs of its own tenant')
  }
  const tenant = vm?.tenant_id ?? body.tenant_id ?? db.session?.tenant_id ?? root().id
  authorize('vm.write', tenant)
  const pool = find(db.pools, body.storage_pool_id ?? LOCAL_POOL, 'storage pool')
  if (!mayUsePool(tenant, pool.id)) throw notFound('storage pool')
  let host: string
  if (pool.kind === 'local') {
    if (vm) {
      if (!vm.host_id) throw conflict('the VM is on no host')
      host = vm.host_id
    } else if (body.host_id) {
      host = checkHost(body.host_id).id
    } else {
      throw badRequest(
        'a disk on local storage is on a host: give the VM to attach it to (vm_id), or the host (host_id)',
      )
    }
  } else {
    host = vm?.host_id ?? hostReaching(pool.id)
  }
  reserve(tenant, 'disk_gib', body.size_gib)
  const disk: DiskRec = {
    id: newId(),
    tenant_id: tenant,
    name,
    size_gib: body.size_gib,
    storage_pool_id: pool.id,
    host_id: pool.kind === 'local' ? host : null,
    vm_id: null,
    boot: false,
    guest_path: null,
    state: 'ready',
    error: null,
    created_at: now(),
  }
  db.disks.push(disk)
  emit('disk', disk.id, 'insert')
  audit('disk.create', ['disk', disk.id], { name, size_gib: body.size_gib, pool: pool.name, vm: vm?.name ?? null }, tenant)
  let task: string | null = null
  if (vm) task = beginAttach(vm, disk)
  return created({ ...diskView(disk), task_id: task })
})

route.patch('/api/v1/disks/{id}', ({ params, body }) => {
  const d = find(db.disks, params.id, 'disk')
  authorize('vm.write', d.tenant_id)
  if (body.name != null) {
    const name = checkName(body.name)
    d.name = name
    audit('disk.rename', ['disk', d.id], { name }, d.tenant_id)
  }
  if (body.size_gib != null) {
    const size = body.size_gib
    if (size > MAX_GIB || size <= d.size_gib) {
      throw badRequest(`the disk is ${d.size_gib} GiB: it can grow, to at most 16384 GiB`)
    }
    if (d.vm_id) {
      const vm = vmOf(d.vm_id)
      if (vm.status !== 'ready') throw conflict(`the VM is ${vm.status}: grow the disk afterwards`)
      checkIdle(vm)
      if (d.state !== 'ready') throw conflict(`the disk is ${d.state}: grow it afterwards`)
      reserve(d.tenant_id, 'disk_gib', size - d.size_gib)
      const spec = vm.disks.find((x) => x.volume_id === d.id)
      if (spec) spec.size_bytes = size * GIB
      touched(vm)
    } else {
      if (d.state !== 'ready') throw conflict(`the disk is ${d.state}: grow it afterwards`)
      reserve(d.tenant_id, 'disk_gib', size - d.size_gib)
    }
    d.size_gib = size
    audit('disk.resize', ['disk', d.id], { name: d.name, size_gib: size }, d.tenant_id)
  }
  emit('disk', d.id)
  return diskView(d)
})

route.delete('/api/v1/disks/{id}', ({ params }) => {
  const d = find(db.disks, params.id, 'disk')
  authorize('vm.write', d.tenant_id)
  if (d.vm_id) {
    const vm = db.vms.find((v) => v.id === d.vm_id)?.name ?? 'a VM'
    throw conflict(`the disk is attached to ${vm}: detach it first`)
  }
  if (d.state !== 'ready' && d.state !== 'failed') throw conflict(`the disk is ${d.state}: delete it afterwards`)
  remove(db.disks, (x) => x.id === d.id)
  emit('disk', d.id, 'delete')
  audit('disk.delete', ['disk', d.id], { name: d.name }, d.tenant_id)
  return none()
})

route.post('/api/v1/vms/{id}/disks', ({ params, body }) => {
  const vm = vmOf(params.id)
  authorize('vm.write', vm.tenant_id)
  const disk = find(db.disks, body.disk_id, 'disk')
  authorize('vm.write', disk.tenant_id)
  const task = beginAttach(vm, disk)
  audit('vm.attach_disk', ['vm', vm.id], { name: vm.name, disk: disk.id, disk_name: disk.name }, vm.tenant_id)
  return accepted({ ...diskView(disk), task_id: task })
})

route.delete('/api/v1/vms/{id}/disks/{disk_id}', ({ params }) => {
  const vm = vmOf(params.id)
  authorize('vm.write', vm.tenant_id)
  const disk = db.disks.find((d) => d.id === params.disk_id)
  if (!disk || disk.vm_id !== vm.id) throw notFound('disk')
  if (disk.boot) throw conflict('the boot disk stays with its VM')
  if (disk.state !== 'ready') throw conflict(`the disk is ${disk.state}`)
  checkVm(vm, 'detach the disk')
  disk.state = 'detaching'
  emit('disk', disk.id)
  const task = startTask({ kind: 'disk.detach', tenant_id: vm.tenant_id, target_kind: 'disk', target_id: disk.id, params: { vm: vm.id } })
  audit('vm.detach_disk', ['vm', vm.id], { name: vm.name, disk: disk.id, disk_name: disk.name }, vm.tenant_id)
  return accepted({ ...diskView(disk), task_id: task.id })
})

route.post('/api/v1/disks/{id}/move', ({ params, body }) => {
  const disk = find(db.disks, params.id, 'disk')
  authorize('vm.write', disk.tenant_id)
  const pool = find(db.pools, body.storage_pool_id ?? disk.storage_pool_id, 'storage pool')
  if (!mayUsePool(disk.tenant_id, pool.id)) throw notFound('storage pool')
  const vm = body.vm_id ? vmOf(body.vm_id) : undefined
  if (vm) {
    authorize('vm.write', vm.tenant_id)
    if (vm.tenant_id !== disk.tenant_id) throw badRequest('a disk attaches to VMs of its own tenant')
  }
  let host: string | null = null
  if (pool.kind === 'local') {
    const want = body.host_id ?? null
    const vmHost = vm?.host_id ?? null
    if (want && vmHost && want !== vmHost) {
      throw badRequest('a disk on local storage attaches to VMs on its host: give the VM’s, or no host')
    }
    host = want ?? vmHost
    if (!host) {
      throw badRequest(
        'a disk on local storage is on a host: give the host to move it to (host_id), or the VM to attach it to (vm_id)',
      )
    }
    const h = checkHost(host)
    if (!h.connected) throw conflict(`${h.name} is not connected`)
  } else if (vm) {
    if (!vm.host_id) throw conflict('the VM is on no host')
    const cluster = db.hosts.find((h) => h.id === vm.host_id)?.cluster_id
    if (!cluster || !pool.cluster_ids.includes(cluster)) throw badRequest(`the VM's host does not reach ${pool.name}`)
  }
  if (pool.id === disk.storage_pool_id && (pool.kind !== 'local' || host === disk.host_id)) {
    throw badRequest('the disk is there already')
  }
  if (disk.vm_id) {
    const name = db.vms.find((v) => v.id === disk.vm_id)?.name ?? 'a VM'
    throw conflict(`the disk is attached to ${name}: it moves with it (migrate the VM), or detach it first`)
  }
  if (disk.state !== 'ready') throw conflict(`the disk is ${disk.state}: move it afterwards`)
  disk.state = 'moving'
  disk.error = null
  emit('disk', disk.id)
  const task = startTask({
    kind: 'disk.move',
    tenant_id: disk.tenant_id,
    target_kind: 'disk',
    target_id: disk.id,
    params: { pool: pool.id, host, vm: body.vm_id ?? null, from_pool: disk.storage_pool_id, from_host: disk.host_id },
  })
  audit('disk.move', ['disk', disk.id], { name: disk.name, pool: pool.name, host, vm: body.vm_id ?? null }, disk.tenant_id)
  return accepted({ ...diskView(disk), task_id: task.id })
})

// ---- Snapshots ------------------------------------------------------------

driver('snapshot.create', {
  steps: () => [['taking the snapshot', 2200]],
  finish: (t) => {
    const s = find(db.snapshots, t.target_id!, 'snapshot')
    s.state = 'ready'
    // Stopped, or frozen by its guest agent (every seeded guest has one).
    s.consistent = true
    s.error = null
    emit('snapshot', s.id)
  },
  failed: (t, error) => {
    const s = db.snapshots.find((x) => x.id === t.target_id)
    if (s) {
      s.state = 'failed'
      s.error = error
      emit('snapshot', s.id)
    }
  },
})

driver('snapshot.revert', {
  steps: () => [['reverting the disks', 3000]],
  finish: (t) => {
    const s = find(db.snapshots, t.target_id!, 'snapshot')
    s.state = 'ready'
    s.error = null
    emit('snapshot', s.id)
    const vm = db.vms.find((v) => v.id === s.vm_id)
    if (vm) touched(vm)
  },
  failed: (t, error) => {
    const s = db.snapshots.find((x) => x.id === t.target_id)
    if (s) {
      s.state = 'ready'
      s.error = `reverting to it failed: ${error}`
      emit('snapshot', s.id)
    }
  },
})

driver('snapshot.delete', {
  steps: () => [['deleting the snapshot', 1500]],
  finish: (t) => {
    remove(db.snapshots, (x) => x.id === t.target_id)
    emit('snapshot', t.target_id!, 'delete')
  },
  failed: (t, error) => {
    const s = db.snapshots.find((x) => x.id === t.target_id)
    if (s) {
      s.state = 'ready'
      s.error = `deleting it failed: ${error}`
      emit('snapshot', s.id)
    }
  },
})

route.get('/api/v1/vms/{id}/snapshots', ({ params }) => {
  const vm = vmOf(params.id)
  authorize('vm.read', vm.tenant_id)
  return db.snapshots
    .filter((s) => s.vm_id === vm.id)
    .sort((a, b) => a.created_at.localeCompare(b.created_at))
    .map(snapshotView)
})

function defaultSnapshotName(vm: VmRec): string {
  const d = new Date()
  const p = (n: number) => String(n).padStart(2, '0')
  const day = `${d.getUTCFullYear()}-${p(d.getUTCMonth() + 1)}-${p(d.getUTCDate())}`
  const short = `${day} ${p(d.getUTCHours())}:${p(d.getUTCMinutes())} UTC`
  return db.snapshots.some((s) => s.vm_id === vm.id && s.name === short)
    ? `${day} ${p(d.getUTCHours())}:${p(d.getUTCMinutes())}:${p(d.getUTCSeconds())} UTC`
    : short
}

route.post('/api/v1/vms/{id}/snapshots', ({ params, body }) => {
  const vm = vmOf(params.id)
  authorize('vm.write', vm.tenant_id)
  const asked = body?.name?.trim()
  let name: string
  if (!asked) name = defaultSnapshotName(vm)
  // eslint-disable-next-line no-control-regex
  else if ([...asked].length > 64 || /[\u0000-\u001f\u007f]/.test(asked)) {
    throw badRequest("a snapshot's name is at most 64 characters, on one line")
  } else name = asked
  if (vm.status !== 'ready') throw conflict(`the VM is ${vm.status}: take a snapshot afterwards`)
  if (!vm.host_id) throw conflict('the VM is not placed on a host')
  if (vm.power !== vm.desired_power && !(vm.power === 'suspended' && vm.desired_power === 'running')) {
    throw conflict(`the VM is ${vm.desired_power === 'running' ? 'starting' : 'stopping'}: take a snapshot once it is`)
  }
  checkIdle(vm)
  if (db.snapshots.filter((s) => s.vm_id === vm.id).length >= MAX_SNAPSHOTS) {
    throw conflict(`a VM may have ${MAX_SNAPSHOTS} snapshots; delete one first`)
  }
  const s: SnapshotRec = {
    id: newId(),
    tenant_id: vm.tenant_id,
    vm_id: vm.id,
    name,
    state: 'creating',
    consistent: null,
    error: null,
    created_at: now(),
  }
  db.snapshots.push(s)
  emit('snapshot', s.id, 'insert')
  const task = startTask({ kind: 'snapshot.create', tenant_id: vm.tenant_id, target_kind: 'snapshot', target_id: s.id })
  audit('snapshot.create', ['vm', vm.id], { snapshot: s.id, name }, vm.tenant_id)
  return accepted({ ...snapshotView(s), task_id: task.id })
})

/** The snapshot and its VM, for a change to the snapshot: the VM's disks idle. */
function lockSnapshot(id: string): [SnapshotRec, VmRec] {
  const s = find(db.snapshots, id, 'snapshot')
  authorize('vm.write', s.tenant_id)
  const vm = vmOf(s.vm_id)
  if (vm.status !== 'ready' && vm.status !== 'error') throw conflict(`the VM is ${vm.status}: try again afterwards`)
  checkIdle(vm)
  return [s, vm]
}

route.post('/api/v1/snapshots/{id}/revert', ({ params }) => {
  const [s, vm] = lockSnapshot(params.id)
  if (s.state !== 'ready') throw conflict(`the snapshot is ${s.state}`)
  if (vm.status !== 'ready' || activeTask(vm.id) || vm.power !== 'stopped' || vm.desired_power !== 'stopped') {
    throw conflict(`stop ${vm.name} first: only a stopped VM can be reverted`)
  }
  s.state = 'reverting'
  s.error = null
  emit('snapshot', s.id)
  const task = startTask({ kind: 'snapshot.revert', tenant_id: s.tenant_id, target_kind: 'snapshot', target_id: s.id })
  audit('snapshot.revert', ['vm', vm.id], { snapshot: s.id, name: s.name }, s.tenant_id)
  return accepted({ task_id: task.id })
})

route.delete('/api/v1/snapshots/{id}', ({ params }) => {
  const [s, vm] = lockSnapshot(params.id)
  if (vm.power !== vm.desired_power && !(vm.power === 'suspended' && vm.desired_power === 'running')) {
    throw conflict(`${vm.name} is starting or stopping: try again once it is done`)
  }
  s.state = 'deleting'
  s.error = null
  emit('snapshot', s.id)
  const task = startTask({ kind: 'snapshot.delete', tenant_id: s.tenant_id, target_kind: 'snapshot', target_id: s.id })
  audit('snapshot.delete', ['vm', vm.id], { snapshot: s.id, name: s.name }, s.tenant_id)
  return accepted({ task_id: task.id })
})

// ---- Backups --------------------------------------------------------------

/** The repository to use: the one asked for, or the tenant's only one. */
function repositoryFor(tenant: string, asked: string | null | undefined) {
  const usable = db.repositories.filter((r) => mayUseRepository(tenant, r.id))
  if (asked) {
    const r = usable.find((x) => x.id === asked)
    if (!r) throw notFound('backup repository')
    return r
  }
  if (usable.length === 1) return usable[0]!
  if (!usable.length) throw badRequest('no backup repository is available to this tenant')
  throw badRequest('choose a backup repository')
}

function vmBackups(vm: VmRec): S['VmBackups'] {
  return {
    schedule: db.schedules[vm.id] ?? null,
    backups: db.backups
      .filter((b) => b.vm_id === vm.id)
      .sort((a, b) => b.created_at.localeCompare(a.created_at))
      .map(backupView),
  }
}

driver('backup.create', {
  steps: (t) => {
    const b = db.backups.find((x) => x.id === t.target_id)
    const gib = (b?.size_bytes ?? 0) / GIB
    return b?.parent_id
      ? [['copying what changed', 2500]]
      : [['copying disks', Math.min(9000, 3000 + gib * 30)]]
  },
  finish: (t) => {
    const b = find(db.backups, t.target_id!, 'backup')
    const vm = b.vm_id ? db.vms.find((v) => v.id === b.vm_id) : undefined
    b.state = 'ready'
    // A full copy keeps what the disks hold (about a third of them here);
    // an incremental one what changed since.
    b.stored_bytes = b.parent_id
      ? Math.round((0.4 + Math.random() * 1.6) * GIB)
      : Math.round(b.size_bytes * (0.25 + Math.random() * 0.15))
    b.consistent = true
    b.completed_at = now()
    b.error = null
    emit('backup', b.id)
    if (vm) emit('vm', vm.id)
  },
  failed: (t, error) => {
    const b = db.backups.find((x) => x.id === t.target_id)
    if (b) {
      b.state = 'failed'
      b.error = error
      b.completed_at = now()
      emit('backup', b.id)
    }
  },
})

driver('backup.delete', {
  steps: (t) => {
    const children = db.backups.some((x) => x.parent_id === t.target_id)
    const out: [string, number][] = []
    if (children) out.push(['merging into the backup built on it', 2500])
    out.push(['removing its files', 1000])
    return out
  },
  finish: (t) => {
    const b = find(db.backups, t.target_id!, 'backup')
    if (db.backups.some((x) => x.parent_id === b.id && x.state === 'running')) {
      throw new Error('a backup being taken builds on it')
    }
    // The backups built on it take in what it held, and build on what it did.
    for (const c of db.backups.filter((x) => x.parent_id === b.id)) {
      c.parent_id = b.parent_id ?? null
      c.stored_bytes = (c.stored_bytes ?? 0) + (b.stored_bytes ?? 0)
      emit('backup', c.id)
    }
    remove(db.backups, (x) => x.id === b.id)
    emit('backup', b.id, 'delete')
    if (b.vm_id) emit('vm', b.vm_id)
  },
  failed: (t, error) => {
    const b = db.backups.find((x) => x.id === t.target_id)
    if (b) {
      b.error = `deleting it failed: ${error}; delete it again`
      emit('backup', b.id)
    }
  },
})

route.get('/api/v1/vms/{id}/backups', ({ params }) => {
  const vm = vmOf(params.id)
  authorize('vm.read', vm.tenant_id)
  return vmBackups(vm)
})

route.post('/api/v1/vms/{id}/backups', ({ params, body }) => {
  const vm = vmOf(params.id)
  authorize('vm.write', vm.tenant_id)
  const repo = repositoryFor(vm.tenant_id, body?.repository_id)
  if (['creating', 'deleting', 'migrating', 'error'].includes(vm.status)) {
    throw conflict(`the VM is ${vm.status}: back it up afterwards`)
  }
  if (!vm.host_id) throw conflict('the VM is not placed on a host')
  if (vm.power !== vm.desired_power && !(vm.power === 'suspended' && vm.desired_power === 'running')) {
    throw conflict(`the VM is ${vm.desired_power === 'running' ? 'starting' : 'stopping'}: back it up once it is`)
  }
  checkIdle(vm)
  const size = vm.disks.reduce((n, d) => n + d.size_bytes, 0)
  reserve(vm.tenant_id, 'backup_gib', Math.ceil(size / GIB))
  const mode = body?.mode ?? 'incremental'
  const parent =
    mode === 'full'
      ? undefined
      : db.backups
          .filter((b) => b.vm_id === vm.id && b.repository_id === repo.id && b.state === 'ready')
          .sort((a, b) => b.created_at.localeCompare(a.created_at))[0]
  const b: BackupRec = {
    id: newId(),
    tenant_id: vm.tenant_id,
    vm_id: vm.id,
    vm_name: vm.name,
    repository_id: repo.id,
    kind: parent ? 'incremental' : 'full',
    parent_id: parent?.id ?? null,
    full_reason:
      mode === 'incremental' && !parent ? 'there is no earlier backup of the VM in this repository to build on' : null,
    state: 'running',
    scheduled: false,
    consistent: null,
    vcpus: vm.vcpus,
    memory_mib: vm.memory_mib,
    disks: vm.disks.map((d) => d.size_bytes),
    size_bytes: size,
    stored_bytes: null,
    error: null,
    created_at: now(),
    completed_at: null,
  }
  db.backups.push(b)
  emit('backup', b.id, 'insert')
  const task = startTask({ kind: 'backup.create', tenant_id: vm.tenant_id, target_kind: 'backup', target_id: b.id })
  audit('backup.create', ['vm', vm.id], { backup: b.id, repository: repo.name, kind: b.kind }, vm.tenant_id)
  return accepted({ ...backupView(b), task_id: task.id })
})

route.put('/api/v1/vms/{id}/backup-schedule', ({ params, body }) => {
  const vm = vmOf(params.id)
  authorize('vm.write', vm.tenant_id)
  let repository: string | null = null
  let fullEvery: number | null = null
  if (body.every == null) {
    delete db.schedules[vm.id]
  } else if (body.every === 'daily' || body.every === 'weekly') {
    const keep = body.keep ?? 7
    if (!(keep >= 1 && keep <= 365)) throw badRequest('keep must be 1-365')
    const full = body.full_every ?? 7
    if (!(full >= 1 && full <= 365)) throw badRequest('full_every must be 1-365')
    const repo = repositoryFor(vm.tenant_id, body.repository_id)
    db.schedules[vm.id] = { every: body.every, keep, full_every: full, repository_id: repo.id }
    repository = repo.id
    fullEvery = full
  } else {
    throw badRequest('every must be daily or weekly')
  }
  emit('vm', vm.id)
  audit(
    'vm.backup_schedule',
    ['vm', vm.id],
    { every: body.every ?? null, keep: body.keep ?? null, repository, full_every: fullEvery },
    vm.tenant_id,
  )
  return vmBackups(vm)
})

route.get('/api/v1/backups', ({ query }) => {
  let scope: Set<string>
  if (query.tenant_id) {
    find(db.tenants, query.tenant_id, 'tenant')
    authorize('vm.read', query.tenant_id)
    scope = subtree(query.tenant_id)
  } else {
    scope = readable('vm.read')
  }
  return db.backups
    .filter((b) => scope.has(b.tenant_id))
    .sort((a, b) => b.created_at.localeCompare(a.created_at))
    .map(backupView)
})

route.get('/api/v1/backups/{id}', ({ params }) => {
  const b = find(db.backups, params.id, 'backup')
  authorize('vm.read', b.tenant_id)
  return backupView(b)
})

route.delete('/api/v1/backups/{id}', ({ params }) => {
  const b = find(db.backups, params.id, 'backup')
  authorize('vm.write', b.tenant_id)
  if (b.state === 'running') throw conflict('the backup is being taken')
  if (db.backups.some((x) => x.parent_id === b.id && x.state === 'running')) {
    throw conflict('a backup being taken builds on it; delete it once that is done')
  }
  if (restoring(b.id)) throw conflict('a VM is being restored from the backup, or from one built on it')
  b.state = 'deleting'
  b.error = null
  emit('backup', b.id)
  const task = startTask({ kind: 'backup.delete', tenant_id: b.tenant_id, target_kind: 'backup', target_id: b.id })
  audit('backup.delete', ['backup', b.id], { vm_name: b.vm_name, created_at: b.created_at }, b.tenant_id)
  return accepted({ task_id: task.id })
})

/** Whether a VM is being restored from this backup or one built on it. */
function restoring(id: string): boolean {
  const below = new Set([id])
  for (let grew = true; grew; ) {
    grew = false
    for (const b of db.backups) {
      if (b.parent_id && below.has(b.parent_id) && !below.has(b.id)) {
        below.add(b.id)
        grew = true
      }
    }
  }
  return db.vms.some(
    (v) => v.status === 'creating' && v.disks.some((d) => d.restored_from && below.has(d.restored_from.backup_id)),
  )
}

/** Whether a backup it builds on is being deleted (merged into it). */
function deletingBelow(b: BackupRec): boolean {
  for (let p = b.parent_id ? db.backups.find((x) => x.id === b.parent_id) : undefined; p; ) {
    if (p.state === 'deleting') return true
    p = p.parent_id ? db.backups.find((x) => x.id === p!.parent_id) : undefined
  }
  return false
}

/** A free address on a network (its first ten left to the router and such). */
function freeIp(networkId: string): string {
  const n = find(db.networks, networkId, 'network')
  const prefix = n.cidr.split('/')[0]!.split('.').slice(0, 3).join('.')
  const used = new Set(db.vms.flatMap((v) => v.nics.filter((x) => x.network_id === n.id).map((x) => x.ip)))
  for (let i = 10; i < 255; i++) {
    const ip = `${prefix}.${i}`
    if (!used.has(ip) && ip !== n.gateway) return ip
  }
  throw conflict(`${n.name} has no free addresses`)
}

function newMac(): string {
  const used = new Set(db.vms.flatMap((v) => v.nics.map((n) => n.mac)))
  for (;;) {
    const m = `02:${[...crypto.getRandomValues(new Uint8Array(5))].map((b) => b.toString(16).padStart(2, '0')).join(':')}`
    if (!used.has(m)) return m
  }
}

/** Clusters a tenant may run VMs in. */
function allowedClusters(tenant: string): string[] {
  for (const t of lineage(tenant).reverse()) {
    if (t.id === root().id) return db.clusters.map((c) => c.id)
    const g = db.grants[t.id]
    if (g) return g.clusters
  }
  return []
}

route.post('/api/v1/backups/{id}/restore', ({ params, body }) => {
  const b = find(db.backups, params.id, 'backup')
  authorize('vm.write', b.tenant_id)
  if (b.state !== 'ready') throw conflict('the backup is not complete')
  if (deletingBelow(b)) {
    throw conflict('a backup it builds on is being deleted (merged into it): restore it once that is done')
  }
  const tenant = b.tenant_id
  const name = body.name.trim()
  if (!/^[A-Za-z0-9]([A-Za-z0-9-]{0,61}[A-Za-z0-9])?$/.test(name)) {
    throw badRequest('a VM name is 1-63 letters, digits and dashes, starting and ending with a letter or digit')
  }
  if (db.vms.some((v) => v.tenant_id === tenant && v.name === name)) throw conflict(`a VM named ${name} exists already`)

  // Where it goes: a ready host of an allowed cluster that reaches the pool.
  const pool = find(db.pools, body.storage_pool_id ?? LOCAL_POOL, 'storage pool')
  if (!mayUsePool(tenant, pool.id)) throw notFound('storage pool')
  let clusters = allowedClusters(tenant)
  if (body.cluster_id) {
    if (!clusters.includes(body.cluster_id)) throw notFound('cluster')
    clusters = [body.cluster_id]
  }
  if (pool.kind !== 'local') clusters = clusters.filter((c) => pool.cluster_ids.includes(c))
  const hosts = db.hosts.filter((h) => h.connected && h.state === 'ready' && h.cluster_id && clusters.includes(h.cluster_id))
  if (!hosts.length) throw unavailable('no host can take the VM: none of its clusters has a ready host that reaches its storage')
  const host = hosts.sort(
    (x, y) => db.vms.filter((v) => v.host_id === x.id).length - db.vms.filter((v) => v.host_id === y.id).length,
  )[0]!

  // On the network asked for, or the tenant's default one.
  const network = body.network_id
    ? find(db.networks, body.network_id, 'network')
    : db.networks.find((n) => {
        const vpc = db.vpcs.find((v) => v.id === n.vpc_id)
        return n.tenant_id === tenant && n.name === 'default' && vpc?.name === 'default'
      }) ?? db.networks.find((n) => n.tenant_id === tenant)
  if (!network || network.tenant_id !== tenant) throw badRequest('the tenant has no network to connect the VM to')
  const groups =
    body.security_group_ids ?? db.groups.filter((g) => g.tenant_id === tenant && g.name === 'default').map((g) => g.id)
  for (const g of groups) {
    const group = find(db.groups, g, 'security group')
    if (group.tenant_id !== tenant) throw notFound('security group')
  }

  const source = b.vm_id ? db.vms.find((v) => v.id === b.vm_id) : undefined
  const id = newId()
  const disks = b.disks.map((size, index) => ({
    volume_id: newId(),
    pool_id: pool.id,
    size_bytes: size,
    image_id: null,
    restored_from: { backup_id: b.id, index },
    unit: index === 0 ? null : index - 1,
  }))
  reserve(tenant, 'disk_gib', Math.ceil(b.disks.reduce((n, s) => n + s, 0) / GIB))
  const start = body.start ?? true
  const vm: VmRec = {
    id,
    tenant_id: tenant,
    name,
    host_id: host.id,
    status: 'creating',
    template: false,
    desired_power: start ? 'running' : 'stopped',
    power: 'stopped',
    power_reason: null,
    power_observed_at: now(),
    error: null,
    vcpus: b.vcpus,
    max_vcpus: Math.max(b.vcpus, source?.max_vcpus ?? 0),
    cpu_hotplug: source?.cpu_hotplug ?? false,
    memory_mib: b.memory_mib,
    max_memory_mib: Math.max(b.memory_mib, source?.max_memory_mib ?? 0),
    free_page_reporting: source?.free_page_reporting ?? true,
    firmware: source?.firmware ?? 'uefi',
    secure_boot: source?.secure_boot ?? false,
    tpm: source?.tpm ?? false,
    sleep: source?.sleep ?? true,
    guest_os: source?.guest_os ?? 'linux',
    first_boot_pending: false,
    account: source?.account ?? null,
    migratable: hosts.length > 1,
    cpu_model: host.cpu_vendor === 'AMD' ? 'EPYC-Genoa' : 'SapphireRapids',
    disks,
    nics: [{ id: newId(), network_id: network.id, ip: freeIp(network.id), mac: newMac(), security_group_ids: groups }],
    migration: null,
    created_at: now(),
    updated_at: now(),
  }
  db.vms.push(vm)
  disks.forEach((d, i) => {
    db.disks.push({
      id: d.volume_id,
      tenant_id: tenant,
      name: null,
      size_gib: Math.ceil(d.size_bytes / GIB),
      storage_pool_id: pool.id,
      host_id: pool.kind === 'local' ? host.id : null,
      vm_id: id,
      boot: i === 0,
      guest_path: null,
      state: 'ready',
      error: null,
      created_at: now(),
    })
    emit('disk', d.volume_id, 'insert')
  })
  emit('vm', id, 'insert')
  const task = startTask({ kind: 'vm.create', tenant_id: tenant, target_kind: 'vm', target_id: id, params: { start, restored_from: b.id } })
  audit('vm.create', ['vm', id], { name, vcpus: vm.vcpus, memory_mib: vm.memory_mib, host: host.name, restored_from: b.id }, tenant)
  audit('backup.restore', ['backup', b.id], { vm_name: b.vm_name }, tenant)
  return accepted({ ...vmView(vm), task_id: task.id })
})
