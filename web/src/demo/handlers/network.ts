// Networking: VPCs and their networks, security groups and their rules,
// floating IPs, load balancers and guardrails, as the controller's vpcs.rs,
// security.rs, floating_ips.rs and load_balancers.rs answer. NICs belong to
// VMs (the VM handlers'); here they are read for ports, members and where
// floating IPs point.
import type { components } from '../../api/schema'
import { audit, authorize, emit, homeTenant, lineage, listScope, may, root, subtree, tenantName, type Perm } from '../ctx'
import { badRequest, conflict, created, HttpError, none, notFound, route } from '../http'
import {
  db,
  find,
  newId,
  now,
  remove,
  type FloatingIpRec,
  type GroupRec,
  type LoadBalancerRec,
  type NetworkRec,
  type NicRec,
  type PolicyRec,
  type RuleRec,
  type VmRec,
  type VpcRec,
} from '../store'

type S = components['schemas']

// ---------------------------------------------------------------------------
// Addresses.

function ipToNum(ip: string): number | null {
  const m = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/.exec(ip.trim())
  if (!m) return null
  const parts = m.slice(1).map(Number)
  if (parts.some((p) => p > 255)) return null
  return ((parts[0]! << 24) >>> 0) + (parts[1]! << 16) + (parts[2]! << 8) + parts[3]!
}

function numToIp(n: number): string {
  return [n >>> 24, (n >>> 16) & 255, (n >>> 8) & 255, n & 255].join('.')
}

type Cidr = { addr: number; len: number }

/** An address or range (`a.b.c.d` is a /32), as mst_netplan::Cidr parses. */
function parseCidr(s: string): Cidr | null {
  const [a, l = '32'] = s.trim().split('/')
  if (!/^\d+$/.test(l)) return null
  const len = Number(l)
  const addr = ipToNum(a ?? '')
  if (addr === null || len > 32) return null
  return { addr, len }
}

const mask = (len: number) => (len === 0 ? 0 : (0xffffffff << (32 - len)) >>> 0)
const base = (c: Cidr) => (c.addr & mask(c.len)) >>> 0
const size = (c: Cidr) => 2 ** (32 - c.len)
const hasHostBits = (c: Cidr) => base(c) !== c.addr
const cidrText = (c: Cidr) => `${numToIp(c.addr)}/${c.len}`
const contains = (c: Cidr, ip: number) => ip >= base(c) && ip < base(c) + size(c)
const overlap = (a: Cidr, b: Cidr) => base(a) < base(b) + size(b) && base(b) < base(a) + size(a)

// ---------------------------------------------------------------------------
// Checks shared by the handlers.

/** Like ctx.authorize, but answers 404 when the user cannot even see the tenant. */
function authorizeResource(perm: Perm, tenantId: string, kind: string) {
  if (may(perm, tenantId)) return
  if (may('tenant.read', tenantId)) throw new HttpError(403, 'permission_denied', 'permission denied')
  throw notFound(kind)
}

function checkName(name: string): string {
  const n = name.trim()
  if (n.length < 1 || n.length > 64) throw badRequest('name must be 1-64 characters')
  return n
}

function quotaUsage(resource: 'vpcs' | 'floating_ips' | 'load_balancers', tenants: Set<string>): number {
  switch (resource) {
    case 'vpcs':
      return db.vpcs.filter((v) => tenants.has(v.tenant_id)).length
    case 'floating_ips':
      return db.floatingIps.filter((f) => tenants.has(f.tenant_id)).length
    case 'load_balancers':
      return db.loadBalancers.filter((l) => tenants.has(l.tenant_id)).length
  }
}

/** One more of `resource` in `tenantId` must fit every quota up the tree. */
function reserveQuota(tenantId: string, resource: 'vpcs' | 'floating_ips' | 'load_balancers') {
  for (const t of lineage(tenantId)) {
    const max = db.quotas[t.id]?.[resource]
    if (max === undefined) continue
    const want = quotaUsage(resource, subtree(t.id)) + 1
    if (want > max) {
      throw new HttpError(409, 'quota_exceeded', `quota exceeded for ${resource} (${want} > ${max}) in tenant ${t.name}`)
    }
  }
}

/** Whether the tenant may use an external network: the provider may use all. */
function mayUseExternal(tenantId: string, externalId: string): boolean {
  if (tenantId === root().id) return true
  return (db.grants[tenantId]?.external_networks ?? []).includes(externalId)
}

