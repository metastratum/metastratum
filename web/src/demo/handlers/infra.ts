// The provider's infrastructure: hosts (enrolling, maintenance, evacuating,
// fencing, retiring), clusters, storage pools, backup repositories and
// external networks. Mirrors the controller's api/{hosts,clusters,
// storage_pools,backups,external_networks}.rs.
import type { components } from '../../api/schema'
import { audit, authorizeInfra, emit } from '../ctx'
import { badRequest, conflict, created, accepted, none, route } from '../http'
import { LOCAL_POOL } from '../seed'
import {
  db,
  find,
  newId,
  now,
  remove,
  type ClusterRec,
  type ExternalNetworkRec,
  type HostRec,
  type PoolRec,
  type TaskRec,
  type VmRec,
} from '../store'
import { driver, startTask } from '../tasks'

type S = components['schemas']

// ---- Hosts ----

/** The VMs placed on a host. */
const vmsOn = (hostId: string) => db.vms.filter((v) => v.host_id === hostId)

/** Whether a VM keeps room on its host: meant to run, or still running. */
const keepsRoom = (v: VmRec) => v.desired_power === 'running' || v.power !== 'stopped'

/** Images only this host holds a copy of (nothing downloads them again). */
export function soleImages(h: HostRec): string[] {
  if (h.state === 'decommissioned') return []
  return db.images
    .filter((i) => i.storage_pool_id === LOCAL_POOL && i.hosts.length === 1 && i.hosts[0] === h.name)
    .map((i) => i.name)
}

export function hostView(h: HostRec): S['HostView'] {
  const vms = vmsOn(h.id)
  return {
    ...h,
    cluster_name: db.clusters.find((c) => c.id === h.cluster_id)?.name ?? null,
    vm_count: vms.length,
    allocated_memory_mib: vms.filter(keepsRoom).reduce((n, v) => n + Math.max(v.max_memory_mib, v.memory_mib), 0),
    sole_images: soleImages(h),
  }
}

/** What a host has left for more VMs, within its cluster's overcommit. */
export function room(h: HostRec): { vcpus: number; memory_mib: number } {
  const c = db.clusters.find((x) => x.id === h.cluster_id)
  const vms = vmsOn(h.id).filter(keepsRoom)
  // Moving here: counted as well.
  const coming = db.vms.filter((v) => v.migration?.dest_host_id === h.id)
  const used = [...vms, ...coming]
  return {
    vcpus: h.cpu_count * (c?.cpu_overcommit ?? 1) - used.reduce((n, v) => n + Math.max(v.max_vcpus, v.vcpus), 0),
    memory_mib: h.memory_mib * (c?.memory_overcommit ?? 1) - used.reduce((n, v) => n + Math.max(v.max_memory_mib, v.memory_mib), 0),
  }
}

route.get('/api/v1/hosts', () => {
  authorizeInfra('infra.read')
  return db.hosts.map(hostView)
})

route.post('/api/v1/hosts/join-tokens', ({ body }) => {
  authorizeInfra()
  const minutes = Math.min(1440, Math.max(1, body.ttl_minutes ?? 60))
  if (body.cluster_id) find(db.clusters, body.cluster_id, 'cluster')
  const expires_at = new Date(Date.now() + minutes * 60_000).toISOString()
  const secret = [...crypto.getRandomValues(new Uint8Array(24))].map((b) => b.toString(16).padStart(2, '0')).join('')
  audit('host.join_token', null, { expires_at, cluster_id: body.cluster_id ?? null })
  return created({ token: `mstj_${newId().replaceAll('-', '')}.${secret}`, expires_at, controllers: ['https://ctl-1.stratus.example:8443'] })
})

