// The demo backend answers the whole API: every list and record for each
// kind of user, and the main things visitors do, with tasks run to the end.
import { afterAll, beforeAll, describe, expect, it, vi } from 'vitest'
import './handlers'
import { handle } from './http'
import { db } from './store'

beforeAll(() => {
  vi.useFakeTimers()
})
afterAll(() => {
  vi.useRealTimers()
})

type Answer = { status: number; body: unknown }

/** Calls the API as the app would, letting the demo's delays pass. */
async function call(method: string, path: string, body?: unknown): Promise<Answer> {
  const req = new Request(`http://demo.test/api/v1${path}`, {
    method,
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  })
  const pending = handle(req)
  await vi.advanceTimersByTimeAsync(500)
  const res = await pending
  const text = await res.text()
  return { status: res.status, body: text ? JSON.parse(text) : undefined }
}

async function ok<T = Record<string, unknown>>(method: string, path: string, body?: unknown): Promise<T> {
  const r = await call(method, path, body)
  if (r.status >= 300) throw new Error(`${method} ${path}: ${r.status} ${JSON.stringify(r.body)}`)
  return r.body as T
}

async function signIn(username: string, tenant = '', password = 'demo') {
  await ok('POST', '/auth/logout')
  await ok('POST', '/auth/login', { username, password, tenant })
}

/** Runs tasks to the end; fails when one did. */
async function settle(taskId?: string) {
  for (let i = 0; i < 120; i++) {
    await vi.advanceTimersByTimeAsync(1000)
    const active = db.tasks.filter((t) => t.state === 'pending' || t.state === 'running')
    if (!active.length) break
  }
  if (taskId) {
    const t = db.tasks.find((x) => x.id === taskId)!
    expect(t.error ?? null, `${t.kind}: ${t.error}`).toBeNull()
    expect(t.state).toBe('succeeded')
  }
}

const LISTS = [
  '/vms?descendants=true',
  '/disks',
  '/backups',
  '/images',
  '/tenants',
  '/vpcs?descendants=true',
  '/networks?descendants=true',
  '/security-groups?descendants=true',
  '/floating-ips?descendants=true',
  '/load-balancers?descendants=true',
  '/tasks',
  '/audit',
  '/users?descendants=true',
  '/tokens',
  '/auth/me',
  '/auth/site',
]
const INFRA = ['/hosts', '/clusters', '/storage-pools', '/backup-repositories', '/external-networks']