function externalNetwork(id: string) {
  return find(db.externalNetworks, id, 'external network')
}

/** Every address taken on an external network: VPC gateways, floating IPs, load balancers. */
function usedExternal(externalId: string): Set<string> {
  const used = new Set<string>()
  for (const v of db.vpcs) if (v.external_network_id === externalId && v.gateway_ip) used.add(v.gateway_ip)
  for (const f of db.floatingIps) if (f.external_network_id === externalId) used.add(f.ip)
  for (const lb of db.loadBalancers) {
    if (!lb.external) continue
    const vpc = db.vpcs.find((v) => v.id === lb.vpc_id)
    if (vpc?.external_network_id === externalId) used.add(lb.ip)
  }
  return used
}

/** The first free address in the external network's pool (never its gateway). */
function allocateExternal(externalId: string): string {
  const e = externalNetwork(externalId)
  const start = ipToNum(e.pool_start)
  const end = ipToNum(e.pool_end)
  const gateway = ipToNum(e.gateway)
  const used = usedExternal(externalId)
  if (start !== null && end !== null) {
    for (let n = start; n <= end; n++) {
      if (n === gateway) continue
      const ip = numToIp(n)
      if (!used.has(ip)) return ip
    }
  }
  throw conflict('the external network has no free addresses')
}

/** Every NIC, with its VM. */
function nics(): { nic: NicRec; vm: VmRec }[] {
  return db.vms.flatMap((vm) => vm.nics.map((nic) => ({ nic, vm })))
}

function nicById(id: string): { nic: NicRec; vm: VmRec } | undefined {
  return nics().find((x) => x.nic.id === id)
}

// ---------------------------------------------------------------------------
// Views.

export function networkView(n: NetworkRec): S['NetworkView'] {
  return { ...n, ports: nics().filter((x) => x.nic.network_id === n.id).length }
}

export function vpcView(v: VpcRec): S['VpcView'] {
  return {
    ...v,
    external_network_name: v.external_network_id
      ? (db.externalNetworks.find((e) => e.id === v.external_network_id)?.name ?? null)
      : null,
    gateway_ip: v.external_network_id ? (v.gateway_ip ?? null) : null,
    networks: db.networks.filter((n) => n.vpc_id === v.id).map(networkView),
  }
}

function ruleView(r: RuleRec): S['RuleView'] {
  const { group_id: _g, ...rule } = r
  return {
    ...rule,
    remote_group_name: r.remote_group_id ? (db.groups.find((g) => g.id === r.remote_group_id)?.name ?? null) : null,
  }
}

export function groupView(g: GroupRec): S['SecurityGroupView'] {
  return {
    ...g,
    rules: db.rules.filter((r) => r.group_id === g.id).map(ruleView),
    members: nics().filter((x) => x.nic.security_group_ids.includes(g.id)).length,
  }
}

export function floatingIpView(f: FloatingIpRec): S['FloatingIpView'] {
  const target = f.nic_id ? nicById(f.nic_id) : undefined
  return {
    ...f,
    external_network_name: db.externalNetworks.find((e) => e.id === f.external_network_id)?.name ?? '',
    vm_id: target?.vm.id ?? null,
    vm_name: target?.vm.name ?? null,
  }
}

/** A member VM's NIC in the VPC (its first one there). */
function memberNic(vm: VmRec, vpcId: string): NicRec | undefined {
  return vm.nics.find((n) => db.networks.find((x) => x.id === n.network_id)?.vpc_id === vpcId)
}

export function loadBalancerView(lb: LoadBalancerRec): S['LoadBalancerView'] {
  const { member_vm_ids, ...rest } = lb
  const members: S['MemberView'][] = []
  for (const id of member_vm_ids) {
    const vm = db.vms.find((v) => v.id === id)
    const nic = vm && memberNic(vm, lb.vpc_id)
    if (vm && nic) members.push({ vm_id: vm.id, vm_name: vm.name, nic_id: nic.id, ip: nic.ip })
  }
  members.sort((a, b) => a.vm_name.localeCompare(b.vm_name))
  return {
    ...rest,
    vpc_name: db.vpcs.find((v) => v.id === lb.vpc_id)?.name ?? '',
    listeners: lb.listeners.map((l) => ({ ...l, target_port: l.target_port ?? l.port })),
    members,
  }
}

/** A guardrail as stored, with the tenant whose administrators set it. */
type Policy = PolicyRec & { owner_tenant_id?: string }
const ownerOf = (p: Policy) => p.owner_tenant_id ?? p.tenant_id