route.patch('/api/v1/hosts/{id}', ({ params, body }) => {
  authorizeInfra()
  const host = find(db.hosts, params.id, 'host')
  if (body.cluster_id !== undefined) {
    const cluster = body.cluster_id
    if (cluster) {
      find(db.clusters, cluster, 'cluster')
      // VMs do not move between AMD and Intel CPUs: a cluster has hosts of one vendor.
      const vendor = host.cpu_vendor
      const other =
        vendor && db.hosts.find((o) => o.id !== host.id && o.cluster_id === cluster && o.cpu_vendor && o.cpu_vendor !== vendor)
      if (other) {
        throw conflict(
          `${host.name} has a ${vendor} CPU, ${other.name} (in that cluster) a ${other.cpu_vendor} one: VMs do not move between them; use a cluster of its own`,
        )
      }
    }
    if (cluster !== host.cluster_id && vmsOn(host.id).length) {
      throw conflict("move or delete the host's VMs before changing its cluster")
    }
    host.cluster_id = cluster
  }
  if (body.state != null) {
    if (body.state !== 'ready' && body.state !== 'maintenance') throw badRequest('state must be ready or maintenance')
    host.state = body.state
  }
  audit('host.update', ['host', host.id], { name: host.name, cluster_id: body.cluster_id, state: body.state })
  emit('host', host.id)
  return none()
})

route.post('/api/v1/hosts/{id}/accept-facts', ({ params }) => {
  authorizeInfra()
  const host = find(db.hosts, params.id, 'host')
  const pending = host.pending_facts as Partial<HostRec> | null | undefined
  if (!pending) throw conflict('the host has no changes waiting')
  Object.assign(host, pending, { pending_facts: null })
  audit('host.accept_facts', ['host', host.id], { name: host.name, accepted: pending })
  emit('host', host.id)
  return none()
})

route.post('/api/v1/hosts/{id}/decommission', ({ params, query }) => {
  authorizeInfra()
  const host = find(db.hosts, params.id, 'host')
  if (host.state === 'decommissioned') throw conflict('the host is already decommissioned')
  if (vmsOn(host.id).length) throw conflict("move or delete the host's VMs before decommissioning it")
  const disks = db.disks.filter((d) => d.host_id === host.id).length
  if (disks > 0) throw conflict(`${disks} disks are on the host's storage: move or delete them before decommissioning it`)
  const sole = soleImages(host)
  if (sole.length && host.connected) {
    throw conflict(
      `the host holds the only copy of ${sole.length} image(s) (${sole.join(', ')}): evacuate it first, which copies them to other hosts`,
    )
  }
  const report: S['DecommissionReport'] = { host_cleanup: [], host_failures: [], certificates_revoked: 0, notes: [] }
  if (sole.length) {
    report.notes.push(`It held the only copy of ${sole.length} image(s), now gone: ${sole.join(', ')}. Delete them and add them again.`)
  }
  if (!query.wipe) {
    report.notes.push(
      "The machine was left as it is: metastratum's files, mounts, TLS settings and agent stay on it. Its agent can no longer connect; before enrolling it again, remove its identity (/var/lib/metastratum-agent).",
    )
  } else if (host.connected) {
    report.host_cleanup = [
      'unmounted storage pools',
      'removed cached images',
      'removed local volumes and seeds',
      "removed metastratum's TLS files and libvirt secrets",
      'restored the OVN southbound settings',
      'removed the agent identity, and stopped',
    ]
  } else {
    report.notes.push("The host's agent is not connected, so nothing on the machine was removed; reinstall it before reuse.")
  }
  report.certificates_revoked = 2
  for (const i of db.images) i.hosts = i.hosts.filter((n) => n !== host.name)
  host.state = 'decommissioned'
  host.cluster_id = null
  host.connected = false
  host.migration_ready = false
  host.controller = null
  host.connected_since = null
  audit('host.decommission', ['host', host.id], {
    name: host.name,
    wipe: !!query.wipe,
    revoked: report.certificates_revoked,
    host_cleanup: report.host_cleanup,
    host_failures: report.host_failures,
    notes: report.notes,
  })
  emit('host', host.id)
  return report
})

