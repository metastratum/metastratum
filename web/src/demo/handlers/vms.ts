// VMs: listing and creating them (placed on a host of a cluster the tenant
// may use, within quotas), changing them, powering them, moving them
// between hosts, cloning, their network interfaces, growing their disks,
// their memory, consoles, and the tasks that do the work. Mirrors the
// controller's api/vms.rs, api/tasks.rs, api/console.rs and engine/vm.rs,
// engine/migrate.rs.
import type { components } from '../../api/schema'
import { audit, authorize, emit, homeTenant, lineage, listScope, me, root, subtree, type Perm } from '../ctx'
import { accepted, badRequest, conflict, created, HttpError, none, notFound, route, unavailable } from '../http'
import { LOCAL_POOL } from '../seed'
import { db, find, newId, now, remove, save, type DiskRec, type NetworkRec, type NicRec, type VmRec } from '../store'
import { activeTask, driver, startTask, taskView } from '../tasks'

type S = components['schemas']
type VmView = S['VmView']
type HostRec = (typeof db.hosts)[number]

const GIB = 2 ** 30

// ---------------------------------------------------------------------------
// Views

/** A Windows VM's first boot is done a while after it first runs. */
const FIRST_BOOT_MS = 25_000

function settleFirstBoot(v: VmRec) {
  if (!v.first_boot_pending || v.power !== 'running' || !v.power_observed_at) return
  if (Date.now() - Date.parse(v.power_observed_at) < FIRST_BOOT_MS) return
  v.first_boot_pending = false
  v.password = null
  emit('vm', v.id)
  save()
}

function hostLive(h: HostRec | undefined): boolean {
  return !!h && h.connected && h.state !== 'decommissioned'
}