function policyViews(tenantId: string): S['PolicyView'][] {
  const line = lineage(tenantId)
  const depth = new Map(line.map((t, i) => [t.id, i]))
  return (db.policies as Policy[])
    .filter((p) => depth.has(p.tenant_id))
    .sort((a, b) => depth.get(a.tenant_id)! - depth.get(b.tenant_id)! || a.created_at.localeCompare(b.created_at))
    .map((p) => ({
      id: p.id,
      tenant_id: p.tenant_id,
      tenant_name: tenantName(p.tenant_id),
      inherited: p.tenant_id !== tenantId,
      owner_tenant_id: ownerOf(p),
      owner_name: tenantName(ownerOf(p)),
      removable: may('policy.write', ownerOf(p)),
      direction: p.direction,
      protocol: p.protocol,
      port_min: p.port_min,
      port_max: p.port_max,
      remote_cidr: p.remote_cidr,
      description: p.description,
      created_at: p.created_at,
    }))
}

// ---------------------------------------------------------------------------
// VPCs.

/** Gives a VPC a way out through `ext`, or takes it away. */
function setExternal(vpc: VpcRec, ext: string | null) {
  if ((vpc.external_network_id ?? null) === ext) return
  const old = vpc.external_network_id
  if (old) {
    const inVpc = new Set(db.networks.filter((n) => n.vpc_id === vpc.id).map((n) => n.id))
    const fips = db.floatingIps.filter(
      (f) => f.external_network_id === old && f.nic_id && inVpc.has(nicById(f.nic_id)?.nic.network_id ?? ''),
    ).length
    if (fips > 0) throw conflict(`${fips} floating IP(s) point at VMs in this VPC; detach them first`)
    if (db.loadBalancers.some((lb) => lb.vpc_id === vpc.id && lb.external)) {
      throw conflict('load balancers in this VPC have addresses on its external network; delete them first')
    }
  }
  if (ext) {
    externalNetwork(ext)
    if (!mayUseExternal(vpc.tenant_id, ext)) {
      throw new HttpError(403, 'permission_denied', 'that external network is not available to this tenant')
    }
    vpc.gateway_ip = allocateExternal(ext)
  } else {
    vpc.gateway_ip = null
  }
  vpc.external_network_id = ext
  emit('external_ip', vpc.id)
}

route.get('/api/v1/vpcs', ({ query }) => {
  const tenants = listScope(query, 'network.read')
  return db.vpcs.filter((v) => tenants.has(v.tenant_id)).map(vpcView)
})

route.get('/api/v1/vpcs/{id}', ({ params }) => {
  const v = find(db.vpcs, params.id, 'VPC')
  authorizeResource('network.read', v.tenant_id, 'VPC')
  return vpcView(v)
})

route.post('/api/v1/vpcs', ({ body }) => {
  const tenant = body.tenant_id ?? homeTenant()
  authorize('network.write', tenant)
  const name = checkName(body.name)
  if (db.vpcs.some((v) => v.tenant_id === tenant && v.name === name)) throw conflict('a VPC with that name already exists')
  reserveQuota(tenant, 'vpcs')
  const vpc: VpcRec = { id: newId(), tenant_id: tenant, name, external_network_id: null, gateway_ip: null, created_at: now() }
  setExternal(vpc, body.external_network_id ?? null)
  db.vpcs.push(vpc)
  audit('vpc.create', ['vpc', vpc.id], { name, external_network_id: body.external_network_id ?? null }, tenant)
  emit('vpc', vpc.id, 'insert')
  return created(vpcView(vpc))
})

route.patch('/api/v1/vpcs/{id}', ({ params, body }) => {
  const vpc = find(db.vpcs, params.id, 'VPC')
  authorizeResource('network.write', vpc.tenant_id, 'VPC')
  const name = body.name != null ? checkName(body.name) : null
  if (name && db.vpcs.some((v) => v.id !== vpc.id && v.tenant_id === vpc.tenant_id && v.name === name)) {
    throw conflict('a VPC with that name already exists')
  }
  // `external_network_id` absent leaves it; `null` disconnects the VPC.
  if ('external_network_id' in body) setExternal(vpc, body.external_network_id ?? null)
  if (name) vpc.name = name
  audit('vpc.update', ['vpc', vpc.id], { name: body.name ?? null, external_network_id: body.external_network_id }, vpc.tenant_id)
  emit('vpc', vpc.id)
  return vpcView(vpc)
})