route.post('/api/v1/hosts/{id}/evacuate', ({ params }) => {
  authorizeInfra()
  const host = find(db.hosts, params.id, 'host')
  if (!host.cluster_id && vmsOn(host.id).length) throw conflict('the host is in no cluster, so its VMs have nowhere to go')
  host.state = 'maintenance'
  emit('host', host.id)
  const t = startTask({ kind: 'host.evacuate', tenant_id: db.session!.tenant_id, target_kind: 'host', target_id: host.id })
  audit('host.evacuate', ['host', host.id], { name: host.name })
  return accepted({ task_id: t.id })
})

/** A ready, connected host of the VM's cluster (not `from`) with room for it. */
function destinationFor(v: VmRec, from: HostRec): HostRec | undefined {
  const need = { vcpus: Math.max(v.max_vcpus, v.vcpus), memory_mib: Math.max(v.max_memory_mib, v.memory_mib) }
  return db.hosts
    .filter((h) => h.id !== from.id && h.cluster_id === from.cluster_id && h.state === 'ready' && h.connected)
    .filter((h) => (!v.tpm || h.tpm) && (!v.secure_boot || h.secure_boot))
    .map((h) => ({ h, r: room(h) }))
    .filter(({ r }) => r.vcpus >= need.vcpus && r.memory_mib >= need.memory_mib)
    .sort((a, b) => b.r.memory_mib - a.r.memory_mib)[0]?.h
}

/** What an evacuation moves, what failed, and what stays (stopped, asleep, bound to the host). */
type Evacuation = { vms: string[]; failures: string[]; staying: string[] }

/** Commits a VM's move: it runs on its destination from now on. */
function arrive(v: VmRec) {
  const dest = v.migration?.dest_host_id
  if (!dest) return
  for (const d of db.disks) if (d.vm_id === v.id && d.host_id === v.host_id) d.host_id = dest
  v.host_id = dest
  v.status = 'ready'
  v.migration = null
  v.updated_at = now()
  emit('vm', v.id)
}

driver('host.evacuate', {
  begin(t) {
    const host = find(db.hosts, t.target_id!, 'host')
    const on = vmsOn(host.id).filter((v) => v.status === 'ready')
    const running = on.filter((v) => v.power === 'running' && v.migratable)
    const staying = on
      .filter((v) => !running.includes(v))
      .map((v) => `${v.name} (${!v.migratable && v.power === 'running' ? 'not migratable' : v.power === 'suspended' ? 'asleep' : v.power})`)
    t.params = { vms: running.map((v) => v.id), failures: [], staying } satisfies Evacuation
  },
  steps(t) {
    const p = t.params as Evacuation
    const names = p.vms.map((id) => db.vms.find((v) => v.id === id)?.name ?? 'a VM')
    return [
      ...names.map((n, i): [string, number] => [`moving ${n} (${i + 1} of ${names.length})`, 2600]),
      ['copying images only it holds', 1200],
    ]
  },
  step(t, i) {
    const p = t.params as Evacuation
    const host = find(db.hosts, t.target_id!, 'host')
    // The previous VM has arrived.
    const prev = db.vms.find((v) => v.id === p.vms[i - 1])
    if (prev?.migration?.task_id === t.id) arrive(prev)
    const v = db.vms.find((x) => x.id === p.vms[i])
    if (!v || v.host_id !== host.id || v.power !== 'running') return
    const dest = destinationFor(v, host)
    if (!dest) {
      p.failures.push(`${v.name}: no other host of its cluster has room for it`)
      return
    }
    v.status = 'migrating'
    v.migration = {
      task_id: t.id,
      dest_host_id: dest.id,
      dest_host_name: dest.name,
      state: 'running',
      cancel_requested: false,
      started_at: now(),
    }
    emit('vm', v.id)
  },
  finish(t) {
    const p = t.params as Evacuation
    for (const id of p.vms) {
      const v = db.vms.find((x) => x.id === id)
      if (v?.migration?.task_id === t.id) arrive(v)
    }
    const host = find(db.hosts, t.target_id!, 'host')
    const failedImages: string[] = []
    for (const i of db.images) {
      if (i.storage_pool_id !== LOCAL_POOL || i.hosts.length !== 1 || i.hosts[0] !== host.name) continue
      const to = db.hosts.find((h) => h.id !== host.id && h.state === 'ready' && h.connected)
      if (!to) {
        failedImages.push(`${i.name}: no other host is up to take it`)
        continue
      }
      i.hosts.push(to.name)
      emit('image', i.id)
    }
    emit('host', host.id)
    const why: string[] = []
    if (p.failures.length) why.push(`${p.failures.length} VM(s) could not be moved: ${p.failures.join('; ')}`)
    if (failedImages.length) {
      why.push(`${failedImages.length} image(s) only it holds could not be copied elsewhere: ${failedImages.join('; ')}`)
    }
    if (why.length) throw new Error(why.join('; '))
  },
  failed(t: TaskRec) {
    // VMs on their way stay where they were.
    for (const v of db.vms) {
      if (v.migration?.task_id === t.id) {
        v.migration = null
        v.status = 'ready'
        emit('vm', v.id)
      }
    }
  },
})