describe('demo backend', () => {
  it('answers every list and record for the provider', async () => {
    await signIn('admin')
    for (const p of [...LISTS, ...INFRA]) await ok('GET', p)
    for (const t of db.tenants) {
      for (const sub of ['', '/quotas', '/grants', '/clusters', '/storage-pools', '/external-networks', '/backup-repositories', '/role-bindings', '/network-policies', '/sign-in', '/two-factor-policy']) {
        await ok('GET', `/tenants/${t.id}${sub}`)
      }
    }
    for (const v of db.vms) {
      for (const sub of ['', '/disks', '/snapshots', '/backups']) await ok('GET', `/vms/${v.id}${sub}`)
      if (v.power === 'running') await ok('GET', `/vms/${v.id}/memory`)
    }
    for (const h of db.hosts) await ok('GET', `/hosts/${h.id}/fencing`)
    for (const u of db.users) {
      await ok('GET', `/users/${u.id}/role-bindings`)
      await ok('GET', `/users/${u.id}/two-factor`)
    }
  })

  it('answers tenants their own, and refuses them the infrastructure', async () => {
    await signIn('alice', 'northwind')
    for (const p of LISTS) await ok('GET', p)
    const vms = await ok<{ name: string }[]>('GET', '/vms?descendants=true')
    expect(vms.map((v) => v.name)).toContain('web-1')
    expect(vms.map((v) => v.name)).not.toContain('app-1')
    expect((await call('GET', '/hosts')).status).toBe(403)
    await signIn('admin')
  })

  it('makes, runs, moves and removes a VM', async () => {
    const image = db.images.find((i) => i.name === 'debian-13')!
    const vm = await ok<{ id: string; task_id: string }>('POST', '/vms', {
      name: 'test-vm',
      vcpus: 2,
      memory_mib: 2048,
      disk_gib: 10,
      image_id: image.id,
      cluster_id: db.clusters.find((c) => c.name === 'ams-prod')!.id,
    })
    await settle(vm.task_id)
    expect((await ok('GET', `/vms/${vm.id}`)).power).toBe('running')

    const from = db.vms.find((v) => v.id === vm.id)!.host_id
    const m = await ok<{ task_id: string }>('POST', `/vms/${vm.id}/migrate`, {})
    await settle(m.task_id)
    expect(db.vms.find((v) => v.id === vm.id)!.host_id).not.toBe(from)

    const snap = await ok<{ task_id: string }>('POST', `/vms/${vm.id}/snapshots`, {})
    await settle(snap.task_id)
    const backup = await ok<{ task_id: string }>('POST', `/vms/${vm.id}/backups`, {})
    await settle(backup.task_id)

    const stop = await ok<{ task_id: string }>('POST', `/vms/${vm.id}/stop`)
    await settle(stop.task_id)
    expect((await ok('GET', `/vms/${vm.id}`)).power).toBe('stopped')
    expect((await ok('PATCH', `/vms/${vm.id}`, { sleep: false, vcpus: 4 })).sleep).toBe(false)

    const del = await ok<{ task_id: string }>('DELETE', `/vms/${vm.id}?delete_disks=true`)
    await settle(del.task_id)
    expect((await call('GET', `/vms/${vm.id}`)).status).toBe(404)
  })

  it('wakes a VM asleep', async () => {
    const desk = db.vms.find((v) => v.name === 'desk-gina')!
    desk.power = 'suspended'
    const t = await ok<{ task_id: string }>('POST', `/vms/${desk.id}/start`)
    await settle(t.task_id)
    expect(db.vms.find((v) => v.id === desk.id)!.power).toBe('running')
  })

  it('builds networks', async () => {
    const tenant = db.tenants.find((t) => t.slug === 'globex')!
    const internet = db.externalNetworks[0]!
    const vpc = await ok<{ id: string }>('POST', '/vpcs', { name: 'test', tenant_id: tenant.id, external_network_id: internet.id })
    const net = await ok<{ id: string }>('POST', '/networks', { name: 'test-net', vpc_id: vpc.id, cidr: '10.50.0.0/24' })
    const sg = await ok<{ id: string }>('POST', '/security-groups', { name: 'test-sg', tenant_id: tenant.id })
    await ok('POST', `/security-groups/${sg.id}/rules`, { direction: 'ingress', protocol: 'tcp', port_min: 22, port_max: 22, remote_cidr: '0.0.0.0/0' })
    const fip = await ok<{ id: string }>('POST', '/floating-ips', { external_network_id: internet.id, tenant_id: tenant.id })
    await ok('DELETE', `/floating-ips/${fip.id}`)
    const lb = await ok<{ id: string }>('POST', '/load-balancers', {
      name: 'test-lb',
      vpc_id: vpc.id,
      network_id: net.id,
      tenant_id: tenant.id,
      listeners: [{ protocol: 'tcp', port: 80 }],
    })
    await ok('DELETE', `/load-balancers/${lb.id}`)
    await ok('POST', `/tenants/${tenant.id}/network-policies`, { direction: 'egress', protocol: 'udp', port_min: 53, port_max: 53, remote_cidr: '0.0.0.0/0' })
    await ok('DELETE', `/networks/${net.id}`)
    await ok('DELETE', `/vpcs/${vpc.id}`)
  })

  it('runs tenants and their people', async () => {
    const root = db.tenants.find((t) => !t.parent_id)!
    const t = await ok<{ id: string }>('POST', '/tenants', { name: 'Initech', slug: 'initech', parent_id: root.id })
    await ok('PUT', `/tenants/${t.id}/quotas`, { vcpus: 8 })
    await ok('PUT', `/tenants/${t.id}/grants`, { clusters: [db.clusters[0]!.id], storage_pools: [] })
    const u = await ok<{ id: string }>('POST', '/users', { username: 'peter', password: 'demo-demo-1', tenant_id: t.id, role: 'operator' })
    await ok('PATCH', `/users/${u.id}`, { display_name: 'Peter' })
    const token = await ok<{ id: string }>('POST', '/tokens', { name: 'ci' })
    await ok('DELETE', `/tokens/${token.id}`)
    await signIn('peter', 'initech', 'demo-demo-1')
    expect((await ok('GET', '/auth/me')).username).toBe('peter')
    await signIn('admin')
    await ok('DELETE', `/users/${u.id}`)
    await ok('DELETE', `/tenants/${t.id}`)
  })

  it('adds images, and evacuates a host', async () => {
    const img = await ok<{ id: string }>('POST', '/images', { name: 'alpine', format: 'qcow2', source_url: 'https://example.org/alpine.qcow2' })
    await settle()
    await vi.advanceTimersByTimeAsync(10_000)
    await settle()
    expect((await ok<{ id: string; state: string }[]>('GET', '/images')).find((i) => i.id === img.id)?.state).toBe('ready')

    const host = db.hosts.find((h) => h.name === 'ams-hv02')!
    const ev = await ok<{ task_id: string }>('POST', `/hosts/${host.id}/evacuate`)
    await settle(ev.task_id)
    expect(db.vms.filter((v) => v.host_id === host.id && v.power === 'running' && v.migratable)).toEqual([])
  })
})