route.delete('/api/v1/vpcs/{id}', ({ params }) => {
  const vpc = find(db.vpcs, params.id, 'VPC')
  authorizeResource('network.write', vpc.tenant_id, 'VPC')
  if (db.loadBalancers.some((lb) => lb.vpc_id === vpc.id)) throw conflict('the VPC has load balancers; delete them first')
  if (db.networks.some((n) => n.vpc_id === vpc.id)) throw conflict("delete the VPC's networks first")
  // Its gateway address goes with it.
  remove(db.vpcs, (v) => v.id === vpc.id)
  audit('vpc.delete', ['vpc', vpc.id], { name: vpc.name }, vpc.tenant_id)
  emit('vpc', vpc.id, 'delete')
  if (vpc.external_network_id) emit('external_ip', vpc.id, 'delete')
  return none()
})

// ---------------------------------------------------------------------------
// Networks.

route.get('/api/v1/networks', ({ query }) => {
  const tenants = listScope(query, 'network.read')
  return db.networks.filter((n) => tenants.has(n.tenant_id)).map(networkView)
})

route.get('/api/v1/networks/{id}', ({ params }) => {
  const n = find(db.networks, params.id, 'network')
  authorizeResource('network.read', n.tenant_id, 'network')
  return networkView(n)
})

route.post('/api/v1/networks', ({ body }) => {
  const vpc = find(db.vpcs, body.vpc_id, 'VPC')
  authorizeResource('network.write', vpc.tenant_id, 'VPC')
  const name = checkName(body.name)
  const cidr = parseCidr(body.cidr)
  if (!cidr || cidr.len < 16 || cidr.len > 29) {
    throw badRequest('cidr must be an IPv4 range from /16 to /29, e.g. 10.1.0.0/24')
  }
  if (hasHostBits(cidr)) throw badRequest('invalid address or range')
  let gateway: string
  if (body.gateway != null) {
    const g = ipToNum(body.gateway)
    if (g === null) throw badRequest('invalid address or range')
    if (!contains(cidr, g)) throw badRequest("the gateway must lie within the network's range")
    gateway = numToIp(g)
  } else {
    gateway = numToIp(base(cidr) + 1)
  }
  if (db.networks.some((n) => n.vpc_id === vpc.id && overlap(parseCidr(n.cidr)!, cidr))) {
    throw conflict('the range overlaps another network in this VPC')
  }
  if (db.externalNetworks.some((e) => overlap(parseCidr(e.cidr)!, cidr))) {
    throw badRequest("the range overlaps an external network's; choose a private range")
  }
  if (db.networks.some((n) => n.tenant_id === vpc.tenant_id && n.name === name)) {
    throw conflict('a network with that name already exists')
  }
  const n: NetworkRec = {
    id: newId(),
    tenant_id: vpc.tenant_id,
    vpc_id: vpc.id,
    name,
    cidr: cidrText(cidr),
    gateway,
    created_at: now(),
  }
  db.networks.push(n)
  audit('network.create', ['network', n.id], { name, cidr: n.cidr, vpc_id: vpc.id }, vpc.tenant_id)
  emit('network', n.id, 'insert')
  emit('vpc', vpc.id)
  return created(networkView(n))
})

route.delete('/api/v1/networks/{id}', ({ params }) => {
  const n = find(db.networks, params.id, 'network')
  authorizeResource('network.write', n.tenant_id, 'network')
  if (db.loadBalancers.some((lb) => lb.network_id === n.id)) {
    throw conflict('load balancers have their address in this network; delete them first')
  }
  if (nics().some((x) => x.nic.network_id === n.id)) throw conflict('VMs are still connected to this network')
  remove(db.networks, (x) => x.id === n.id)
  audit('network.delete', ['network', n.id], { name: n.name }, n.tenant_id)
  emit('network', n.id, 'delete')
  emit('vpc', n.vpc_id)
  return none()
})

// ---------------------------------------------------------------------------
// Security groups.

type RuleSpec = S['RuleSpec']