/** Fencing settings as shown: secrets never come back. */
function fencingView(hostId: string): S['Fencing'] {
  const f = db.fencing[hostId]
  if (!f?.kind) return { kind: null, config: {} }
  const config = { ...((f.config ?? {}) as Record<string, unknown>) }
  // The seed's older spelling.
  if (config.url === undefined && typeof config.address === 'string') {
    config.url = config.address
    delete config.address
  }
  if (config.password !== undefined) config.password = '(set)'
  return { kind: f.kind, config }
}

route.get('/api/v1/hosts/{id}/fencing', ({ params }) => {
  authorizeInfra('infra.read')
  find(db.hosts, params.id, 'host')
  return fencingView(params.id)
})

route.put('/api/v1/hosts/{id}/fencing', ({ params, body }) => {
  authorizeInfra()
  const host = find(db.hosts, params.id, 'host')
  if (!body.kind) {
    delete db.fencing[host.id]
    host.fence_kind = null
  } else {
    const config = { ...((body.config ?? {}) as Record<string, unknown>) }
    const old = db.fencing[host.id]
    // Write-only secrets: kept unless replaced.
    if (old?.kind === body.kind && (config.password === undefined || config.password === '(set)')) {
      const kept = (old.config as Record<string, unknown> | null)?.password
      if (kept !== undefined) config.password = kept
      else delete config.password
    }
    if (body.kind === 'redfish') {
      if (typeof config.url !== 'string' || !/^https?:\/\//.test(config.url)) throw badRequest('redfish needs a url (https://...)')
      if (!config.username || !config.password) throw badRequest('redfish needs a username and a password')
    } else if (body.kind === 'libvirt') {
      if (typeof config.domain !== 'string' || !config.domain) throw badRequest('libvirt needs the domain of the host')
    } else {
      throw badRequest('kind must be redfish or libvirt')
    }
    db.fencing[host.id] = { kind: body.kind, config }
    host.fence_kind = body.kind
  }
  audit('host.fencing', ['host', host.id], { name: host.name, kind: body.kind ?? null })
  emit('host', host.id)
  return fencingView(host.id)
})

route.post('/api/v1/hosts/{id}/fencing/test', ({ params }) => {
  authorizeInfra()
  const host = find(db.hosts, params.id, 'host')
  if (!db.fencing[host.id]?.kind) throw conflict('the host has no fencing configured')
  return { power: host.state === 'decommissioned' ? 'off' : 'on' }
})

route.post('/api/v1/hosts/{id}/renew-certificate', ({ params }) => {
  authorizeInfra()
  const host = find(db.hosts, params.id, 'host')
  if (host.state === 'decommissioned') throw conflict('the host is decommissioned')
  host.cert_expires_at = new Date(Date.now() + 30 * 86400_000).toISOString()
  host.connected_since = now()
  audit('host.renew_certificate', ['host', host.id], { name: host.name })
  emit('host', host.id)
  return none()
})

// ---- Clusters ----

type ClusterBody = S['ClusterSpec']

function checkCluster(b: ClusterBody) {
  const name = b.name.trim()
  if (!name || name.length > 64) throw badRequest('name must be 1-64 characters')
  const cpu = b.cpu_overcommit ?? 4
  const mem = b.memory_overcommit ?? 1
  if (!(cpu >= 0.1 && cpu <= 64) || !(mem >= 0.1 && mem <= 4)) {
    throw badRequest('overcommit ratios must be 0.1-64 for CPU and 0.1-4 for memory')
  }
  const ok = (v: number | null | undefined, max: number) => v == null || (v >= 1 && v <= max)
  if (!(ok(b.vm_net_mbps, 400_000) && ok(b.vm_disk_iops, 10_000_000) && ok(b.vm_disk_mbps, 100_000))) {
    throw badRequest('limits must be positive: network up to 400000 Mbit/s, disks up to 10000000 IOPS and 100000 MB/s')
  }
  return {
    name,
    cpu_overcommit: cpu,
    memory_overcommit: mem,
    ha_enabled: !!b.ha_enabled,
    vm_net_mbps: b.vm_net_mbps ?? null,
    vm_disk_iops: b.vm_disk_iops ?? null,
    vm_disk_mbps: b.vm_disk_mbps ?? null,
  }
}

route.get('/api/v1/clusters', () => {
  authorizeInfra('infra.read')
  return db.clusters
})

route.post('/api/v1/clusters', ({ body }) => {
  authorizeInfra()
  const c: ClusterRec = { id: newId(), created_at: now(), ...checkCluster(body) }
  if (db.clusters.some((x) => x.name === c.name)) throw conflict('cluster already exists')
  db.clusters.push(c)
  audit('cluster.create', ['cluster', c.id], { name: c.name })
  emit('cluster', c.id, 'insert')
  return created(c)
})

route.put('/api/v1/clusters/{id}', ({ params, body }) => {
  authorizeInfra()
  const c = find(db.clusters, params.id, 'cluster')
  const v = checkCluster(body)
  if (db.clusters.some((x) => x.id !== c.id && x.name === v.name)) throw conflict('cluster already exists')
  Object.assign(c, v)
  audit('cluster.update', ['cluster', c.id], {
    name: c.name,
    cpu_overcommit: c.cpu_overcommit,
    memory_overcommit: c.memory_overcommit,
    limits: { vm_net_mbps: c.vm_net_mbps, vm_disk_iops: c.vm_disk_iops, vm_disk_mbps: c.vm_disk_mbps },
  })
  emit('cluster', c.id)
  return c
})

route.delete('/api/v1/clusters/{id}', ({ params }) => {
  authorizeInfra()
  const c = find(db.clusters, params.id, 'cluster')
  const hosts = db.hosts.filter((h) => h.cluster_id === c.id).length
  if (hosts) throw conflict(`the cluster still has ${hosts} hosts`)
  remove(db.clusters, (x) => x.id === c.id)
  for (const g of Object.values(db.grants)) g.clusters = g.clusters.filter((id) => id !== c.id)
  for (const p of db.pools) p.cluster_ids = p.cluster_ids.filter((id) => id !== c.id)
  audit('cluster.delete', ['cluster', c.id], { name: c.name })
  emit('cluster', c.id, 'delete')
  return none()
})

// ---- Storage pools ----

const SECRET_KEYS = ['key']

function redact(config: unknown): Record<string, unknown> {
  const c = { ...((config ?? {}) as Record<string, unknown>) }
  for (const k of SECRET_KEYS) if (k in c) c[k] = '(set)'
  return c
}

/** Volumes kept in a pool: the disks in it. */
const volumes = (poolId: string) => db.disks.filter((d) => d.storage_pool_id === poolId).length

export function poolView(p: PoolRec): S['PoolView'] {
  return { ...p, config: redact(p.config), volumes: volumes(p.id) }
}

function checkPoolConfig(kind: string, config: Record<string, unknown>) {
  const str = (k: string) => typeof config[k] === 'string' && (config[k] as string).trim() !== ''
  if (kind === 'nfs') {
    if (!str('server') || !str('export')) throw badRequest('NFS pools need a server and an export')
    if (!(config.export as string).startsWith('/')) throw badRequest('the export must be an absolute path')
  } else if (kind === 'rbd') {
    const m = config.monitors
    const monitors = Array.isArray(m) && m.length > 0 && m.every((x) => typeof x === 'string')
    if (!monitors || !str('pool') || !str('user') || !str('key')) {
      throw badRequest('RBD pools need monitors, a pool, a user and a key')
    }
  } else {
    throw badRequest('kind must be nfs or rbd')
  }
}

/** The storage a pool's data is on: pools with the same are one storage under two names. */
function storageOf(kind: string, config: Record<string, unknown>): string | null {
  const f = (k: string) => (typeof config[k] === 'string' ? (config[k] as string).trim() : null)
  if (kind === 'nfs') {
    const server = f('server')
    const exp = f('export')
    return server && exp ? `nfs:${server.toLowerCase()}:${exp.replace(/\/+$/, '')}` : null
  }
  if (kind === 'rbd') {
    const pool = f('pool')
    if (!Array.isArray(config.monitors) || !pool) return null
    const monitors = (config.monitors as string[])
      .map((m) => m.trim().toLowerCase())
      .map((m) => (m.includes(':') ? m : `${m}:6789`))
      .sort()
    return `rbd:${monitors.join(',')}:${pool}`
  }
  return null
}

function checkUnique(id: string | null, kind: string, config: Record<string, unknown>) {
  const storage = storageOf(kind, config)
  if (!storage) return
  const other = db.pools.find(
    (o) => o.id !== id && o.kind === kind && storageOf(o.kind, (o.config ?? {}) as Record<string, unknown>) === storage,
  )
  if (other) throw conflict(`${other.name} is on that storage already`)
}

function poolName(name: string) {
  const n = name.trim()
  if (!n || n.length > 64) throw badRequest('name must be 1-64 characters')
  return n
}

route.get('/api/v1/storage-pools', () => {
  authorizeInfra('infra.read')
  return db.pools.map(poolView)
})

route.post('/api/v1/storage-pools', ({ body }) => {
  authorizeInfra()
  const kind = body.kind ?? ''
  const config = { ...((body.config ?? {}) as Record<string, unknown>) }
  checkPoolConfig(kind, config)
  checkUnique(null, kind, config)
  const name = poolName(body.name)
  if (db.pools.some((p) => p.name === name)) throw conflict('storage pool already exists')
  for (const c of body.cluster_ids) find(db.clusters, c, 'cluster')
  const p: PoolRec = { id: newId(), name, kind, config, cluster_ids: [...body.cluster_ids], created_at: now() }
  db.pools.push(p)
  audit('storage_pool.create', ['storage_pool', p.id], { name, kind, config: redact(config) })
  emit('storage_pool', p.id, 'insert')
  return created(poolView(p))
})

route.put('/api/v1/storage-pools/{id}', ({ params, body }) => {
  authorizeInfra()
  const p = find(db.pools, params.id, 'storage pool')
  if (p.kind === 'local') throw badRequest('the local pool cannot be changed')
  const config = { ...((body.config ?? {}) as Record<string, unknown>) }
  // Secrets are never returned, so clients cannot send them back.
  const old = (p.config ?? {}) as Record<string, unknown>
  for (const k of SECRET_KEYS) if ((!(k in config) || config[k] === '(set)') && k in old) config[k] = old[k]
  checkPoolConfig(p.kind, config)
  checkUnique(p.id, p.kind, config)
  const name = poolName(body.name)
  if (db.pools.some((x) => x.id !== p.id && x.name === name)) throw conflict('storage pool already exists')
  const removed = p.cluster_ids.filter((c) => !body.cluster_ids.includes(c))
  if (removed.length && volumes(p.id) > 0) throw conflict('detaching clusters from a pool with volumes is not supported yet')
  for (const c of body.cluster_ids) find(db.clusters, c, 'cluster')
  Object.assign(p, { name, config, cluster_ids: [...body.cluster_ids] })
  audit('storage_pool.update', ['storage_pool', p.id], { name, config: redact(config) })
  emit('storage_pool', p.id)
  return poolView(p)
})

route.delete('/api/v1/storage-pools/{id}', ({ params, query }) => {
  authorizeInfra()
  const p = find(db.pools, params.id, 'storage pool')
  if (p.kind === 'local') throw badRequest('the local pool is built in')
  const n = volumes(p.id)
  if (n > 0) throw conflict(`the pool still holds ${n} volumes`)
  const images = db.images.filter((i) => i.storage_pool_id === p.id).length
  if (images > 0) throw conflict(`the pool keeps ${images} images; delete them first`)
  remove(db.pools, (x) => x.id === p.id)
  for (const g of Object.values(db.grants)) g.storage_pools = g.storage_pools.filter((id) => id !== p.id)
  audit('storage_pool.delete', ['storage_pool', p.id], { name: p.name, delete_data: !!query.delete_data })
  emit('storage_pool', p.id, 'delete')
  return none()
})

// ---- Backup repositories ----

route.get('/api/v1/backup-repositories', () => {
  authorizeInfra('infra.read')
  return db.repositories
})

route.post('/api/v1/backup-repositories', ({ body }) => {
  authorizeInfra()
  const name = body.name.trim()
  if (!name || name.length > 64) throw badRequest('the name must be 1-64 characters')
  if (!body.server.trim() || !body.export.startsWith('/')) {
    throw badRequest('a repository needs an NFS server and an absolute export path')
  }
  if (db.repositories.some((r) => r.name === name)) throw conflict('backup repository already exists')
  const r = {
    id: newId(),
    name,
    kind: 'nfs',
    server: body.server.trim(),
    export: body.export.replace(/\/+$/, '') || '/',
    created_at: now(),
  }
  db.repositories.push(r)
  const config: Record<string, unknown> = { server: r.server, export: r.export }
  const options = body.options?.trim()
  if (options) config.options = options
  audit('backup_repository.create', ['backup_repository', r.id], { name, config })
  emit('backup_repository', r.id, 'insert')
  return created(r)
})

route.delete('/api/v1/backup-repositories/{id}', ({ params }) => {
  authorizeInfra()
  const r = find(db.repositories, params.id, 'backup repository')
  if (db.backups.some((b) => b.repository_id === r.id)) throw conflict('the repository still holds backups; delete them first')
  remove(db.repositories, (x) => x.id === r.id)
  // Schedules that used it stop.
  for (const [vm, s] of Object.entries(db.schedules)) {
    if (s.repository_id === r.id) {
      delete db.schedules[vm]
      emit('vm', vm)
    }
  }
  for (const g of Object.values(db.grants)) {
    if (g.backup_repositories) g.backup_repositories = g.backup_repositories.filter((id) => id !== r.id)
  }
  audit('backup_repository.delete', ['backup_repository', r.id], { name: r.name })
  emit('backup_repository', r.id, 'delete')
  return none()
})

// ---- External networks ----

export function externalNetworkView(n: ExternalNetworkRec): S['ExternalNetworkView'] {
  const used =
    db.floatingIps.filter((f) => f.external_network_id === n.id).length +
    db.vpcs.filter((v) => v.external_network_id === n.id && v.gateway_ip).length +
    db.loadBalancers.filter((lb) => lb.external && db.vpcs.find((v) => v.id === lb.vpc_id)?.external_network_id === n.id).length
  return {
    ...n,
    used,
    hosts: db.hosts.filter((h) => h.state !== 'decommissioned' && h.physical_networks.includes(n.physical_network)).map((h) => h.name),
  }
}

const ipToInt = (ip: string) => ip.split('.').reduce((n, o) => n * 256 + Number(o), 0)
const intToIp = (n: number) => [24, 16, 8, 0].map((s) => Math.floor(n / 2 ** s) % 256).join('.')

/** An IPv4 CIDR (/8 to /30), normalized, or null. */
function parseCidr(s: string): { net: number; len: number } | null {
  const m = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})\/(\d{1,2})$/.exec(s)
  if (!m || m.slice(1, 5).some((o) => Number(o) > 255)) return null
  const len = Number(m[5])
  if (len < 8 || len > 30) return null
  const size = 2 ** (32 - len)
  return { net: Math.floor(ipToInt(m.slice(1, 5).join('.')) / size) * size, len }
}