/** A VM as the API shows it. */
export function vmView(v: VmRec): VmView {
  settleFirstBoot(v)
  const h = db.hosts.find((x) => x.id === v.host_id)
  const { password: _pw, nics, migration, ...rest } = v
  return {
    ...rest,
    host_name: h?.name ?? null,
    host_connected: hostLive(h),
    active_task: activeTask(v.id)?.kind ?? null,
    migration: migration
      ? { ...migration, dest_host_name: db.hosts.find((x) => x.id === migration.dest_host_id)?.name ?? null }
      : null,
    nics: nics.map((n) => {
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

/** The VM, if the signed-in user may do `perm` with it (404 otherwise). */
function load(id: string, perm: Perm): VmRec {
  const v = find(db.vms, id, 'vm')
  authorize(perm, v.tenant_id)
  return v
}

function touch(v: VmRec) {
  v.updated_at = now()
  emit('vm', v.id)
}

function setPower(v: VmRec, power: string, reason: string | null = null) {
  v.power = power
  v.power_reason = reason
  v.power_observed_at = now()
  touch(v)
  if (v.host_id) emit('host', v.host_id)
}

// ---------------------------------------------------------------------------
// Hosts, room, placement

/** VMs a host keeps room for: running ones or about to, and migrations to it. */
function used(hostId: string): { vcpus: number; memory: number } {
  let vcpus = 0
  let memory = 0
  for (const v of db.vms) {
    const here = v.host_id === hostId && (v.desired_power === 'running' || v.power !== 'stopped')
    const coming = v.migration?.dest_host_id === hostId
    if (here || coming) {
      vcpus += v.max_vcpus
      memory += v.max_memory_mib
    }
  }
  return { vcpus, memory }
}

function clusterOf(h: HostRec) {
  return db.clusters.find((c) => c.id === h.cluster_id)
}

/** Why a host has no room for `vcpus` and `memory` more ("has only …"), or null. */
function lacksRoom(h: HostRec, vcpus: number, memory: number): string | null {
  const c = clusterOf(h)
  const u = used(h.id)
  const freeCpu = h.cpu_count * (c?.cpu_overcommit ?? 1) - u.vcpus
  const freeMem = h.memory_mib * (c?.memory_overcommit ?? 1) - u.memory
  if (freeCpu < vcpus) return `has only ${Math.max(0, freeCpu).toFixed(1)} vCPUs free`
  if (freeMem < memory) return `has only ${Math.max(0, Math.floor(freeMem))} MiB of memory free`
  return null
}

/** Why a host cannot run a VM whatever its room ("is not connected", …), or null. */
function cannotRun(h: HostRec, needs: { tpm: boolean; secure_boot: boolean }, maintenanceOk = false): string | null {
  if (!hostLive(h)) return 'is not connected'
  if (h.state !== 'ready' && !(maintenanceOk && h.state === 'maintenance')) return `is in ${h.state}`
  if (!h.kvm) return 'has no KVM'
  if (needs.tpm && !h.tpm) return 'has no TPM for guests (swtpm)'
  if (needs.secure_boot && !h.secure_boot) return "has no Secure Boot for guests (OVMF's)"
  return null
}

/** Picks the least loaded host that can take the VM, or says why none can. */
function pick(
  hosts: HostRec[],
  vcpus: number,
  memory: number,
  needs: { tpm: boolean; secure_boot: boolean },
): HostRec {
  const why: string[] = []
  let best: HostRec | null = null
  let bestLoad = Infinity
  for (const h of hosts) {
    const reason = cannotRun(h, needs) ?? lacksRoom(h, vcpus, memory)
    if (reason) {
      why.push(`${h.name} (${reason})`)
      continue
    }
    const c = clusterOf(h)
    const u = used(h.id)
    const load = u.memory / (h.memory_mib * (c?.memory_overcommit ?? 1)) + u.vcpus / (h.cpu_count * (c?.cpu_overcommit ?? 1))
    if (load < bestLoad) {
      best = h
      bestLoad = load
    }
  }
  if (!best) throw unavailable(why.length ? `no host can take this VM: ${why.join('; ')}` : 'no hosts are available')
  return best
}

/** Clusters the tenant may use (all of them for the provider). */
function allowedClusters(tenant: string): Set<string> {
  if (tenant === root().id) return new Set(db.clusters.map((c) => c.id))
  return new Set(db.grants[tenant]?.clusters ?? [])
}

function mayUsePool(tenant: string, poolId: string): boolean {
  return poolId === LOCAL_POOL || tenant === root().id || (db.grants[tenant]?.storage_pools ?? []).includes(poolId)
}

// ---------------------------------------------------------------------------
// Quotas

/** What a tenant's subtree uses. */
function usage(tenant: string): Record<string, number> {
  const ids = subtree(tenant)
  const out: Record<string, number> = { vms: 0, vcpus: 0, memory_mib: 0, disk_gib: 0 }
  for (const v of db.vms) {
    if (!ids.has(v.tenant_id)) continue
    out.vms! += 1
    out.vcpus! += v.max_vcpus
    out.memory_mib! += v.max_memory_mib
  }
  for (const d of db.disks) if (ids.has(d.tenant_id)) out.disk_gib! += d.size_gib
  return out
}

/** Refuses (409) what would go past a quota of the tenant or one above it. */
function reserve(tenant: string, delta: Record<string, number>) {
  for (const t of lineage(tenant)) {
    const limits = db.quotas[t.id]
    if (!limits) continue
    const u = usage(t.id)
    for (const [resource, max] of Object.entries(limits)) {
      const add = delta[resource] ?? 0
      const want = (u[resource] ?? 0) + add
      if (add > 0 && want > max) {
        throw new HttpError(409, 'quota_exceeded', `quota exceeded for ${resource} (${want} > ${max}) in tenant ${t.name}`)
      }
    }
  }
}

// ---------------------------------------------------------------------------
// Networks: defaults, addresses

function ipToInt(ip: string): number {
  return ip.split('.').reduce((a, b) => a * 256 + Number(b), 0)
}
function intToIp(n: number): string {
  return [24, 16, 8, 0].map((s) => Math.floor(n / 2 ** s) % 256).join('.')
}

/** The next free address of a network. */
function nextIp(n: NetworkRec): string {
  const [base, bits] = n.cidr.split('/')
  const size = 2 ** (32 - Number(bits))
  const start = ipToInt(base!)
  const taken = new Set<string>([n.gateway])
  for (const v of db.vms) for (const x of v.nics) if (x.network_id === n.id) taken.add(x.ip)
  for (const lb of db.loadBalancers) if (lb.network_id === n.id) taken.add(lb.ip)
  for (let i = size > 32 ? 10 : 2; i < size - 1; i++) {
    const ip = intToIp(start + i)
    if (!taken.has(ip)) return ip
  }
  throw conflict(`network ${n.name} has no free addresses`)
}

function newMac(): string {
  const taken = new Set(db.vms.flatMap((v) => v.nics.map((n) => n.mac)))
  for (;;) {
    const m = `02:${[...crypto.getRandomValues(new Uint8Array(5))].map((b) => b.toString(16).padStart(2, '0')).join(':')}`
    if (!taken.has(m)) return m
  }
}

/** The tenant's default network (its `default` VPC's), made if missing. */
function defaultNetwork(tenant: string): NetworkRec {
  let vpc = db.vpcs.find((v) => v.tenant_id === tenant && v.name === 'default')
  if (!vpc) {
    vpc = { id: newId(), tenant_id: tenant, name: 'default', external_network_id: null, gateway_ip: null, created_at: now() }
    db.vpcs.push(vpc)
    emit('vpc', vpc.id, 'insert')
  }
  let n = db.networks.find((x) => x.tenant_id === tenant && x.vpc_id === vpc.id && x.name === 'default')
  if (!n) {
    if (db.networks.some((x) => x.vpc_id === vpc.id && x.cidr === '10.0.0.0/24')) {
      throw conflict('the default VPC already has a network overlapping 10.0.0.0/24; choose a network for the VM')
    }
    n = { id: newId(), tenant_id: tenant, vpc_id: vpc.id, name: 'default', cidr: '10.0.0.0/24', gateway: '10.0.0.1', created_at: now() }
    db.networks.push(n)
    emit('network', n.id, 'insert')
  }
  return n
}

/** The tenant's `default` security group, made if missing. */
function defaultGroup(tenant: string): string {
  let g = db.groups.find((x) => x.tenant_id === tenant && x.name === 'default')
  if (!g) {
    g = {
      id: newId(),
      tenant_id: tenant,
      name: 'default',
      description: 'Allows all outgoing traffic, and incoming traffic from other members',
      created_at: now(),
    }
    db.groups.push(g)
    db.rules.push(
      { id: newId(), group_id: g.id, direction: 'egress', protocol: 'any', port_min: null, port_max: null, remote_cidr: '0.0.0.0/0', remote_group_id: null, description: '' },
      { id: newId(), group_id: g.id, direction: 'ingress', protocol: 'any', port_min: null, port_max: null, remote_cidr: null, remote_group_id: g.id, description: '' },
    )
    emit('security_group', g.id, 'insert')
  }
  return g.id
}

function checkGroups(tenant: string, ids: string[]): string[] {
  const out = [...new Set(ids)].sort()
  for (const id of out) {
    const g = db.groups.find((x) => x.id === id)
    if (!g || g.tenant_id !== tenant) throw notFound('security group')
  }
  return out
}

function tenantNetwork(tenant: string, id: string): NetworkRec {
  const n = db.networks.find((x) => x.id === id)
  if (!n || n.tenant_id !== tenant) throw notFound('network')
  return n
}

// ---------------------------------------------------------------------------
// Checks shared by several routes

function checkName(name: string) {
  const ok = /^[A-Za-z0-9-]{1,63}$/.test(name) && !name.startsWith('-') && !name.endsWith('-')
  if (!ok) throw badRequest('name must be 1-63 letters, digits or hyphens, not starting or ending with a hyphen')
}

function checkSize(vcpus: number, memory: number) {
  if (!(vcpus >= 1 && vcpus <= 128)) throw badRequest('vcpus must be between 1 and 128')
  if (!(memory >= 128 && memory <= 4 * 1024 * 1024)) throw badRequest('memory_mib must be between 128 and 4194304')
}

function checkMaxVcpus(vcpus: number, max: number | null | undefined) {
  if (max != null && (max < vcpus || max > 128)) throw badRequest('max_vcpus must be at least vcpus, and at most 128')
}

function checkMaxMemory(memory: number, max: number | null | undefined) {
  if (max != null && (max < memory || max > 4 * 1024 * 1024)) {
    throw badRequest('max_memory_mib must be at least memory_mib, and at most 4194304')
  }
}

const BUILT_IN = new Set([
  'guest',
  'defaultaccount',
  'wdagutilityaccount',
  'krbtgt',
  'administrators',
  'users',
  'guests',
  'power users',
  'remote desktop users',
  'backup operators',
  'system',
  'local service',
  'network service',
  'everyone',
  'authenticated users',
])

function windowsUser(name: string) {
  if (
    !name ||
    [...name].length > 20 ||
    /["/\\[\]:;|=,+*?<>@%]/.test(name) ||
    /\p{Cc}/u.test(name) ||
    name.endsWith('.') ||
    /^[. ]+$/.test(name) ||
    name !== name.trim()
  ) {
    throw badRequest(
      'a Windows account name is 1-20 characters, none of " / \\ [ ] : ; | = , + * ? < > @ %, not only dots and spaces, and does not start or end in a space or end in a dot',
    )
  }
  if (BUILT_IN.has(name.toLowerCase())) throw badRequest(`${name} is one of Windows' own accounts or groups: name another`)
}

function windowsPassword(pw: string, account: string) {
  const kinds = [/\p{Lu}/u.test(pw), /\p{Ll}/u.test(pw), /\d/.test(pw), /[^\p{L}\p{N}]/u.test(pw)].filter(Boolean).length
  if ([...pw].length < 8 || kinds < 3 || ([...account].length > 2 && pw.toLowerCase().includes(account.toLowerCase()))) {
    throw badRequest(
      `Windows needs a password of 8 characters or more, with three of: upper case, lower case, digits, symbols (and not the account name, ${account})`,
    )
  }
}

/** A disk change, snapshot or backup working on the VM's disks. */
function disksBusy(v: VmRec): string | null {
  const disk = db.disks.find((d) => d.vm_id === v.id && d.state !== 'ready' && d.state !== 'failed')
  if (disk) return `a disk of the VM is ${disk.state}`
  const snap = db.snapshots.find((s) => s.vm_id === v.id && s.state !== 'ready' && s.state !== 'failed')
  if (snap) return `a snapshot of the VM is ${snap.state}`
  const backup = db.backups.find((b) => b.vm_id === v.id && b.state !== 'ready' && b.state !== 'failed')
  if (backup) return `a backup of the VM is ${backup.state}`
  return null
}

// ---------------------------------------------------------------------------
// Creating VMs (from an image, or as a copy of another)

type CreateVm = S['CreateVm']
/** Where a new VM's disks come from. */
export type Source =
  | { kind: 'image' }
  | { kind: 'clone'; vm: VmRec }
  | { kind: 'backup'; backup: S['BackupView']; disks: { size_bytes: number; unit?: number | null }[]; guest_os: 'linux' | 'windows'; account: string | null }

/**
 * Creates a VM as the controller's create_inner does, and starts its
 * `vm.create` task. Exported for restores (backups).
 */
export function createVm(req: CreateVm, source: Source): S['Accepted_VmView'] {
  const fromImage = source.kind === 'image'
  checkName(req.name)
  checkSize(req.vcpus, req.memory_mib)
  checkMaxVcpus(req.vcpus, req.max_vcpus)
  checkMaxMemory(req.memory_mib, req.max_memory_mib)
  if (fromImage && !(req.disk_gib >= 1 && req.disk_gib <= 16384)) throw badRequest('disk_gib must be between 1 and 16384')
  const tenant = req.tenant_id ?? homeTenant()
  find(db.tenants, tenant, 'tenant')
  authorize('vm.write', tenant)
  if (db.vms.some((v) => v.tenant_id === tenant && v.name === req.name)) {
    throw conflict(`a VM named ${req.name} already exists in the tenant`)
  }

  let guestOs: 'linux' | 'windows' =
    source.kind === 'clone' ? source.vm.guest_os : source.kind === 'backup' ? source.guest_os : 'linux'
  let username = req.username ?? null
  let secureBoot = req.secure_boot
  let tpm = req.tpm
  let sleep = req.sleep
  let imageId: string | null = null
  if (fromImage) {
    const image = db.images.find((i) => i.id === req.image_id)
    const usable =
      image && (image.tenant_id === tenant || (image.shared && lineage(tenant).some((t) => t.id === image.tenant_id)))
    if (!image || !usable) throw notFound('image')
    imageId = image.id
    secureBoot ??= image.secure_boot
    tpm ??= image.tpm
    sleep ??= image.sleep
    if (image.os === 'windows') {
      guestOs = 'windows'
      username = username?.trim() ?? null
      if (
        username?.toLowerCase() === 'administrator' &&
        image.admin_user?.toLowerCase() !== 'administrator'
      ) {
        throw badRequest('the image does not use its Administrator (as Windows 11 does not): name another account')
      }
      username ??= image.admin_user ?? null
      if (!req.user_data) {
        if (!username) throw badRequest('name the Windows account to sign in with (username): the image has no default')
        if (!req.password) throw badRequest('a Windows VM needs a password, for its account')
      }
    }
    if (image.state === 'failed') {
      throw conflict(
        `the image could not be ${image.origin === 'upload' ? 'uploaded' : 'downloaded'} (${image.error ?? 'no reason given'}); delete it and add it again`,
      )
    }
    if (image.state === 'uploading') throw conflict('the image is still being uploaded; make VMs from it once it is ready')
    if (image.virtual_bytes && req.disk_gib * GIB < image.virtual_bytes) {
      throw badRequest(`the disk must be at least as large as the image: ${Math.ceil(image.virtual_bytes / GIB)} GiB`)
    }
  }
  if (guestOs === 'linux' && username) throw badRequest('username is for Windows VMs; Linux images have their default user')
  if (guestOs === 'windows' && source.kind === 'clone') username = source.vm.account ?? null
  if (guestOs === 'windows' && source.kind === 'backup') username = source.account
  if (username) windowsUser(username)
  const firmware = req.firmware ?? 'uefi'
  if (secureBoot && firmware !== 'uefi') throw badRequest('Secure Boot needs UEFI firmware')
  if (guestOs === 'windows' && !req.user_data) {
    if ((req.ssh_authorized_keys ?? []).length) throw badRequest('Windows VMs take a password (their account\'s), not SSH keys')
    if (req.password) windowsPassword(req.password, username ?? 'Administrator')
  }

  // Pools: a clone's disks stay in their pools (local ones on their host).
  const poolIds =
    source.kind === 'clone'
      ? [...new Set(source.vm.disks.map((d) => d.pool_id))]
      : [req.storage_pool_id ?? LOCAL_POOL]
  const pools = poolIds.map((id) => {
    const p = db.pools.find((x) => x.id === id)
    if (!p || !mayUsePool(tenant, id)) throw notFound('storage pool')
    return p
  })
  const pin = source.kind === 'clone' && pools.some((p) => p.kind === 'local') ? source.vm.host_id : null
  const pool = pools.find((p) => p.kind !== 'local') ?? pools[0]
  if (!pool) throw conflict('the VM has no disks')

  // Placement.
  let clusters = allowedClusters(tenant)
  if (pool.kind !== 'local') {
    clusters = new Set([...clusters].filter((c) => pool.cluster_ids.includes(c)))
    if (!clusters.size) throw unavailable(`storage pool ${pool.name} is not attached to any cluster this tenant may use`)
  }
  if (req.cluster_id) {
    if (!clusters.has(req.cluster_id)) throw notFound('cluster')
    clusters = new Set([req.cluster_id])
  }
  const reservedVcpus = Math.max(req.max_vcpus ?? 0, req.vcpus)
  const reservedMemory = Math.max(req.max_memory_mib ?? 0, req.memory_mib)
  const needs = { tpm: !!tpm, secure_boot: !!secureBoot }
  const candidates = db.hosts.filter(
    (h) => h.cluster_id && clusters.has(h.cluster_id) && (!pin || h.id === pin) && h.state !== 'decommissioned',
  )
  const host = pick(candidates, reservedVcpus, reservedMemory, needs)
  for (const other of pools.filter((p) => p.kind !== 'local')) {
    if (!host.cluster_id || !other.cluster_ids.includes(host.cluster_id)) {
      throw conflict(`no host has room that reaches all of its disks' pools (${other.name} too)`)
    }
  }
  const diskGib = req.disk_gib
  reserve(tenant, { vms: 1, vcpus: reservedVcpus, memory_mib: reservedMemory, disk_gib: diskGib })
  const network = req.network_id ? tenantNetwork(tenant, req.network_id) : defaultNetwork(tenant)
  const groups = req.security_group_ids ? checkGroups(tenant, req.security_group_ids) : [defaultGroup(tenant)]

  const id = newId()
  const disks: S['DiskSpec'][] =
    source.kind === 'clone'
      ? source.vm.disks.map((d) => ({
          volume_id: newId(),
          pool_id: d.pool_id,
          size_bytes: d.size_bytes,
          image_id: d.image_id ?? null,
          cloned_from: d.volume_id,
          unit: d.unit ?? null,
        }))
      : source.kind === 'backup'
        ? source.disks.map((d, index) => ({
            volume_id: newId(),
            pool_id: pool.id,
            size_bytes: d.size_bytes,
            restored_from: { backup_id: source.backup.id, index },
            unit: d.unit ?? null,
          }))
        : [{ volume_id: newId(), pool_id: pool.id, size_bytes: diskGib * GIB, image_id: imageId }]
  const sameCluster = db.hosts.filter((h) => h.cluster_id === host.cluster_id && h.state !== 'decommissioned')
  const migratable = req.migratable ?? sameCluster.length > 1
  const desired = req.start === false ? 'stopped' : 'running'
  const windowsPending = guestOs === 'windows' && !req.user_data && !!req.password
  const t = now()
  const nic: NicRec = { id: newId(), network_id: network.id, ip: nextIp(network), mac: newMac(), security_group_ids: groups }
  const vm: VmRec = {
    id,
    tenant_id: tenant,
    name: req.name,
    host_id: host.id,
    status: 'creating',
    template: false,
    desired_power: desired,
    power: 'stopped',
    power_reason: null,
    power_observed_at: null,
    error: null,
    vcpus: req.vcpus,
    max_vcpus: reservedVcpus,
    cpu_hotplug: (req.max_vcpus ?? 0) > req.vcpus,
    memory_mib: req.memory_mib,
    max_memory_mib: reservedMemory,
    free_page_reporting: req.free_page_reporting ?? true,
    firmware,
    secure_boot: !!secureBoot,
    tpm: !!tpm,
    sleep: sleep !== false,
    guest_os: guestOs,
    first_boot_pending: windowsPending,
    account: guestOs === 'windows' ? (username ?? 'Administrator') : null,
    migratable,
    cpu_model: null,
    disks,
    nics: [nic],
    migration: null,
    created_at: t,
    updated_at: t,
    password: windowsPending ? req.password : null,
  }
  db.vms.push(vm)
  for (const d of disks) {
    const local = db.pools.find((p) => p.id === d.pool_id)?.kind === 'local'
    const rec: DiskRec = {
      id: d.volume_id,
      tenant_id: tenant,
      name: null,
      size_gib: Math.ceil(d.size_bytes / GIB),
      storage_pool_id: d.pool_id,
      host_id: local ? host.id : null,
      vm_id: id,
      boot: d === disks[0],
      guest_path: null,
      state: 'creating',
      error: null,
      created_at: t,
    }
    db.disks.push(rec)
    emit('disk', rec.id, 'insert')
  }
  const task = startTask({
    kind: 'vm.create',
    tenant_id: tenant,
    target_kind: 'vm',
    target_id: id,
    params: {
      cloned_from: source.kind === 'clone' ? source.vm.id : null,
      restored_from: source.kind === 'backup' ? source.backup.id : null,
    },
  })
  emit('vm', id, 'insert')
  emit('host', host.id)
  emit('network', network.id)
  audit(
    'vm.create',
    ['vm', id],
    {
      name: req.name,
      vcpus: req.vcpus,
      memory_mib: req.memory_mib,
      disk_gib: req.disk_gib,
      host: host.name,
      secure_boot: !!secureBoot,
      tpm: !!tpm,
      sleep: sleep !== false,
      restored_from: source.kind === 'backup' ? source.backup.id : null,
      cloned_from: source.kind === 'clone' ? source.vm.id : null,
    },
    tenant,
  )
  return { ...vmView(vm), task_id: task.id }
}

/** The CPU model a cluster's hosts share. */
function clusterCpu(hostId: string | null | undefined): string | null {
  const h = db.hosts.find((x) => x.id === hostId)
  if (!h) return null
  const vendors = new Set(db.hosts.filter((x) => x.cluster_id === h.cluster_id).map((x) => x.cpu_vendor))
  if (vendors.size > 1) return 'qemu64-v3'
  return h.cpu_vendor === 'AMD' ? 'EPYC-Genoa' : 'SapphireRapids'
}

/** Starts a Windows VM's first boot clock (its password goes after). */
function afterBoot(v: VmRec) {
  if (!v.first_boot_pending) return
  setTimeout(() => {
    const cur = db.vms.find((x) => x.id === v.id)
    if (cur) settleFirstBoot(cur)
  }, FIRST_BOOT_MS + 500)
}

driver('vm.create', {
  steps: (t) => {
    const v = db.vms.find((x) => x.id === t.target_id)
    const copy = t.params.cloned_from || t.params.restored_from
    const steps: [string, number][] = [
      ['network', 900],
      [copy ? 'volumes' : 'image', copy ? 3500 : 2500],
      ['define', 1200],
    ]
    if (v?.desired_power === 'running') steps.push(['start', 1800])
    return steps
  },
  step: (t, i) => {
    // The source of a clone stays put while its disks are copied.
    if (i === 0) {
      const v = db.vms.find((x) => x.id === t.target_id)
      if (!v) throw new Error('the VM is gone')
      if (v.migratable && !v.cpu_model) v.cpu_model = clusterCpu(v.host_id)
    }
  },
  finish: (t) => {
    const v = db.vms.find((x) => x.id === t.target_id)
    if (!v) throw new Error('the VM is gone')
    v.status = 'ready'
    for (const d of db.disks) {
      if (d.vm_id === v.id && d.state === 'creating') {
        d.state = 'ready'
        emit('disk', d.id)
      }
    }
    if (v.desired_power === 'running') {
      setPower(v, 'running')
      afterBoot(v)
    } else {
      setPower(v, 'stopped')
    }
  },
  failed: (t, error) => {
    const v = db.vms.find((x) => x.id === t.target_id)
    if (!v) return
    v.status = 'error'
    v.error = error
    touch(v)
  },
})

route.get('/api/v1/vms', ({ query }) => {
  const tenants = listScope(query, 'vm.read')
  return db.vms
    .filter((v) => tenants.has(v.tenant_id))
    .sort((a, b) => a.name.localeCompare(b.name))
    .map(vmView)
})

route.get('/api/v1/vms/{id}', ({ params }) => vmView(load(params.id, 'vm.read')))

route.post('/api/v1/vms', ({ body }) => accepted(createVm(body, { kind: 'image' })))

route.post('/api/v1/vms/{id}/clone', ({ params, body }) => {
  const src = load(params.id, 'vm.write')
  if (src.status !== 'ready' || src.power !== 'stopped' || src.desired_power !== 'stopped') {
    throw conflict(`stop ${src.name} first: only a stopped VM is cloned`)
  }
  const busy = disksBusy(src)
  if (busy) throw conflict(`${busy}; try again once that is done`)
  const diskBytes = src.disks.reduce((a, d) => a + d.size_bytes, 0)
  const req: CreateVm = {
    tenant_id: src.tenant_id,
    name: body.name,
    vcpus: src.vcpus,
    max_vcpus: src.cpu_hotplug ? src.max_vcpus : null,
    memory_mib: src.memory_mib,
    max_memory_mib: src.max_memory_mib > src.memory_mib ? src.max_memory_mib : null,
    free_page_reporting: src.free_page_reporting,
    disk_gib: Math.ceil(diskBytes / GIB),
    image_id: '00000000-0000-0000-0000-000000000000',
    firmware: src.firmware,
    ssh_authorized_keys: body.ssh_authorized_keys ?? [],
    password: body.password ?? null,
    start: body.start ?? null,
    cluster_id: body.cluster_id ?? null,
    network_id: body.network_id ?? null,
    security_group_ids: body.security_group_ids,
    migratable: src.migratable,
    secure_boot: src.secure_boot,
    tpm: src.tpm,
    sleep: src.sleep,
  }
  return accepted(createVm(req, { kind: 'clone', vm: src }))
})

// ---------------------------------------------------------------------------
// Changing VMs

route.patch('/api/v1/vms/{id}', ({ params, body: req }) => {
  const vm = load(params.id, 'vm.write')
  const migratable = req.migratable ?? vm.migratable
  const vcpus = req.vcpus ?? vm.vcpus
  const memory = req.memory_mib ?? vm.memory_mib
  const maxVcpus = req.max_vcpus ?? Math.max(vm.max_vcpus, vcpus)
  const maxMemory = req.max_memory_mib ?? Math.max(vm.max_memory_mib, memory)
  const template = req.template ?? vm.template
  const reporting = req.free_page_reporting ?? vm.free_page_reporting
  const secureBoot = req.secure_boot ?? vm.secure_boot
  const tpm = req.tpm ?? vm.tpm
  const sleep = req.sleep ?? vm.sleep
  const fitCpu = !!req.fit_cpu
  if (secureBoot && vm.firmware !== 'uefi') throw badRequest('Secure Boot needs UEFI firmware')
  if (fitCpu && !migratable) throw badRequest("the VM does not migrate: it has its host's CPU as is")
  const only = (a: unknown[], b: unknown[]) => a.every((x, i) => x === b[i])
  const settings = [migratable, maxVcpus, maxMemory, template, reporting, secureBoot, tpm, sleep]
  const was = [vm.migratable, vm.max_vcpus, vm.max_memory_mib, vm.template, vm.free_page_reporting, vm.secure_boot, vm.tpm, vm.sleep]
  if (!fitCpu && only([vcpus, memory, ...settings], [vm.vcpus, vm.memory_mib, ...was])) return vmView(vm)
  checkSize(vcpus, memory)
  checkMaxVcpus(vcpus, maxVcpus)
  checkMaxMemory(memory, maxMemory)
  const busy = !!activeTask(vm.id)
  if (!vm.host_id) throw conflict('the VM is on no host')
  const running = vm.power === 'running' && vm.desired_power === 'running'
  if (vm.status === 'ready' && !busy && running) {
    if (fitCpu || !only(settings, was)) {
      throw conflict(`stop ${vm.name} first: only its CPUs and memory change while it runs, within what they may grow to`)
    }
    if (vcpus > vm.max_vcpus) {
      throw conflict(`${vm.name} can have ${vm.max_vcpus} vCPUs while it runs; stop it to give it more`)
    }
    if (memory > vm.max_memory_mib) {
      throw conflict(`${vm.name} can grow to ${vm.max_memory_mib} MiB while it runs; stop it to give it more`)
    }
    if (vcpus !== vm.vcpus && !vm.cpu_hotplug) {
      throw conflict(`${vm.name}'s CPUs are not laid out for plugging; stop it to change them (or give it room to grow)`)
    }
    const from = { vcpus: vm.vcpus, memory_mib: vm.memory_mib }
    vm.vcpus = vcpus
    vm.memory_mib = memory
    touch(vm)
    audit('vm.update', ['vm', vm.id], { name: vm.name, vcpus, memory_mib: memory, from, live: true }, vm.tenant_id)
    return vmView(vm)
  }
  if (vm.status !== 'ready' || busy || vm.power !== 'stopped' || vm.desired_power !== 'stopped') {
    throw conflict(`stop ${vm.name} first; this can only change while it is stopped`)
  }
  const growCpu = maxVcpus - vm.max_vcpus
  const growMem = maxMemory - vm.max_memory_mib
  if (growCpu > 0 || growMem > 0) reserve(vm.tenant_id, { vcpus: Math.max(0, growCpu), memory_mib: Math.max(0, growMem) })
  vm.vcpus = vcpus
  vm.max_vcpus = Math.max(maxVcpus, vcpus)
  vm.cpu_hotplug = maxVcpus > vcpus
  vm.memory_mib = memory
  vm.max_memory_mib = Math.max(maxMemory, memory)
  vm.free_page_reporting = reporting
  vm.secure_boot = secureBoot
  vm.tpm = tpm
  vm.sleep = sleep
  const migratableChanged = migratable !== vm.migratable
  vm.migratable = migratable
  if (migratable && (migratableChanged || fitCpu)) vm.cpu_model = clusterCpu(vm.host_id)
  vm.template = template
  touch(vm)
  audit(
    'vm.update',
    ['vm', vm.id],
    {
      name: vm.name,
      migratable: req.migratable ?? null,
      vcpus: req.vcpus ?? null,
      max_vcpus: req.max_vcpus ?? null,
      memory_mib: req.memory_mib ?? null,
      max_memory_mib: req.max_memory_mib ?? null,
      template: req.template ?? null,
      free_page_reporting: req.free_page_reporting ?? null,
      fit_cpu: fitCpu,
      secure_boot: req.secure_boot ?? null,
      tpm: req.tpm ?? null,
      sleep: req.sleep ?? null,
    },
    vm.tenant_id,
  )
  return vmView(vm)
})

route.post('/api/v1/vms/{id}/disks/{index}/resize', ({ params, body }) => {
  const vm = load(params.id, 'vm.write')
  const index = Number(params.index)
  const disk = vm.disks[index]
  if (!disk) throw notFound('disk')
  const size = body.size_gib * GIB
  if (body.size_gib > 16384 || size <= disk.size_bytes) {
    throw badRequest(`the disk is ${Math.ceil(disk.size_bytes / GIB)} GiB: it can grow, to at most 16384 GiB`)
  }
  if (vm.status !== 'ready') throw conflict(`the VM is ${vm.status}: resize it afterwards`)
  if (vm.power !== vm.desired_power) throw conflict('the VM is starting or stopping: resize it once it is done')
  const busy = disksBusy(vm)
  if (busy) throw conflict(`${busy}; try again once that is done`)
  if (!vm.host_id) throw conflict('the VM is on no host')
  const pool = db.pools.find((p) => p.id === disk.pool_id)
  if (pool?.kind !== 'rbd' && db.snapshots.some((s) => s.vm_id === vm.id)) {
    throw conflict('the VM has snapshots, and qcow2 disks cannot grow with snapshots in them; delete them first')
  }
  reserve(vm.tenant_id, { disk_gib: Math.ceil((size - disk.size_bytes) / GIB) })
  disk.size_bytes = size
  const rec = db.disks.find((d) => d.id === disk.volume_id)
  if (rec) {
    rec.size_gib = body.size_gib
    emit('disk', rec.id)
  }
  touch(vm)
  audit('vm.resize_disk', ['vm', vm.id], { name: vm.name, disk: index, size_gib: body.size_gib }, vm.tenant_id)
  return vmView(vm)
})

route.get('/api/v1/vms/{id}/memory', ({ params }) => {
  const vm = load(params.id, 'vm.read')
  if (vm.power !== 'running' || !vm.host_id) throw conflict(`${vm.name} is not running`)
  // Steady, plausible figures for each VM: a share of it in use.
  const seed = [...vm.id].reduce((a, c) => (a * 31 + c.charCodeAt(0)) % 997, 7)
  const usedShare = 0.25 + (seed % 50) / 100
  const available = vm.memory_mib - (vm.guest_os === 'windows' ? 0 : 96)
  const usable = Math.round(available * (1 - usedShare))
  return {
    actual_mib: vm.memory_mib,
    driver: true,
    available_mib: available,
    usable_mib: usable,
    unused_mib: Math.round(usable * 0.35),
    rss_mib: Math.round(vm.memory_mib * (usedShare + 0.08)),
  }
})

// ---------------------------------------------------------------------------
// Power

/**
 * A stopped VM takes nothing on its host: starting it needs room there again,
 * or (disks all on shared storage) another host of its cluster. As
 * make_room does.
 */
function makeRoom(vm: VmRec) {
  if (!vm.host_id || vm.desired_power === 'running' || vm.power !== 'stopped' || vm.template) return
  const host = db.hosts.find((h) => h.id === vm.host_id)
  if (!host) throw conflict("the VM's host no longer exists")
  const local = vm.disks.some((d) => db.pools.find((p) => p.id === d.pool_id)?.kind === 'local')
  const needs = { tpm: vm.tpm, secure_boot: vm.secure_boot }
  const unfit = cannotRun(host, needs, local)
  const full = unfit ? null : lacksRoom(host, vm.max_vcpus, vm.max_memory_mib)
  const noRoom = !!full
  const lacking = unfit ?? full
  if (!lacking) return
  const why = `${host.name} ${lacking}`
  if (local) {
    throw conflict(
      `${why}; it starts only there (its disks are on that host's own storage)${noRoom ? ': stop another VM there first' : ''}`,
    )
  }
  if (vm.status !== 'ready') throw conflict(`${why}; the VM is ${vm.status}, so it is not moved to another host`)
  const others = db.hosts.filter((h) => h.id !== host.id && h.cluster_id === host.cluster_id)
  try {
    const dest = pick(others, vm.max_vcpus, vm.max_memory_mib, needs)
    emit('host', host.id)
    vm.host_id = dest.id
    emit('host', dest.id)
  } catch (e) {
    throw conflict(`${why}, and no other host of its cluster can take it: ${e instanceof Error ? e.message : e}`)
  }
}

function powerAction(id: string, kind: string, desired: 'running' | 'stopped' | null, params: Record<string, unknown>) {
  const vm = load(id, 'vm.write')
  if (vm.status === 'deleting') throw conflict('VM is being deleted')
  if (vm.status === 'migrating') throw conflict('VM is being migrated; wait, or cancel the migration')
  if (desired === 'running') makeRoom(vm)
  if (db.snapshots.some((s) => s.vm_id === vm.id && s.state === 'reverting')) {
    throw conflict('the VM is being reverted to a snapshot; start it once that is done')
  }
  if (db.disks.some((d) => d.vm_id === vm.id && (d.state === 'attaching' || d.state === 'detaching'))) {
    throw conflict('a disk of the VM is being attached or detached; try again once that is done')
  }
  if (desired === 'running') {
    if (vm.template) throw conflict('the VM is a template: clone it, or make it a VM again first')
    const cloning = db.tasks.some(
      (t) => t.kind === 'vm.create' && t.params.cloned_from === vm.id && (t.state === 'pending' || t.state === 'running'),
    )
    if (cloning) throw conflict('a clone of the VM is being made; start it once that is done')
  }
  if (desired) {
    vm.desired_power = desired
    touch(vm)
  }
  const t = startTask({ kind, tenant_id: vm.tenant_id, target_kind: 'vm', target_id: vm.id, params })
  audit(kind, ['vm', vm.id], { name: vm.name, params }, vm.tenant_id)
  return accepted({ task_id: t.id })
}

function taskVm(t: { target_id?: string | null }): VmRec {
  const v = db.vms.find((x) => x.id === t.target_id)
  if (!v) throw new Error('the VM is gone')
  return v
}

driver('vm.start', {
  steps: (t) => {
    const v = db.vms.find((x) => x.id === t.target_id)
    if (v?.power === 'suspended') return [['waking the guest', 1200]]
    if (v?.power === 'paused') return [['resuming', 800]]
    return [['start', 2200]]
  },
  finish: (t) => {
    const v = taskVm(t)
    if (v.power !== 'running') {
      setPower(v, 'running')
      afterBoot(v)
    }
  },
})

driver('vm.stop', {
  steps: (t) => (t.params.force ? [['power off', 900]] : [['shutting down', 3200]]),
  step: (t) => {
    const v = taskVm(t)
    if (!t.params.force && v.power === 'running') setPower(v, 'shutting_down')
  },
  finish: (t) => {
    const v = taskVm(t)
    setPower(v, 'stopped', t.params.force ? 'destroyed' : 'shutdown')
  },
})

driver('vm.reboot', {
  steps: () => [['reboot', 2600]],
  step: (t) => {
    const v = taskVm(t)
    if (v.power !== 'running') throw new Error(`${v.name} is not running`)
  },
  finish: (t) => {
    const v = taskVm(t)
    setPower(v, 'running')
  },
})

route.post('/api/v1/vms/{id}/start', ({ params }) => powerAction(params.id, 'vm.start', 'running', {}))
route.post('/api/v1/vms/{id}/stop', ({ params, query }) =>
  powerAction(params.id, 'vm.stop', 'stopped', { force: !!query.force }),
)
route.post('/api/v1/vms/{id}/reboot', ({ params }) => powerAction(params.id, 'vm.reboot', null, {}))

// ---------------------------------------------------------------------------
// Deleting

driver('vm.delete', {
  steps: () => [
    ['undefine', 900],
    ['volumes', 1400],
    ['network', 600],
  ],
  finish: (t) => {
    const v = taskVm(t)
    const deleteDisks = !!t.params.delete_disks
    const nicIds = new Set(v.nics.map((n) => n.id))
    for (const f of db.floatingIps) {
      if (f.nic_id && nicIds.has(f.nic_id)) {
        f.nic_id = null
        emit('external_ip', f.id)
      }
    }
    for (const lb of db.loadBalancers) {
      if (lb.member_vm_ids.includes(v.id)) {
        lb.member_vm_ids = lb.member_vm_ids.filter((x) => x !== v.id)
        emit('load_balancer', lb.id)
      }
    }
    // Disks made with it go; disks of their own stay, attached to nothing.
    for (const d of [...db.disks]) {
      if (d.vm_id !== v.id) continue
      if (d.boot || !d.name || deleteDisks) {
        remove(db.disks, (x) => x.id === d.id)
        emit('disk', d.id, 'delete')
      } else {
        d.vm_id = null
        d.guest_path = null
        emit('disk', d.id)
      }
    }
    for (const s of db.snapshots.filter((s) => s.vm_id === v.id)) emit('snapshot', s.id, 'delete')
    remove(db.snapshots, (s) => s.vm_id === v.id)
    for (const b of db.backups) {
      if (b.vm_id === v.id) {
        b.vm_id = null
        emit('backup', b.id)
      }
    }
    delete db.schedules[v.id]
    remove(db.vms, (x) => x.id === v.id)
    emit('vm', v.id, 'delete')
    if (v.host_id) emit('host', v.host_id)
    for (const n of new Set(v.nics.map((x) => x.network_id))) emit('network', n)
  },
  failed: (t, error) => {
    const v = db.vms.find((x) => x.id === t.target_id)
    if (!v) return
    v.status = 'error'
    v.error = `deleting it failed: ${error}`
    touch(v)
  },
})

route.delete('/api/v1/vms/{id}', ({ params, query }) => {
  const vm = load(params.id, 'vm.write')
  if (vm.status === 'migrating') throw conflict('VM is being migrated; cancel the migration first')
  if (vm.status === 'deleting') throw conflict('VM is being deleted')
  const busy = disksBusy(vm)
  if (busy) throw conflict(`${busy}; delete the VM once that is done`)
  const deleteDisks = !!query.delete_disks
  vm.status = 'deleting'
  vm.desired_power = 'stopped'
  touch(vm)
  const t = startTask({
    kind: 'vm.delete',
    tenant_id: vm.tenant_id,
    target_kind: 'vm',
    target_id: vm.id,
    params: { delete_disks: deleteDisks },
  })
  audit('vm.delete', ['vm', vm.id], { name: vm.name, delete_disks: deleteDisks }, vm.tenant_id)
  return accepted({ task_id: t.id })
})

// ---------------------------------------------------------------------------
// Live migration

const MIGRATION_STEPS = 5

driver('vm.migrate', {
  steps: (t) => {
    const v = db.vms.find((x) => x.id === t.target_id)
    const total = (v?.memory_mib ?? 4096) / 1024
    const steps: [string, number][] = [['preparing the destination', 1500]]
    for (let i = 0; i < MIGRATION_STEPS; i++) {
      const done = (total * i) / MIGRATION_STEPS
      steps.push([`copying: ${done.toFixed(1)} GiB of ${total.toFixed(1)} GiB`, 1300])
    }
    steps.push(['finishing', 1200])
    return steps
  },
  step: (t, i) => {
    const v = taskVm(t)
    if (!v.migration) throw new Error('the migration was undone')
    if (v.migration.cancel_requested) throw new Error('the migration was cancelled')
    v.migration.state = i === 0 ? 'preparing' : i <= MIGRATION_STEPS ? 'running' : 'committing'
    emit('vm', v.id)
  },
  finish: (t) => {
    const v = taskVm(t)
    const m = v.migration
    if (!m) throw new Error('the migration was undone')
    const from = v.host_id
    v.host_id = m.dest_host_id
    for (const d of db.disks) {
      if (d.vm_id === v.id && d.host_id) {
        d.host_id = m.dest_host_id
        emit('disk', d.id)
      }
    }
    v.migration = null
    v.status = 'ready'
    touch(v)
    if (from) emit('host', from)
    emit('host', m.dest_host_id)
  },
  failed: (t) => {
    const v = db.vms.find((x) => x.id === t.target_id)
    if (!v) return
    const dest = v.migration?.dest_host_id
    v.migration = null
    if (v.status === 'migrating') v.status = 'ready'
    touch(v)
    if (dest) emit('host', dest)
  },
})

route.post('/api/v1/vms/{id}/migrate', ({ params, body }) => {
  const vm = load(params.id, 'vm.read')
  authorize('infra.write', homeTenant())
  const want = body.host_id ?? null
  if (vm.status !== 'ready') throw conflict(`${vm.name} is busy (${vm.status})`)
  const busy = disksBusy(vm)
  if (busy) throw conflict(`${vm.name}: ${busy}`)
  if (vm.disks.some((d) => d.pool_id === LOCAL_POOL) && db.snapshots.some((s) => s.vm_id === vm.id)) {
    throw conflict(
      `${vm.name} has snapshots on its host's local disk, which would not move with it; delete them first`,
    )
  }
  if (!vm.migratable) throw conflict(`${vm.name} is not migratable: it uses its host's full CPU and disk caching`)
  if (vm.power !== 'running') throw conflict(`${vm.name} is not running; only running VMs can be live-migrated`)
  if (!vm.host_id) throw conflict(`${vm.name} is not on a host`)
  if (want === vm.host_id) throw conflict(`${vm.name} already runs there`)
  const src = db.hosts.find((h) => h.id === vm.host_id)
  if (!src) throw conflict("the VM's host no longer exists")
  if (!hostLive(src)) throw conflict(`${src.name} is not connected`)
  if (!src.cluster_id) throw conflict(`${src.name} is in no cluster; VMs migrate within their host's cluster`)
  const rejected: string[] = []
  const candidates = db.hosts.filter((h) => {
    if (h.id === src.id || (want && h.id !== want) || h.state === 'decommissioned') return false
    if (h.cluster_id !== src.cluster_id) {
      rejected.push(`${h.name}: in another cluster`)
      return false
    }
    if (!h.migration_ready) {
      rejected.push(`${h.name}: does not accept migrations (QEMU's TLS is not set up)`)
      return false
    }
    return true
  })
  if (want && !db.hosts.some((h) => h.id === want)) throw notFound('host')
  let dest: HostRec
  try {
    dest = pick(candidates, vm.max_vcpus, vm.max_memory_mib, { tpm: vm.tpm, secure_boot: vm.secure_boot })
  } catch (e) {
    const why = [...rejected, ...(candidates.length && e instanceof Error ? [e.message] : [])]
    throw conflict(why.length ? `cannot migrate ${vm.name}: ${why.join('; ')}` : `no other host can take ${vm.name}`)
  }
  const t = startTask({
    kind: 'vm.migrate',
    tenant_id: vm.tenant_id,
    target_kind: 'vm',
    target_id: vm.id,
    params: { dest_host_id: dest.id },
  })
  vm.status = 'migrating'
  vm.migration = {
    task_id: t.id,
    dest_host_id: dest.id,
    state: 'preparing',
    cancel_requested: false,
    started_at: now(),
  }
  touch(vm)
  emit('host', dest.id)
  audit('vm.migrate', ['vm', vm.id], { name: vm.name, from: vm.host_id, to: dest.id, to_name: dest.name }, vm.tenant_id)
  return accepted({ task_id: t.id })
})

route.post('/api/v1/vms/{id}/migrate/cancel', ({ params }) => {
  const vm = load(params.id, 'vm.read')
  authorize('infra.write', homeTenant())
  if (!vm.migration) throw conflict('the VM is not being migrated')
  vm.migration.cancel_requested = true
  touch(vm)
  audit('vm.migrate.cancel', ['vm', vm.id], { name: vm.name }, vm.tenant_id)
  return none()
})

// ---------------------------------------------------------------------------
// Network interfaces

const MAX_NICS = 8

function nicsChangeable(vm: VmRec) {
  if (vm.status === 'migrating') throw conflict('the VM is being migrated; change its interfaces once it is done')
  if (vm.status !== 'ready') throw conflict(`the VM is ${vm.status}; its interfaces cannot change now`)
  const t = activeTask(vm.id)
  if (t) throw conflict(`a task (${t.kind}) is working on the VM; change its interfaces once it is done`)
  const d = db.disks.find((x) => x.vm_id === vm.id && x.state !== 'ready' && x.state !== 'failed')
  if (d) throw conflict(`a disk of the VM is ${d.state}; change its interfaces once that is done`)
}

route.put('/api/v1/vms/{id}/nics/{nic}/security-groups', ({ params, body }) => {
  const vm = load(params.id, 'vm.write')
  const nic = vm.nics.find((n) => n.id === params.nic)
  if (!nic) throw notFound('NIC')
  const groups = checkGroups(vm.tenant_id, body.security_group_ids)
  nic.security_group_ids = groups
  touch(vm)
  for (const g of groups) emit('security_group', g)
  audit('vm.nic.security_groups', ['vm', vm.id], { nic: nic.id, security_group_ids: groups }, vm.tenant_id)
  return vmView(vm)
})

route.post('/api/v1/vms/{id}/nics', ({ params, body }) => {
  const vm = load(params.id, 'vm.write')
  nicsChangeable(vm)
  if (vm.nics.length >= MAX_NICS) throw conflict(`a VM has at most ${MAX_NICS} interfaces`)
  const network = tenantNetwork(vm.tenant_id, body.network_id)
  const groups = body.security_group_ids ? checkGroups(vm.tenant_id, body.security_group_ids) : [defaultGroup(vm.tenant_id)]
  const nic: NicRec = { id: newId(), network_id: network.id, ip: nextIp(network), mac: newMac(), security_group_ids: groups }
  vm.nics.push(nic)
  touch(vm)
  emit('network', network.id)
  audit(
    'vm.nic.add',
    ['vm', vm.id],
    { name: vm.name, nic: nic.id, network: network.name, ip: nic.ip, security_group_ids: groups },
    vm.tenant_id,
  )
  return created(vmView(vm))
})

route.patch('/api/v1/vms/{id}/nics/{nic}', ({ params, body }) => {
  const vm = load(params.id, 'vm.write')
  nicsChangeable(vm)
  const nic = vm.nics.find((n) => n.id === params.nic)
  if (!nic) throw notFound('NIC')
  const network = tenantNetwork(vm.tenant_id, body.network_id)
  if (nic.network_id === network.id) return vmView(vm)
  const fip = db.floatingIps.find((f) => f.nic_id === nic.id)
  if (fip) {
    const vpc = db.vpcs.find((v) => v.id === network.vpc_id)
    if (vpc?.external_network_id !== fip.external_network_id) {
      throw conflict(`the interface has a floating IP, which VPC ${vpc?.name ?? ''} does not route to; detach it first`)
    }
  }
  const otherVpc = db.loadBalancers.some((lb) => lb.member_vm_ids.includes(vm.id) && lb.vpc_id !== network.vpc_id)
  if (otherVpc) {
    throw conflict('the interface is a member of a load balancer in another VPC; remove the VM from it first')
  }
  const from = nic.network_id
  nic.network_id = network.id
  nic.ip = nextIp(network)
  touch(vm)
  emit('network', from)
  emit('network', network.id)
  audit('vm.nic.move', ['vm', vm.id], { name: vm.name, nic: nic.id, from, to: network.name, ip: nic.ip }, vm.tenant_id)
  return vmView(vm)
})

route.delete('/api/v1/vms/{id}/nics/{nic}', ({ params }) => {
  const vm = load(params.id, 'vm.write')
  nicsChangeable(vm)
  const at = vm.nics.findIndex((n) => n.id === params.nic)
  if (at < 0) throw notFound('NIC')
  if (vm.nics.length === 1) throw conflict('a VM keeps at least one interface; move it to another network instead')
  const [gone] = vm.nics.splice(at, 1)
  for (const f of db.floatingIps) {
    if (f.nic_id === gone!.id) {
      f.nic_id = null
      emit('external_ip', f.id)
    }
  }
  touch(vm)
  emit('network', gone!.network_id)
  audit('vm.nic.remove', ['vm', vm.id], { name: vm.name, nic: gone!.id, ip: gone!.ip }, vm.tenant_id)
  return vmView(vm)
})

// ---------------------------------------------------------------------------
// Consoles

/** Console tickets handed out: the VM and the kind of console. */
export const consoleTickets = new Map<string, { vm_id: string; kind: S['Kind']; expires: number }>()

/** The VM (and kind) a console ticket is for, while it is valid. */
export function vmForTicket(ticket: string): { vm: VmRec; kind: S['Kind'] } | undefined {
  const t = consoleTickets.get(ticket)
  if (!t) return undefined
  consoleTickets.delete(ticket)
  const vm = db.vms.find((v) => v.id === t.vm_id)
  if (!vm || t.expires < Date.now()) return undefined
  return { vm, kind: t.kind }
}

route.post('/api/v1/vms/{id}/console', ({ params, body }) => {
  const vm = load(params.id, 'vm.console')
  if (vm.power !== 'running') throw conflict('VM is not running')
  const ticket = newId().replace(/-/g, '')
  consoleTickets.set(ticket, { vm_id: vm.id, kind: body.kind, expires: Date.now() + 30_000 })
  return { ticket, path: `/api/v1/console/${ticket}` }
})

// ---------------------------------------------------------------------------
// Tasks

route.get('/api/v1/tasks', ({ query }) => {
  me()
  const tenant = query.tenant_id ?? homeTenant()
  find(db.tenants, tenant, 'tenant')
  authorize('task.read', tenant)
  const ids = subtree(tenant)
  const limit = Math.min(500, Math.max(1, query.limit ?? 50))
  return db.tasks
    .filter((t) => ids.has(t.tenant_id) && (!query.target_id || t.target_id === query.target_id))
    .sort((a, b) => b.created_at.localeCompare(a.created_at))
    .slice(0, limit)
    .map(taskView)
})

route.get('/api/v1/tasks/{id}', ({ params }) => {
  const t = find(db.tasks, params.id, 'task')
  authorize('task.read', t.tenant_id)
  return taskView(t)
})