/** Checks a rule (or guardrail) as security.rs's check_rule does. */
function checkRule(r: RuleSpec) {
  if (r.direction !== 'ingress' && r.direction !== 'egress') throw badRequest('direction must be ingress or egress')
  if (!['any', 'tcp', 'udp', 'icmp'].includes(r.protocol)) throw badRequest('protocol must be any, tcp, udp or icmp')
  let ports: [number, number] | null = null
  if (r.port_min != null) {
    const max = r.port_max ?? r.port_min
    if (r.port_min < 1 || r.port_min > 65535 || max > 65535 || max < r.port_min) {
      throw badRequest('ports must be 1-65535, lowest first')
    }
    if (r.protocol !== 'tcp' && r.protocol !== 'udp') throw badRequest('ports apply to tcp and udp only')
    ports = [r.port_min, max]
  } else if (r.port_max != null) {
    throw badRequest('give port_min (and optionally port_max)')
  }
  let remote: string | null = null
  if (r.remote_cidr != null) {
    const c = parseCidr(r.remote_cidr)
    if (!c || hasHostBits(c)) throw badRequest('remote_cidr must be an IPv4 address or range')
    remote = cidrText(c)
  }
  if ((r.description ?? '').length > 256) throw badRequest('descriptions are at most 256 characters')
  return { direction: r.direction, protocol: r.protocol, ports, remote, description: (r.description ?? '').trim() }
}

route.get('/api/v1/security-groups', ({ query }) => {
  const tenants = listScope(query, 'network.read')
  return db.groups.filter((g) => tenants.has(g.tenant_id)).map(groupView)
})

route.get('/api/v1/security-groups/{id}', ({ params }) => {
  const g = find(db.groups, params.id, 'security group')
  authorizeResource('network.read', g.tenant_id, 'security group')
  return groupView(g)
})

route.post('/api/v1/security-groups', ({ body }) => {
  const tenant = body.tenant_id ?? homeTenant()
  authorize('network.write', tenant)
  const name = checkName(body.name)
  if (db.groups.some((g) => g.tenant_id === tenant && g.name === name)) {
    throw conflict('a security group with that name already exists')
  }
  const g: GroupRec = { id: newId(), tenant_id: tenant, name, description: (body.description ?? '').trim(), created_at: now() }
  db.groups.push(g)
  audit('security_group.create', ['security_group', g.id], { name }, tenant)
  emit('security_group', g.id, 'insert')
  return created(groupView(g))
})

route.patch('/api/v1/security-groups/{id}', ({ params, body }) => {
  const g = find(db.groups, params.id, 'security group')
  authorizeResource('network.write', g.tenant_id, 'security group')
  const name = body.name != null ? checkName(body.name) : g.name
  if (db.groups.some((x) => x.id !== g.id && x.tenant_id === g.tenant_id && x.name === name)) {
    throw conflict('a security group with that name already exists')
  }
  g.name = name
  if (body.description != null) g.description = body.description.trim()
  audit('security_group.update', ['security_group', g.id], { name }, g.tenant_id)
  emit('security_group', g.id)
  return groupView(g)
})

route.delete('/api/v1/security-groups/{id}', ({ params }) => {
  const g = find(db.groups, params.id, 'security group')
  authorizeResource('network.write', g.tenant_id, 'security group')
  if (nics().some((x) => x.nic.security_group_ids.includes(g.id))) throw conflict('VMs still use this security group')
  remove(db.groups, (x) => x.id === g.id)
  remove(db.rules, (r) => r.group_id === g.id)
  // Rules elsewhere that allowed its members go too.
  const referring = new Set(db.rules.filter((r) => r.remote_group_id === g.id).map((r) => r.group_id))
  remove(db.rules, (r) => r.remote_group_id === g.id)
  audit('security_group.delete', ['security_group', g.id], { name: g.name }, g.tenant_id)
  emit('security_group', g.id, 'delete')
  for (const id of referring) emit('security_group', id)
  return none()
})

route.post('/api/v1/security-groups/{id}/rules', ({ params, body }) => {
  const g = find(db.groups, params.id, 'security group')
  authorizeResource('network.write', g.tenant_id, 'security group')
  const c = checkRule(body)
  const remoteGroup = body.remote_group_id ?? null
  if (c.remote !== null && remoteGroup === null) {
    // An address range.
  } else if (c.remote === null && remoteGroup !== null) {
    const remote = find(db.groups, remoteGroup, 'security group')
    if (remote.tenant_id !== g.tenant_id) throw badRequest('a rule can only refer to groups of the same tenant')
  } else {
    throw badRequest('give either remote_cidr or remote_group_id')
  }
  const rule: RuleRec = {
    id: newId(),
    group_id: g.id,
    direction: c.direction,
    protocol: c.protocol,
    port_min: c.ports?.[0] ?? null,
    port_max: c.ports?.[1] ?? null,
    remote_cidr: c.remote,
    remote_group_id: remoteGroup,
    description: c.description,
  }
  db.rules.push(rule)
  audit(
    'security_group.rule.add',
    ['security_group', g.id],
    {
      rule: rule.id,
      direction: rule.direction,
      protocol: rule.protocol,
      ports: [rule.port_min, rule.port_max],
      remote_cidr: rule.remote_cidr,
      remote_group_id: rule.remote_group_id,
    },
    g.tenant_id,
  )
  emit('security_group', g.id)
  return created(groupView(g))
})