function checkExtName(name: string) {
  const n = name.trim()
  if (!n || n.length > 64) throw badRequest('name must be 1-64 characters')
  return n
}

/** The pool's range, inside the network and in order. */
function checkPool(cidr: { net: number; len: number }, start: string, end: string) {
  const ip = /^\d{1,3}(\.\d{1,3}){3}$/
  if (!ip.test(start) || !ip.test(end)) throw badRequest('pool_start and pool_end must be IPv4 addresses')
  const size = 2 ** (32 - cidr.len)
  const inside = (a: number) => a > cidr.net && a < cidr.net + size - 1
  const [s, e] = [ipToInt(start), ipToInt(end)]
  if (!inside(s) || !inside(e) || s > e) throw badRequest('the pool must be a range of addresses within the network')
}

route.get('/api/v1/external-networks', () => {
  authorizeInfra('infra.read')
  return db.externalNetworks.map(externalNetworkView)
})

route.post('/api/v1/external-networks', ({ body }) => {
  authorizeInfra()
  const name = checkExtName(body.name)
  const physnet = body.physical_network.trim()
  if (!physnet || /[,: ]/.test(physnet)) throw badRequest('physical_network must be a bridge-mapping name')
  const cidr = parseCidr(body.cidr.trim())
  if (!cidr) throw badRequest('cidr must be an IPv4 range from /8 to /30')
  checkPool(cidr, body.pool_start.trim(), body.pool_end.trim())
  if (db.externalNetworks.some((n) => n.name === name)) throw conflict('external network already exists')
  const n: ExternalNetworkRec = {
    id: newId(),
    name,
    physical_network: physnet,
    cidr: `${intToIp(cidr.net)}/${cidr.len}`,
    gateway: body.gateway?.trim() || intToIp(cidr.net + 1),
    pool_start: body.pool_start.trim(),
    pool_end: body.pool_end.trim(),
    created_at: now(),
  }
  db.externalNetworks.push(n)
  audit('external_network.create', ['external_network', n.id], { name: n.name, cidr: n.cidr, physical_network: n.physical_network })
  emit('external_network', n.id, 'insert')
  return created(externalNetworkView(n))
})

