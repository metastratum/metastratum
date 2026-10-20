// What the demo starts with: a provider running two clusters, and a few
// customer tenants with networks and VMs of their own. Built afresh (with
// new IDs, dated from now) for each visitor, and on Reset.
import type { components } from '../api/schema'
import type {
  BackupRec,
  Db,
  DiskRec,
  ImageRec,
  NetworkRec,
  NicRec,
  TaskRec,
  TenantRec,
  VmRec,
} from './store'
import { newId } from './ids'

type S = components['schemas']

/** Bump when the seed changes: visitors' kept data is replaced. */
export const SEED_VERSION = 1

export const LOCAL_POOL = '00000000-0000-0000-0000-000000000001'
const GIB = 2 ** 30
const ago = (days: number, hours = 0) => new Date(Date.now() - (days * 24 + hours) * 3600_000).toISOString()

export function seed(): Db {
  const t0 = ago(120)

  // Tenants: the provider, and its customers.
  const tenant = (name: string, slug: string, parent: TenantRec | null, age: number): TenantRec => ({
    id: newId(),
    name,
    slug,
    parent_id: parent?.id ?? null,
    slug_path: parent ? (parent.slug_path ? `${parent.slug_path}/${slug}` : slug) : '',
    depth: parent ? parent.depth + 1 : 1,
    domains: [],
    created_at: ago(age),
  })
  const provider = tenant('Stratus Hosting', 'provider', null, 120)
  const northwind = tenant('Northwind Traders', 'northwind', provider, 90)
  const nwDev = tenant('Northwind Dev', 'dev', northwind, 60)
  const globex = tenant('Globex', 'globex', provider, 45)
  northwind.domains = ['cloud.northwind.example']
  const tenants = [provider, northwind, nwDev, globex]

  // People. Everyone's password in the demo is "demo".
  const user = (username: string, display_name: string, t: TenantRec, age: number) => ({
    id: newId(),
    username,
    display_name,
    email: `${username}@${t === provider ? 'stratus.example' : `${t.slug}.example`}`,
    tenant_id: t.id,
    local: true,
    disabled: false,
    created_at: ago(age),
    password: 'demo',
  })
  const admin = user('admin', 'Dana Admin', provider, 120)
  const ops = user('oscar', 'Oscar Operations', provider, 100)
  const alice = user('alice', 'Alice Northwind', northwind, 90)
  const dev = user('devon', 'Devon Developer', nwDev, 58)
  const gina = user('gina', 'Gina Globex', globex, 44)
  const users = [admin, ops, alice, dev, gina]
  const bind = (u: { id: string }, t: TenantRec, role: string) => ({
    id: newId(),
    user_id: u.id,
    tenant_id: t.id,
    role,
    source: 'manual',
    created_at: ago(30),
  })
  const bindings = [
    bind(admin, provider, 'admin'),
    bind(ops, provider, 'operator'),
    bind(alice, northwind, 'admin'),
    bind(dev, nwDev, 'operator'),
    bind(dev, northwind, 'viewer'),
    bind(gina, globex, 'admin'),
  ]

  // Clusters and hosts.
  const cluster = (name: string, ha: boolean, age: number): S['ClusterView'] => ({
    id: newId(),
    name,
    cpu_overcommit: 4,
    memory_overcommit: 1,
    ha_enabled: ha,
    vm_net_mbps: null,
    vm_disk_iops: ha ? 20000 : null,
    vm_disk_mbps: null,
    created_at: ago(age),
  })
  const ams = cluster('ams-prod', true, 118)
  const rtm = cluster('rtm-general', false, 70)
  const host = (name: string, c: S['ClusterView'], i: number, cpus: number, memGib: number) => ({
    id: newId(),
    name,
    address: `10.20.${c === ams ? 1 : 2}.${10 + i}`,
    state: 'ready',
    cluster_id: c.id,
    connected: true,
    migration_ready: true,
    controller: 'ctl-1',
    connected_since: ago(6),
    fence_kind: c.ha_enabled ? 'redfish' : null,
    cpu_count: cpus,
    memory_mib: memGib * 1024,
    cpu_model: c === ams ? 'AMD EPYC 9354P 32-Core Processor' : 'Intel(R) Xeon(R) Gold 6430',
    cpu_vendor: c === ams ? 'AMD' : 'Intel',
    kvm: true,
    tpm: true,
    secure_boot: true,
    libvirt_version: '11.3.0',
    ovn_chassis: name,
    physical_networks: ['external'],
    agent_version: '0.6.0',
    cert_expires_at: new Date(Date.now() + 24 * 86400_000).toISOString(),
    last_seen_at: new Date().toISOString(),
    pending_facts: null,
    cannot_run: [],
  })
  const hosts = [
    host('ams-hv01', ams, 1, 64, 128),
    host('ams-hv02', ams, 2, 64, 128),
    host('ams-hv03', ams, 3, 64, 128),
    host('rtm-hv01', rtm, 1, 48, 96),
    host('rtm-hv02', rtm, 2, 48, 96),
  ]
  const [h1, h2, h3, r1, r2] = hosts as [typeof hosts[0], typeof hosts[0], typeof hosts[0], typeof hosts[0], typeof hosts[0]]
  const fencing: Db['fencing'] = Object.fromEntries(
    [h1, h2, h3].map((h, i) => [
      h.id,
      { kind: 'redfish', config: { url: `https://10.20.0.${21 + i}`, username: 'fence', password: '(set)' } },
    ]),
  )

  // Storage.
  const pools = [
    { id: LOCAL_POOL, name: 'local', kind: 'local', config: {}, cluster_ids: [], created_at: t0 },
    {
      id: newId(),
      name: 'ceph-ssd',
      kind: 'rbd',
      config: { monitors: ['10.20.0.31:6789', '10.20.0.32:6789', '10.20.0.33:6789'], pool: 'vms', user: 'metastratum', key: '(set)' },
      cluster_ids: [ams.id],
      created_at: ago(117),
    },
    {
      id: newId(),
      name: 'nfs-bulk',
      kind: 'nfs',
      config: { server: '10.20.0.40', export: '/export/vms' },
      cluster_ids: [ams.id, rtm.id],
      created_at: ago(69),
    },
  ]
  const [, ceph, nfs] = pools as [typeof pools[0], typeof pools[0], typeof pools[0]]
  const repo = { id: newId(), name: 'nas-backups', kind: 'nfs', server: '10.20.0.50', export: '/export/backups', created_at: ago(100) }

  // The internet, as a provider network.
  const internet = {
    id: newId(),
    name: 'internet',
    physical_network: 'external',
    cidr: '203.0.113.0/24',
    gateway: '203.0.113.1',
    pool_start: '203.0.113.10',
    pool_end: '203.0.113.250',
    created_at: ago(116),
  }

  // Images, published by the provider for everyone.
  const image = (name: string, opts: Partial<ImageRec> & { gib: number; virtualGib: number }, age: number): ImageRec => ({
    id: newId(),
    tenant_id: provider.id,
    name,
    origin: 'download',
    format: 'qcow2',
    os: 'linux',
    shared: true,
    secure_boot: false,
    tpm: false,
    sleep: true,
    admin_user: null,
    created_at: ago(age),
    state: 'ready',
    error: null,
    size_bytes: Math.round(opts.gib * GIB),
    virtual_bytes: opts.virtualGib * GIB,
    sha256: [...crypto.getRandomValues(new Uint8Array(32))].map((b) => b.toString(16).padStart(2, '0')).join(''),
    digest: [...crypto.getRandomValues(new Uint8Array(32))].map((b) => b.toString(16).padStart(2, '0')).join(''),
    file_bytes: null,
    received_bytes: null,
    hosts: hosts.map((h) => h.name),
    storage_pool_id: LOCAL_POOL,
    ...opts,
  })
  const debian = image('debian-13', { gib: 0.42, virtualGib: 3 }, 115)
  const ubuntu = image('ubuntu-24.04', { gib: 0.6, virtualGib: 3.5 }, 112)
  const rocky = image('rocky-10', { gib: 0.55, virtualGib: 10 }, 80)
  const winServer = image(
    'windows-server-2025',
    { gib: 7.1, virtualGib: 40, os: 'windows', admin_user: 'Administrator', origin: 'upload', hosts: [h1.name, h2.name, h3.name] },
    40,
  )
  const win11 = image(
    'windows-11',
    { gib: 7.8, virtualGib: 64, os: 'windows', secure_boot: true, tpm: true, origin: 'upload', hosts: [h1.name, h2.name] },
    20,
  )
  const images = [debian, ubuntu, rocky, winServer, win11]

  // Networks: a default VPC for each tenant, and Northwind's own.
  const vpcs: Db['vpcs'] = []
  const networks: NetworkRec[] = []
  const groups: Db['groups'] = []
  const rules: Db['rules'] = []
  const vpc = (t: TenantRec, name: string, external: boolean, age: number) => {
    const v = {
      id: newId(),
      tenant_id: t.id,
      name,
      external_network_id: external ? internet.id : null,
      gateway_ip: external ? `203.0.113.${10 + vpcs.length}` : null,
      created_at: ago(age),
    }
    vpcs.push(v)
    return v
  }
  const network = (v: { id: string; tenant_id: string }, name: string, prefix: string, age: number) => {
    const n = { id: newId(), tenant_id: v.tenant_id, vpc_id: v.id, name, cidr: `${prefix}.0/24`, gateway: `${prefix}.1`, created_at: ago(age) }
    networks.push(n)
    return n
  }
  const defaultGroup = (t: TenantRec, age: number) => {
    const g = {
      id: newId(),
      tenant_id: t.id,
      name: 'default',
      description: 'Allows all outgoing traffic, and incoming traffic from other members',
      created_at: ago(age),
    }
    groups.push(g)
    rules.push(
      { id: newId(), group_id: g.id, direction: 'egress', protocol: 'any', port_min: null, port_max: null, remote_cidr: '0.0.0.0/0', remote_group_id: null, description: '' },
      { id: newId(), group_id: g.id, direction: 'ingress', protocol: 'any', port_min: null, port_max: null, remote_cidr: null, remote_group_id: g.id, description: '' },
    )
    return g
  }
  const nets: Record<string, NetworkRec> = {}
  const sgs: Record<string, { id: string }> = {}
  for (const [t, age, prefix] of [
    [provider, 118, '10.0.0'],
    [northwind, 90, '10.0.0'],
    [nwDev, 60, '10.0.0'],
    [globex, 45, '10.0.0'],
  ] as const) {
    const v = vpc(t, 'default', t === globex, age)
    nets[t.slug] = network(v, 'default', prefix, age)
    sgs[t.slug] = defaultGroup(t, age)
  }
  const shop = vpc(northwind, 'webshop', true, 85)
  const front = network(shop, 'frontend', '10.10.1', 85)
  const back = network(shop, 'backend', '10.10.2', 85)
  const web = {
    id: newId(),
    tenant_id: northwind.id,
    name: 'web',
    description: 'HTTP and HTTPS from anywhere; SSH from the office',
    created_at: ago(85),
  }
  groups.push(web)
  rules.push(
    { id: newId(), group_id: web.id, direction: 'ingress', protocol: 'tcp', port_min: 80, port_max: 80, remote_cidr: '0.0.0.0/0', remote_group_id: null, description: 'HTTP' },
    { id: newId(), group_id: web.id, direction: 'ingress', protocol: 'tcp', port_min: 443, port_max: 443, remote_cidr: '0.0.0.0/0', remote_group_id: null, description: 'HTTPS' },
    { id: newId(), group_id: web.id, direction: 'ingress', protocol: 'tcp', port_min: 22, port_max: 22, remote_cidr: '198.51.100.0/24', remote_group_id: null, description: 'SSH from the office' },
  )
  const dbGroup = {
    id: newId(),
    tenant_id: northwind.id,
    name: 'database',
    description: 'PostgreSQL from the web servers',
    created_at: ago(85),
  }
  groups.push(dbGroup)
  rules.push({
    id: newId(),
    group_id: dbGroup.id,
    direction: 'ingress',
    protocol: 'tcp',
    port_min: 5432,
    port_max: 5432,
    remote_cidr: null,
    remote_group_id: web.id,
    description: 'PostgreSQL',
  })

  // VMs, their disks and interfaces.
  const vms: VmRec[] = []
  const disks: DiskRec[] = []
  const macs = new Set<string>()
  const mac = () => {
    for (;;) {
      const m = `02:${[...crypto.getRandomValues(new Uint8Array(5))].map((b) => b.toString(16).padStart(2, '0')).join(':')}`
      if (!macs.has(m)) return macs.add(m) && m
    }
  }
  const ips = new Map<string, number>()
  const nic = (n: NetworkRec, groupIds: string[]): NicRec => {
    const last = (ips.get(n.id) ?? 9) + 1
    ips.set(n.id, last)
    return { id: newId(), network_id: n.id, ip: n.cidr.replace(/0\/24$/, String(last)), mac: mac(), security_group_ids: groupIds }
  }
  type VmOpts = {
    t: TenantRec
    host: typeof h1
    img: ImageRec
    vcpus: number
    mem: number
    gib: number
    pool?: string
    nics: NicRec[]
    power?: string
    age: number
    extra?: Partial<VmRec>
  }
  const vm = (name: string, o: VmOpts): VmRec => {
    const id = newId()
    const pool = o.pool ?? LOCAL_POOL
    const volume = newId()
    const power = o.power ?? 'running'
    const v: VmRec = {
      id,
      tenant_id: o.t.id,
      name,
      host_id: o.host.id,
      status: 'ready',
      template: false,
      desired_power: power === 'stopped' ? 'stopped' : 'running',
      power,
      power_reason: power === 'stopped' ? 'shutdown' : null,
      power_observed_at: ago(0, o.age % 24),
      error: null,
      vcpus: o.vcpus,
      max_vcpus: o.vcpus,
      cpu_hotplug: false,
      memory_mib: o.mem,
      max_memory_mib: o.mem,
      free_page_reporting: true,
      firmware: 'uefi',
      secure_boot: o.img.secure_boot,
      tpm: o.img.tpm,
      sleep: o.img.sleep,
      guest_os: o.img.os === 'windows' ? 'windows' : 'linux',
      first_boot_pending: false,
      account: o.img.os === 'windows' ? (o.img.admin_user ?? 'user') : null,
      migratable: true,
      cpu_model: o.host.cpu_vendor === 'AMD' ? 'EPYC-Genoa' : 'SapphireRapids',
      disks: [{ volume_id: volume, pool_id: pool, size_bytes: o.gib * GIB, image_id: o.img.id }],
      nics: o.nics,
      migration: null,
      created_at: ago(o.age),
      updated_at: ago(Math.max(0, o.age - 3)),
      ...o.extra,
    }
    vms.push(v)
    disks.push({
      id: volume,
      tenant_id: o.t.id,
      name: null,
      size_gib: o.gib,
      storage_pool_id: pool,
      host_id: pool === LOCAL_POOL ? o.host.id : null,
      vm_id: id,
      boot: true,
      guest_path: null,
      state: 'ready',
      error: null,
      created_at: ago(o.age),
    })
    return v
  }
  const web1 = vm('web-1', { t: northwind, host: h1, img: debian, vcpus: 4, mem: 8192, gib: 40, pool: ceph.id, nics: [nic(front, [web.id, sgs.northwind!.id])], age: 84 })
  const web2 = vm('web-2', { t: northwind, host: h2, img: debian, vcpus: 4, mem: 8192, gib: 40, pool: ceph.id, nics: [nic(front, [web.id, sgs.northwind!.id])], age: 84 })
  const pg = vm('db-1', {
    t: northwind,
    host: h3,
    img: debian,
    vcpus: 8,
    mem: 32768,
    gib: 100,
    pool: ceph.id,
    nics: [nic(back, [dbGroup.id, sgs.northwind!.id])],
    age: 83,
    extra: { max_memory_mib: 65536, max_vcpus: 16, cpu_hotplug: true },
  })
  vm('ci-runner', { t: nwDev, host: r1, img: ubuntu, vcpus: 8, mem: 16384, gib: 80, nics: [nic(nets.dev!, [sgs.dev!.id])], age: 50 })
  vm('scratch', { t: nwDev, host: r2, img: rocky, vcpus: 2, mem: 4096, gib: 20, nics: [nic(nets.dev!, [sgs.dev!.id])], power: 'stopped', age: 20 })
  vm('app-1', { t: globex, host: h2, img: ubuntu, vcpus: 4, mem: 8192, gib: 50, pool: nfs.id, nics: [nic(nets.globex!, [sgs.globex!.id])], age: 40 })
  vm('ad-dc', { t: globex, host: h1, img: winServer, vcpus: 4, mem: 8192, gib: 80, pool: ceph.id, nics: [nic(nets.globex!, [sgs.globex!.id])], age: 38 })
  vm('desk-gina', {
    t: globex,
    host: h1,
    img: win11,
    vcpus: 4,
    mem: 8192,
    gib: 64,
    nics: [nic(nets.globex!, [sgs.globex!.id])],
    power: 'suspended',
    age: 12,
    extra: { account: 'gina', migratable: false, cpu_model: null },
  })
  vm('monitoring', { t: provider, host: h3, img: debian, vcpus: 2, mem: 4096, gib: 30, nics: [nic(nets.provider!, [sgs.provider!.id])], age: 110 })
  vm('debian-base', {
    t: provider,
    host: r1,
    img: debian,
    vcpus: 2,
    mem: 2048,
    gib: 10,
    pool: nfs.id,
    nics: [nic(nets.provider!, [sgs.provider!.id])],
    power: 'stopped',
    age: 30,
    extra: { template: true },
  })
  // A data disk on its own, and one attached.
  disks.push(
    {
      id: newId(),
      tenant_id: northwind.id,
      name: 'pg-wal',
      size_gib: 50,
      storage_pool_id: ceph.id,
      host_id: null,
      vm_id: pg.id,
      boot: false,
      guest_path: `/dev/disk/by-id/scsi-0QEMU_QEMU_HARDDISK_pgwal`,
      state: 'ready',
      error: null,
      created_at: ago(80),
    },
    {
      id: newId(),
      tenant_id: northwind.id,
      name: 'uploads-archive',
      size_gib: 200,
      storage_pool_id: nfs.id,
      host_id: null,
      vm_id: null,
      boot: false,
      guest_path: null,
      state: 'ready',
      error: null,
      created_at: ago(33),
    },
  )
  pg.disks.push({ volume_id: disks.at(-2)!.id, pool_id: ceph.id, size_bytes: 50 * GIB, unit: 1 })

  // Addresses, a load balancer and guardrails.
  const floatingIps: Db['floatingIps'] = [
    { id: newId(), tenant_id: northwind.id, external_network_id: internet.id, ip: '203.0.113.40', nic_id: web1.nics[0]!.id, created_at: ago(84) },
    { id: newId(), tenant_id: northwind.id, external_network_id: internet.id, ip: '203.0.113.41', nic_id: null, created_at: ago(10) },
  ]
  const loadBalancers: Db['loadBalancers'] = [
    {
      id: newId(),
      tenant_id: northwind.id,
      vpc_id: shop.id,
      network_id: front.id,
      name: 'shop',
      ip: '203.0.113.42',
      external: true,
      health_check: true,
      listeners: [
        { protocol: 'tcp', port: 80, target_port: 80 },
        { protocol: 'tcp', port: 443, target_port: 443 },
      ],
      member_vm_ids: [web1.id, web2.id],
      created_at: ago(80),
    },
  ]
  const policies: Db['policies'] = [
    {
      id: newId(),
      tenant_id: provider.id,
      direction: 'egress',
      protocol: 'tcp',
      port_min: 25,
      port_max: 25,
      remote_cidr: '0.0.0.0/0',
      description: 'No outgoing mail: use the relay',
      created_at: ago(100),
    },
    {
      id: newId(),
      tenant_id: northwind.id,
      direction: 'ingress',
      protocol: 'tcp',
      port_min: 3389,
      port_max: 3389,
      remote_cidr: '0.0.0.0/0',
      description: 'No Remote Desktop from outside',
      created_at: ago(60),
    },
  ]

  // Snapshots and backups of the database.
  const snapshots: Db['snapshots'] = [
    { id: newId(), tenant_id: northwind.id, vm_id: pg.id, name: 'before-upgrade', state: 'ready', consistent: true, error: null, created_at: ago(14) },
  ]
  const backups: BackupRec[] = []
  let parent: string | null = null
  for (let d = 13; d >= 0; d--) {
    const full = d % 7 === 6
    const b: BackupRec = {
      id: newId(),
      tenant_id: northwind.id,
      vm_id: pg.id,
      vm_name: pg.name,
      repository_id: repo.id,
      kind: full ? 'full' : 'incremental',
      parent_id: full ? null : parent,
      full_reason: full ? 'every 7th' : null,
      state: 'ready',
      scheduled: true,
      consistent: true,
      vcpus: pg.vcpus,
      memory_mib: pg.memory_mib,
      disks: [0, 1],
      size_bytes: 150 * GIB,
      stored_bytes: full ? 38 * GIB : Math.round((0.8 + Math.random()) * GIB),
      error: null,
      created_at: ago(d, 21),
      completed_at: ago(d, 20.6),
    }
    backups.push(b)
    parent = b.id
  }
  const schedules: Db['schedules'] = { [pg.id]: { every: 'daily', keep: 14, full_every: 7, repository_id: repo.id } }

  // Some history.
  const done = (kind: string, t: TenantRec, target: [string, string], d: number, h: number): TaskRec => ({
    id: newId(),
    kind,
    tenant_id: t.id,
    target_kind: target[0],
    target_id: target[1],
    state: 'succeeded',
    step: null,
    progress: 1,
    error: null,
    created_at: ago(d, h),
    started_at: ago(d, h),
    finished_at: ago(d, h - 0.01),
    params: {},
    at: 0,
  })
  const tasks = [
    done('backup.create', northwind, ['vm', pg.id], 0, 20.6),
    done('vm.migrate', northwind, ['vm', web2.id], 1, 3),
    done('vm.start', globex, ['vm', vms.find((v) => v.name === 'desk-gina')!.id], 2, 5),
  ]
  const audit: Db['audit'] = [
    { id: 3, at: ago(1, 3), action: 'vm.migrate', outcome: 'success', details: { name: 'web-2', to: h2.name }, target_kind: 'vm', target_id: web2.id, tenant_id: northwind.id, user_id: alice.id, username: alice.username },
    { id: 2, at: ago(10), action: 'floating_ip.allocate', outcome: 'success', details: { ip: '203.0.113.41' }, target_kind: 'floating_ip', target_id: floatingIps[1]!.id, tenant_id: northwind.id, user_id: alice.id, username: alice.username },
    { id: 1, at: ago(12), action: 'vm.create', outcome: 'success', details: { name: 'desk-gina' }, target_kind: 'vm', target_id: vms.find((v) => v.name === 'desk-gina')!.id, tenant_id: globex.id, user_id: gina.id, username: gina.username },
  ]

  return {
    version: SEED_VERSION,
    session: { user_id: admin.id, tenant_id: provider.id },
    tenants,
    quotas: {
      [northwind.id]: { vcpus: 64, memory_mib: 262144, disk_gib: 4096, floating_ips: 8 },
      [nwDev.id]: { vcpus: 16, memory_mib: 65536 },
      [globex.id]: { vcpus: 32, memory_mib: 131072, disk_gib: 2048, floating_ips: 4 },
    },
    grants: {
      [northwind.id]: { clusters: [ams.id, rtm.id], storage_pools: [ceph.id, nfs.id], external_networks: [internet.id], backup_repositories: [repo.id] },
      [nwDev.id]: { clusters: [rtm.id], storage_pools: [nfs.id], external_networks: [], backup_repositories: [] },
      [globex.id]: { clusters: [ams.id], storage_pools: [ceph.id, nfs.id], external_networks: [internet.id], backup_repositories: [repo.id] },
    },
    identityProviders: {},
    twoFactorRequired: [],
    users,
    bindings,
    tokens: [],
    factors: [],
    backupCodes: {},
    audit,
    hosts,
    fencing,
    clusters: [ams, rtm],
    pools,
    repositories: [repo],
    externalNetworks: [internet],
    images,
    vpcs,
    networks,
    groups,
    rules,
    floatingIps,
    loadBalancers,
    policies,
    vms,
    disks,
    snapshots,
    backups,
    schedules,
    tasks,
  }
}