route.delete('/api/v1/security-group-rules/{id}', ({ params }) => {
  const rule = find(db.rules, params.id, 'security group rule')
  const g = find(db.groups, rule.group_id, 'security group')
  authorizeResource('network.write', g.tenant_id, 'security group')
  remove(db.rules, (r) => r.id === rule.id)
  audit('security_group.rule.delete', ['security_group', g.id], { rule: rule.id }, g.tenant_id)
  emit('security_group', g.id)
  return none()
})

// ---------------------------------------------------------------------------
// Floating IPs.

/** A floating IP may point at a NIC of its tenant's VMs, in a VPC routed to its network. */
function checkTarget(tenantId: string, externalId: string, nicId: string) {
  const target = nicById(nicId)
  if (!target || target.vm.tenant_id !== tenantId) throw notFound('NIC')
  const net = find(db.networks, target.nic.network_id, 'network')
  const vpc = find(db.vpcs, net.vpc_id, 'VPC')
  if (vpc.external_network_id !== externalId) {
    throw badRequest(`VPC ${vpc.name} does not route to this floating IP's external network; connect it first`)
  }
  return target
}

/** Announces the VMs whose NICs gained or lost a floating IP. */
function vmsChanged(...nicIds: (string | null | undefined)[]) {
  for (const id of new Set(nicIds)) {
    const vm = id ? nicById(id)?.vm : undefined
    if (vm) emit('vm', vm.id)
  }
}

route.get('/api/v1/floating-ips', ({ query }) => {
  const tenants = listScope(query, 'network.read')
  return db.floatingIps.filter((f) => tenants.has(f.tenant_id)).map(floatingIpView)
})

route.get('/api/v1/floating-ips/{id}', ({ params }) => {
  const f = find(db.floatingIps, params.id, 'floating IP')
  authorizeResource('network.read', f.tenant_id, 'floating IP')
  return floatingIpView(f)
})

route.post('/api/v1/floating-ips', ({ body }) => {
  const tenant = body.tenant_id ?? homeTenant()
  authorize('network.write', tenant)
  externalNetwork(body.external_network_id)
  if (!mayUseExternal(tenant, body.external_network_id)) {
    throw new HttpError(403, 'permission_denied', 'that external network is not available to this tenant')
  }
  if (body.nic_id) checkTarget(tenant, body.external_network_id, body.nic_id)
  reserveQuota(tenant, 'floating_ips')
  const f: FloatingIpRec = {
    id: newId(),
    tenant_id: tenant,
    external_network_id: body.external_network_id,
    ip: allocateExternal(body.external_network_id),
    nic_id: body.nic_id ?? null,
    created_at: now(),
  }
  db.floatingIps.push(f)
  audit('floating_ip.allocate', ['floating_ip', f.id], { ip: f.ip, nic_id: f.nic_id }, tenant)
  emit('external_ip', f.id, 'insert')
  vmsChanged(f.nic_id)
  return created(floatingIpView(f))
})

route.patch('/api/v1/floating-ips/{id}', ({ params, body }) => {
  const f = find(db.floatingIps, params.id, 'floating IP')
  authorizeResource('network.write', f.tenant_id, 'floating IP')
  const nic = body.nic_id ?? null
  if (nic) {
    checkTarget(f.tenant_id, f.external_network_id, nic)
    const other = db.floatingIps.find((x) => x.id !== f.id && x.nic_id === nic)
    if (other) throw conflict(`the interface already has floating IP ${other.ip}`)
  }
  const before = f.nic_id
  f.nic_id = nic
  audit('floating_ip.update', ['floating_ip', f.id], { ip: f.ip, nic_id: nic }, f.tenant_id)
  emit('external_ip', f.id)
  vmsChanged(before, nic)
  return floatingIpView(f)
})