route.put('/api/v1/external-networks/{id}', ({ params, body }) => {
  authorizeInfra()
  const n = find(db.externalNetworks, params.id, 'external network')
  const name = checkExtName(body.name)
  checkPool(parseCidr(n.cidr)!, body.pool_start.trim(), body.pool_end.trim())
  if (db.externalNetworks.some((x) => x.id !== n.id && x.name === name)) throw conflict('external network already exists')
  Object.assign(n, { name, pool_start: body.pool_start.trim(), pool_end: body.pool_end.trim() })
  audit('external_network.update', ['external_network', n.id], { name: n.name, pool_start: n.pool_start, pool_end: n.pool_end })
  emit('external_network', n.id)
  return externalNetworkView(n)
})

route.delete('/api/v1/external-networks/{id}', ({ params }) => {
  authorizeInfra()
  const n = find(db.externalNetworks, params.id, 'external network')
  if (db.vpcs.some((v) => v.external_network_id === n.id) || db.floatingIps.some((f) => f.external_network_id === n.id)) {
    throw conflict('VPCs or floating IPs still use this external network')
  }
  remove(db.externalNetworks, (x) => x.id === n.id)
  for (const g of Object.values(db.grants)) {
    if (g.external_networks) g.external_networks = g.external_networks.filter((id) => id !== n.id)
  }
  audit('external_network.delete', ['external_network', n.id], { name: n.name })
  emit('external_network', n.id, 'delete')
  return none()
})
