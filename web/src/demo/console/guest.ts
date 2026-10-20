// What a console shows of its guest, worked out from the VM's record: its
// distribution (from its image), addresses, disks and memory.
import { db, type VmRec } from '../store'

export type Distro = {
  /** /etc/os-release's PRETTY_NAME. */
  pretty: string
  id: string
  version: string
  codename: string
  /** The cloud image's default user. */
  user: string
  kernel: string
  /** `uname -v`. */
  build: string
}

const DEBIAN: Distro = {
  pretty: 'Debian GNU/Linux 13 (trixie)',
  id: 'debian',
  version: '13',
  codename: 'trixie',
  user: 'debian',
  kernel: '6.12.48+deb13-cloud-amd64',
  build: '#1 SMP PREEMPT_DYNAMIC Debian 6.12.48-1 (2025-09-20)',
}
const UBUNTU: Distro = {
  pretty: 'Ubuntu 24.04.3 LTS',
  id: 'ubuntu',
  version: '24.04',
  codename: 'noble',
  user: 'ubuntu',
  kernel: '6.8.0-85-generic',
  build: '#85-Ubuntu SMP PREEMPT_DYNAMIC Thu Sep 18 15:26:59 UTC 2025',
}
const ROCKY: Distro = {
  pretty: 'Rocky Linux 10.0 (Red Quartz)',
  id: 'rocky',
  version: '10.0',
  codename: 'Red Quartz',
  user: 'rocky',
  kernel: '6.12.0-55.12.1.el10_0.x86_64',
  build: '#1 SMP PREEMPT_DYNAMIC Wed May 28 15:48:22 UTC 2025',
}

export function imageName(vm: VmRec): string {
  const id = vm.disks[0]?.image_id
  return (id && db.images.find((i) => i.id === id)?.name) || ''
}

export function distro(vm: VmRec): Distro {
  const name = imageName(vm).toLowerCase()
  if (name.includes('ubuntu')) return UBUNTU
  if (name.includes('rocky') || name.includes('rhel') || name.includes('alma')) return ROCKY
  return DEBIAN
}

export function windowsEdition(vm: VmRec): string {
  const name = imageName(vm).toLowerCase()
  return name.includes('11') && !name.includes('server') ? 'Windows 11 Pro' : 'Windows Server 2025 Datacenter'
}

/** Windows' computer name: the VM's, cut to NetBIOS's 15 characters. */
export function computerName(vm: VmRec): string {
  return vm.name.slice(0, 15).toUpperCase()
}

export type Iface = { name: string; mac: string; ip: string; prefix: number; gateway: string; network: string }

export function interfaces(vm: VmRec, linuxNames = true): Iface[] {
  return vm.nics.map((n, i) => {
    const net = db.networks.find((x) => x.id === n.network_id)
    const prefix = Number(net?.cidr.split('/')[1] ?? 24)
    return {
      name: linuxNames ? `enp${i + 1}s0` : i === 0 ? 'Ethernet' : `Ethernet ${i + 1}`,
      mac: n.mac,
      ip: n.ip,
      prefix,
      gateway: net?.gateway ?? '',
      network: net?.name ?? '',
    }
  })
}

/** An IPv6 link-local address from a MAC (EUI-64). */
export function linkLocal(mac: string): string {
  const b = mac.split(':').map((x) => parseInt(x, 16))
  if (b.length !== 6) return 'fe80::1'
  b[0]! ^= 2
  const w = [(b[0]! << 8) | b[1]!, (b[2]! << 8) | 0xff, 0xfe00 | b[3]!, (b[4]! << 8) | b[5]!]
  return `fe80::${w.map((x) => x.toString(16)).join(':')}`
}

export type GuestDisk = { dev: string; gib: number; boot: boolean; name: string | null }

export function guestDisks(vm: VmRec): GuestDisk[] {
  let scsi = 1
  return vm.disks.map((d, i) => {
    const rec = db.disks.find((x) => x.id === d.volume_id)
    const gib = Math.round(d.size_bytes / 2 ** 30)
    return i === 0
      ? { dev: 'vda', gib, boot: true, name: rec?.name ?? null }
      : { dev: `sd${String.fromCharCode(97 + (d.unit ?? scsi++))}`, gib, boot: false, name: rec?.name ?? null }
  })
}

/** Seconds since the VM last started (as far as the record says). */
export function uptimeSeconds(vm: VmRec): number {
  const since = vm.power_observed_at ? Date.parse(vm.power_observed_at) : Date.parse(vm.created_at)
  return Math.max(60, Math.floor((Date.now() - (Number.isFinite(since) ? since : Date.now())) / 1000))
}

/** A stable pseudo-random number for this VM and a label, in [0, 1). */
export function stable(vm: VmRec, label: string): number {
  let h = 2166136261
  for (const c of vm.id + label) h = Math.imul(h ^ c.charCodeAt(0), 16777619)
  return ((h >>> 0) % 10000) / 10000
}

export function memoryMib(vm: VmRec) {
  const total = Math.round(vm.memory_mib * 0.96)
  const used = Math.round(total * (0.12 + stable(vm, 'mem') * 0.35))
  const cache = Math.round(total * (0.1 + stable(vm, 'cache') * 0.25))
  return { total, used, cache, free: Math.max(0, total - used - cache), available: total - used }
}

export const pad = (s: string | number, n: number) => String(s).padEnd(n)
export const lpad = (s: string | number, n: number) => String(s).padStart(n)