route.delete('/api/v1/floating-ips/{id}', ({ params }) => {
  const f = find(db.floatingIps, params.id, 'floating IP')
  authorizeResource('network.write', f.tenant_id, 'floating IP')
  remove(db.floatingIps, (x) => x.id === f.id)
  audit('floating_ip.release', ['floating_ip', f.id], { ip: f.ip }, f.tenant_id)
  emit('external_ip', f.id, 'delete')
  vmsChanged(f.nic_id)
  return none()
})

// ---------------------------------------------------------------------------
// Load balancers.

type ListenerSpec = S['ListenerSpec']

function checkLbName(name: string): string {
  const n = name.trim()
  // eslint-disable-next-line no-control-regex
  if (!n || [...n].length > 64 || /[\u0000-\u001f\u007f]/.test(n)) {
    throw badRequest("a load balancer's name is 1-64 characters, on one line")
  }
  return n
}

function checkListeners(specs: ListenerSpec[]): ListenerSpec[] {
  if (specs.length > 32) throw badRequest('a load balancer has at most 32 listeners')
  const seen = new Set<string>()
  return specs.map((l) => {
    if (l.protocol !== 'tcp' && l.protocol !== 'udp') throw badRequest("a listener's protocol is tcp or udp")
    const target = l.target_port ?? l.port
    if (l.port < 1 || l.port > 65535 || target < 1 || target > 65535) throw badRequest('ports are 1-65535')
    const key = `${l.protocol}/${l.port}`
    if (seen.has(key)) throw badRequest(`two listeners on ${l.protocol} port ${l.port}`)
    seen.add(key)
    return { protocol: l.protocol, port: l.port, target_port: target }
  })
}

/** The members, each the tenant's with a NIC in the VPC. */
function checkMembers(tenantId: string, vpcId: string, ids: string[]): string[] {
  const out = [...new Set(ids)]
  for (const id of out) {
    const vm = db.vms.find((v) => v.id === id)
    if (!vm || vm.tenant_id !== tenantId) throw notFound('VM')
    if (!memberNic(vm, vpcId)) throw badRequest(`${vm.name} has no network interface in the load balancer's VPC`)
  }
  return out
}

/** A free address in a network for a load balancer's virtual IP. */
function reserveInNetwork(n: NetworkRec): string {
  const c = parseCidr(n.cidr)!
  const used = new Set([n.gateway, ...nics().filter((x) => x.nic.network_id === n.id).map((x) => x.nic.ip)])
  for (const lb of db.loadBalancers) if (lb.network_id === n.id && !lb.external) used.add(lb.ip)
  for (let s = 1; s < size(c) - 1; s++) {
    const ip = numToIp(base(c) + s)
    if (!used.has(ip)) return ip
  }
  throw conflict('network has no free addresses')
}

route.get('/api/v1/load-balancers', ({ query }) => {
  const tenants = listScope(query, 'network.read')
  return db.loadBalancers.filter((lb) => tenants.has(lb.tenant_id)).map(loadBalancerView)
})

route.get('/api/v1/load-balancers/{id}', ({ params }) => {
  const lb = find(db.loadBalancers, params.id, 'load balancer')
  authorizeResource('network.read', lb.tenant_id, 'load balancer')
  return loadBalancerView(lb)
})

route.post('/api/v1/load-balancers', ({ body }) => {
  const tenant = body.tenant_id ?? homeTenant()
  authorize('network.write', tenant)
  const name = checkLbName(body.name)
  const listeners = checkListeners(body.listeners ?? [])
  const health = body.health_check ?? true
  const vpc = db.vpcs.find((v) => v.id === body.vpc_id)
  if (!vpc || vpc.tenant_id !== tenant) throw notFound('VPC')
  reserveQuota(tenant, 'load_balancers')
  if (db.loadBalancers.some((l) => l.tenant_id === tenant && l.name === name)) {
    throw conflict('a load balancer with this name already exists')
  }
  let ip: string
  let external = false
  const networkId = body.network_id ?? null
  if (networkId) {
    const n = find(db.networks, networkId, 'network')
    if (n.vpc_id !== vpc.id) throw badRequest('the network is not in the VPC')
    ip = reserveInNetwork(n)
  } else {
    if (!vpc.external_network_id) {
      throw badRequest(
        `VPC ${vpc.name} has no external network: connect it to one, or put the address in one of its networks`,
      )
    }
    ip = allocateExternal(vpc.external_network_id)
    external = true
  }
  const members = checkMembers(tenant, vpc.id, body.member_vm_ids ?? [])
  const lb: LoadBalancerRec = {
    id: newId(),
    tenant_id: tenant,
    vpc_id: vpc.id,
    network_id: networkId,
    name,
    ip,
    external,
    health_check: health,
    listeners,
    member_vm_ids: members,
    created_at: now(),
  }
  db.loadBalancers.push(lb)
  audit('load_balancer.create', ['load_balancer', lb.id], { name, ip, vpc: vpc.name }, tenant)
  emit('load_balancer', lb.id, 'insert')
  if (external) emit('external_ip', lb.id, 'insert')
  return created(loadBalancerView(lb))
})

