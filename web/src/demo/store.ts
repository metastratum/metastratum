// The demo's data: what a controller keeps in Postgres, as records shaped
// after the API's views (fields worked out from others, such as names of
// what a record refers to and counts, are left out and filled in as views
// are made). Kept in the visitor's browser, so their changes survive a
// reload; Reset starts again from the seed.
import type { components } from '../api/schema'
import { notFound } from './http'
import { seed, SEED_VERSION } from './seed'

type S = components['schemas']

export type TenantRec = S['TenantView']
export type UserRec = Omit<S['UserView'], 'two_factor'> & { password: string }
export type BindingRec = Omit<S['BindingView'], 'tenant_name' | 'username'>
export type TokenRec = S['TokenView'] & { user_id: string }
export type FactorRec = S['FactorView'] & { user_id: string }
export type HostRec = Omit<S['HostView'], 'allocated_memory_mib' | 'vm_count' | 'cluster_name' | 'sole_images'>
export type ClusterRec = S['ClusterView']
export type PoolRec = Omit<S['PoolView'], 'volumes'>
export type RepositoryRec = S['RepositoryView']
export type ExternalNetworkRec = Omit<S['ExternalNetworkView'], 'used' | 'hosts'>
/** `hosts`: the names of the hosts holding a copy. */
export type ImageRec = Omit<S['ImageView'], 'storage_pool_name' | 'stuck'>
export type VpcRec = Omit<S['VpcView'], 'networks' | 'external_network_name'>
export type NetworkRec = Omit<S['NetworkView'], 'ports'>
export type GroupRec = Omit<S['SecurityGroupView'], 'members' | 'rules'>
export type RuleRec = Omit<S['RuleView'], 'remote_group_name'> & { group_id: string }
/** A floating IP: an address of an external network, maybe on a NIC. */
export type FloatingIpRec = {
  id: string
  tenant_id: string
  external_network_id: string
  ip: string
  nic_id: string | null
  created_at: string
}
export type LoadBalancerRec = Omit<S['LoadBalancerView'], 'members' | 'vpc_name'> & { member_vm_ids: string[] }
/** A guardrail, owned by `tenant_id`, for its whole subtree. */
export type PolicyRec = {
  id: string
  tenant_id: string
  direction: string
  protocol: string
  port_min: number | null
  port_max: number | null
  remote_cidr: string
  description: string
  created_at: string
}
export type NicRec = Omit<S['NicView'], 'network_name' | 'floating_ip' | 'floating_ip_id'>
export type VmRec = Omit<S['VmView'], 'host_name' | 'host_connected' | 'active_task' | 'nics'> & {
  nics: NicRec[]
  /** Its Windows account's password, while its first boot is pending. */
  password?: string | null
}
export type DiskRec = Omit<S['DiskView'], 'host_name' | 'storage_pool_name' | 'storage_pool_kind' | 'vm_name'>
export type SnapshotRec = S['SnapshotView']
export type BackupRec = S['BackupView']
export type TaskRec = S['TaskView'] & {
  /** What the task works on; its driver's to read. */
  params: Record<string, unknown>
  /** The step it is at (see tasks.ts). */
  at: number
}

export type Db = {
  /** The seed's version: a newer seed replaces what was kept. */
  version: number
  /** Who is signed in, if anyone. */
  session: { user_id: string; tenant_id: string } | null
  tenants: TenantRec[]
  /** Each tenant's quota limits, by resource. */
  quotas: Record<string, Record<string, number>>
  /** What each tenant was granted (see `Grants`). */
  grants: Record<string, S['Grants']>
  identityProviders: Record<string, S['ProviderView']>
  /** Tenants whose users need a second factor. */
  twoFactorRequired: string[]
  users: UserRec[]
  bindings: BindingRec[]
  tokens: TokenRec[]
  factors: FactorRec[]
  /** Backup codes left, by user. */
  backupCodes: Record<string, number>
  audit: S['AuditEntry'][]
  hosts: HostRec[]
  fencing: Record<string, S['Fencing']>
  clusters: ClusterRec[]
  pools: PoolRec[]
  repositories: RepositoryRec[]
  externalNetworks: ExternalNetworkRec[]
  images: ImageRec[]
  vpcs: VpcRec[]
  networks: NetworkRec[]
  groups: GroupRec[]
  rules: RuleRec[]
  floatingIps: FloatingIpRec[]
  loadBalancers: LoadBalancerRec[]
  policies: PolicyRec[]
  vms: VmRec[]
  disks: DiskRec[]
  snapshots: SnapshotRec[]
  backups: BackupRec[]
  schedules: Record<string, S['ScheduleView']>
  tasks: TaskRec[]
}

const KEY = 'metastratum.demo'

function load(): Db {
  try {
    const kept = JSON.parse(localStorage.getItem(KEY) ?? 'null') as Db | null
    if (kept && kept.version === SEED_VERSION) return kept
  } catch {
    // Unreadable or unavailable: start afresh.
  }
  return seed()
}

/** The demo's data. Handlers change it in place; it is saved after. */
export const db: Db = load()

let timer = 0
/** Saves the data soon (once per burst of changes). */
export function save() {
  clearTimeout(timer)
  timer = window.setTimeout(() => {
    try {
      localStorage.setItem(KEY, JSON.stringify(db))
    } catch {
      // Not kept across reloads; the demo goes on.
    }
  }, 200)
}

/** Back to the seed: forgets the visitor's changes and reloads. */
export function reset() {
  clearTimeout(timer)
  try {
    localStorage.removeItem(KEY)
  } catch {
    // Nothing kept.
  }
  location.reload()
}

export { newId, now } from './ids'

/** The record with this ID, or the error a controller answers with. */
export function find<T extends { id: string }>(list: T[], id: string, what: string): T {
  const x = list.find((r) => r.id === id)
  if (!x) throw notFound(what)
  return x
}

/** Removes the records `pred` matches. */
export function remove<T>(list: T[], pred: (x: T) => boolean) {
  for (let i = list.length - 1; i >= 0; i--) if (pred(list[i]!)) list.splice(i, 1)
}