route.put('/api/v1/load-balancers/{id}', ({ params, body }) => {
  const lb = find(db.loadBalancers, params.id, 'load balancer')
  authorizeResource('network.write', lb.tenant_id, 'load balancer')
  const name = checkLbName(body.name)
  const listeners = checkListeners(body.listeners)
  if (db.loadBalancers.some((l) => l.id !== lb.id && l.tenant_id === lb.tenant_id && l.name === name)) {
    throw conflict('a load balancer with this name already exists')
  }
  const members = checkMembers(lb.tenant_id, lb.vpc_id, body.member_vm_ids)
  lb.name = name
  lb.health_check = body.health_check
  lb.listeners = listeners
  lb.member_vm_ids = members
  audit(
    'load_balancer.update',
    ['load_balancer', lb.id],
    { name, listeners: body.listeners, members: body.member_vm_ids },
    lb.tenant_id,
  )
  emit('load_balancer', lb.id)
  return loadBalancerView(lb)
})

route.delete('/api/v1/load-balancers/{id}', ({ params }) => {
  const lb = find(db.loadBalancers, params.id, 'load balancer')
  authorizeResource('network.write', lb.tenant_id, 'load balancer')
  remove(db.loadBalancers, (l) => l.id === lb.id)
  audit('load_balancer.delete', ['load_balancer', lb.id], { name: lb.name, ip: lb.ip }, lb.tenant_id)
  emit('load_balancer', lb.id, 'delete')
  if (lb.external) emit('external_ip', lb.id, 'delete')
  return none()
})

// ---------------------------------------------------------------------------
// Guardrails.

route.get('/api/v1/tenants/{id}/network-policies', ({ params }) => {
  find(db.tenants, params.id, 'tenant')
  authorizeResource('network.read', params.id, 'tenant')
  return policyViews(params.id)
})

route.post('/api/v1/tenants/{id}/network-policies', ({ params, body }) => {
  find(db.tenants, params.id, 'tenant')
  authorizeResource('policy.write', params.id, 'tenant')
  if (body.remote_group_id) throw badRequest('policies match addresses (remote_cidr), not groups')
  const c = checkRule(body)
  // Owned by the highest tenant the user sets it from: a guardrail an
  // administrator above puts on a sub-tenant is not the sub-tenant's to remove.
  const owner = lineage(params.id).find((t) => may('policy.write', t.id))?.id ?? params.id
  const p: Policy = {
    id: newId(),
    tenant_id: params.id,
    owner_tenant_id: owner,
    direction: c.direction,
    protocol: c.protocol,
    port_min: c.ports?.[0] ?? null,
    port_max: c.ports?.[1] ?? null,
    remote_cidr: c.remote ?? '0.0.0.0/0',
    description: c.description,
    created_at: now(),
  }
  db.policies.push(p)
  audit(
    'network_policy.create',
    ['network_policy', p.id],
    { direction: p.direction, protocol: p.protocol, ports: [p.port_min, p.port_max], remote_cidr: p.remote_cidr },
    params.id,
  )
  emit('network_policy', p.id, 'insert')
  return created(policyViews(params.id))
})

route.delete('/api/v1/network-policies/{id}', ({ params }) => {
  const p = find(db.policies as Policy[], params.id, 'policy')
  // Readable where it applies; removable only by whoever set it.
  authorizeResource('network.read', p.tenant_id, 'policy')
  if (!may('policy.write', ownerOf(p))) {
    throw new HttpError(403, 'permission_denied', 'a tenant above set this guardrail; only its administrators remove it')
  }
  remove(db.policies, (x) => x.id === p.id)
  audit('network_policy.delete', ['network_policy', p.id], {}, p.tenant_id)
  emit('network_policy', p.id, 'delete')
  return none()
})
