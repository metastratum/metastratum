// Images: added from a URL (the controller downloads it, as it is added and
// never again) or uploaded in parts; either way its file comes to one host,
// which checks it and stores it ('image.store'). Mirrors the controller's
// api/images.rs and receive.rs.
import type { components } from '../../api/schema'
import { audit, authorize, emit, homeTenant, lineage, listScope, root, subtree } from '../ctx'
import { badRequest, conflict, created, HttpError, none, route, unavailable } from '../http'
import { LOCAL_POOL } from '../seed'
import { db, find, newId, now, remove, save, type ImageRec, type PoolRec } from '../store'
import { driver, startTask } from '../tasks'

type S = components['schemas']

export function imageView(i: ImageRec): S['ImageView'] {
  return {
    ...i,
    received_bytes: i.state === 'downloading' || i.state === 'uploading' ? (i.received_bytes ?? 0) : null,
    stuck: false,
    storage_pool_name: db.pools.find((p) => p.id === i.storage_pool_id)?.name ?? '',
  }
}

/** Images a tenant may use: its own, and those its ancestors share. */
export function usableImages(tenantId: string): ImageRec[] {
  const above = new Set(lineage(tenantId).map((t) => t.id))
  return db.images
    .filter((i) => i.tenant_id === tenantId || (i.shared && above.has(i.tenant_id)))
    .sort((a, b) => a.name.localeCompare(b.name))
}

/** Local storage is everyone's; shared pools must be granted. */
function mayUsePool(tenantId: string, pool: PoolRec): boolean {
  return pool.id === LOCAL_POOL || tenantId === root().id || !!db.grants[tenantId]?.storage_pools.includes(pool.id)
}

/** A host an image's file can come to: up, in service, reaching the pool. */
function receivingHost(pool: PoolRec) {
  const hosts = db.hosts.filter(
    (h) => h.state === 'ready' && h.connected && (pool.kind === 'local' || (!!h.cluster_id && pool.cluster_ids.includes(h.cluster_id))),
  )
  if (!hosts.length) {
    throw unavailable(
      pool.kind === 'local'
        ? 'no connected host in service to keep the image (with an agent that takes images: upgrade it)'
        : `no connected host in service reaches ${pool.name} (with an agent that takes images: upgrade it)`,
    )
  }
  return hosts[Math.floor(Math.random() * hosts.length)]!
}

/** Refuses what Windows does not take as an account name. */
function windowsUser(name: string) {
  const bad = /["/\\[\]:;|=,+*?<>@%\p{Cc}]/u
  if (!name || [...name].length > 20 || bad.test(name) || name.endsWith('.') || /^[. ]+$/.test(name) || name !== name.trim()) {
    throw badRequest(
      'a Windows account name is 1-20 characters, none of " / \\ [ ] : ; | = , + * ? < > @ %, not only dots and spaces, and does not start or end in a space or end in a dot',
    )
  }
  const builtIn = [
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
  ]
  if (builtIn.includes(name.toLowerCase())) throw badRequest(`${name} is one of Windows' own accounts or groups: name another`)
}

/** Whether an address is one tenants may not have the controller download from. */
function privateAddress(host: string): boolean {
  const m = /^(\d+)\.(\d+)\.(\d+)\.(\d+)$/.exec(host)
  if (m) {
    const [a, b] = [Number(m[1]), Number(m[2])]
    return a === 10 || a === 127 || a === 0 || (a === 169 && b === 254) || (a === 172 && b >= 16 && b <= 31) || (a === 192 && b === 168) || (a === 100 && b >= 64 && b <= 127)
  }
  return host.startsWith('[')
}

const ADDING_AT_ONCE = 2
const ADDING = new Set(['downloading', 'uploading', 'storing'])

/** Refuses adding a third image at once in the tenant's tree. */
function mayAdd(tenantId: string) {
  const line = lineage(tenantId)
  const tree = line.length > 1 ? subtree(line[1]!.id) : new Set([tenantId])
  if (db.images.filter((i) => tree.has(i.tenant_id) && ADDING.has(i.state)).length >= ADDING_AT_ONCE) {
    throw conflict(
      `${ADDING_AT_ONCE} images of the tenant's are being added (its sub-tenants' and parent's count too); add more once they are done`,
    )
  }
}

const hex = (n: number) => [...crypto.getRandomValues(new Uint8Array(n))].map((b) => b.toString(16).padStart(2, '0')).join('')
const GIB = 2 ** 30

/** Starts storing an image whose file is all there, on `hostId`. */
function store(i: ImageRec, hostId: string) {
  i.state = 'storing'
  emit('image', i.id)
  startTask({ kind: 'image.store', tenant_id: i.tenant_id, target_kind: 'image', target_id: i.id, params: { host_id: hostId } })
}

driver('image.store', {
  steps: () => [
    ['checking the image', 1600],
    ['adding it to the store', 2400],
  ],
  finish(t) {
    const i = db.images.find((x) => x.id === t.target_id)
    // Deleted meanwhile: nothing to store.
    if (!i) return
    const host = db.hosts.find((h) => h.id === t.params.host_id) ?? db.hosts.find((h) => h.state === 'ready')
    const file = i.file_bytes ?? i.received_bytes ?? GIB
    i.state = 'ready'
    i.error = null
    i.size_bytes = i.format === 'qcow2' ? file : Math.round(file * 0.35)
    i.virtual_bytes = Math.max(Math.ceil((file * (i.format === 'qcow2' ? 3 : 1)) / GIB), 1) * GIB
    i.sha256 = hex(32)
    i.digest = hex(32)
    i.hosts = i.storage_pool_id === LOCAL_POOL && host ? [host.name] : []
    emit('image', i.id)
  },
  failed(t, error) {
    const i = db.images.find((x) => x.id === t.target_id)
    if (!i) return
    i.state = 'failed'
    i.error = error
    emit('image', i.id)
  },
})

/** Downloads under way, by image (their timers). */
const downloads = new Map<string, number>()
/** The host each upload's parts go to. */
const uploadHosts = new Map<string, string>()

/** The controller downloading an image's URL: progress, then storing it. */
function download(id: string, hostId: string) {
  if (downloads.has(id)) return
  const timer = window.setInterval(() => {
    const i = db.images.find((x) => x.id === id)
    if (!i || i.state !== 'downloading') {
      clearInterval(timer)
      downloads.delete(id)
      return
    }
    // The server says how large it is with its first bytes.
    if (!i.file_bytes) i.file_bytes = Math.round((0.35 + Math.random() * 1.6) * GIB)
    i.received_bytes = Math.min(i.file_bytes, (i.received_bytes ?? 0) + Math.ceil(i.file_bytes / 6))
    if (i.received_bytes >= i.file_bytes) {
      clearInterval(timer)
      downloads.delete(id)
      store(i, hostId)
    } else {
      emit('image', i.id)
    }
    save()
  }, 1000)
  downloads.set(id, timer)
}

// Downloads a reload cut short go on.
for (const i of db.images) {
  if (i.state === 'downloading') {
    const host = db.hosts.find((h) => h.state === 'ready' && h.connected)
    if (host) download(i.id, host.id)
  }
}

route.get('/api/v1/images', ({ query }) => {
  const [tenant] = listScope({ tenant_id: query.tenant_id }, 'image.read')
  return usableImages(tenant!).map(imageView)
})

route.post('/api/v1/images', ({ body }) => {
  const tenant = body.tenant_id ?? homeTenant()
  find(db.tenants, tenant, 'tenant')
  authorize('image.write', tenant)
  const name = body.name.trim()
  if (!name || body.name.length > 128) throw badRequest('name must be 1-128 characters')
  let url: string | null = null
  if (body.source_url != null && body.upload_bytes == null) {
    let parsed: URL
    try {
      parsed = new URL(body.source_url)
    } catch {
      throw badRequest('source_url: relative URL without a base')
    }
    if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') {
      throw badRequest(`source_url: ${parsed.protocol.replace(':', '')}: only http and https are allowed`)
    }
    if (tenant !== root().id && privateAddress(parsed.hostname)) {
      throw badRequest(`source_url: ${parsed.hostname.replace(/^\[|\]$/g, '')} is not a public address`)
    }
    url = body.source_url
  } else if (body.source_url == null && body.upload_bytes != null) {
    if (body.upload_bytes <= 0) throw badRequest('upload_bytes must be more than 0')
  } else {
    throw badRequest('give either source_url (to download it) or upload_bytes (to upload it)')
  }
  if (body.format !== 'qcow2' && body.format !== 'raw') throw badRequest('format must be qcow2 or raw')
  const os = body.os ?? 'linux'
  if (os !== 'linux' && os !== 'windows') throw badRequest('os must be linux or windows')
  const adminUser = body.admin_user?.trim() ?? null
  if (adminUser != null) {
    if (os !== 'windows') throw badRequest('admin_user is for windows images')
    windowsUser(adminUser)
  }
  const pool = db.pools.find((p) => p.id === (body.storage_pool_id ?? LOCAL_POOL))
  if (!pool || !mayUsePool(tenant, pool)) throw new HttpError(404, 'not_found', 'storage pool not found')
  const host = receivingHost(pool)
  mayAdd(tenant)
  if (db.images.some((i) => i.tenant_id === tenant && i.name === name)) throw conflict('image already exists')
  const i: ImageRec = {
    id: newId(),
    tenant_id: tenant,
    name,
    origin: url ? 'download' : 'upload',
    format: body.format,
    os,
    shared: !!body.shared,
    secure_boot: !!body.secure_boot,
    tpm: !!body.tpm,
    sleep: body.sleep ?? true,
    admin_user: adminUser,
    created_at: now(),
    state: url ? 'downloading' : 'uploading',
    error: null,
    size_bytes: null,
    virtual_bytes: null,
    sha256: null,
    digest: null,
    file_bytes: body.upload_bytes ?? null,
    received_bytes: 0,
    hosts: [],
    storage_pool_id: pool.id,
  }
  db.images.push(i)
  emit('image', i.id, 'insert')
  if (url) download(i.id, host.id)
  else uploadHosts.set(i.id, host.id)
  audit(
    'image.create',
    ['image', i.id],
    {
      name: i.name,
      url,
      upload_bytes: body.upload_bytes ?? null,
      storage_pool: pool.name,
      os: i.os,
      secure_boot: i.secure_boot,
      tpm: i.tpm,
      sleep: i.sleep,
      admin_user: i.admin_user,
    },
    tenant,
  )
  return created(imageView(i))
})

/** The most one request may carry of an upload. */
const MAX_PART = 32 << 20

route.put('/api/v1/images/{id}/upload', ({ params, query, body }) => {
  const i = find(db.images, params.id, 'image')
  authorize('image.write', i.tenant_id)
  if (i.origin !== 'upload') throw conflict('the image is downloaded, not uploaded')
  if (i.state === 'failed') {
    throw conflict(`uploading it failed (${i.error ?? 'no reason given'}); delete it and add it again`)
  }
  if (i.state !== 'uploading') throw conflict('all of the image has been uploaded')
  const total = i.file_bytes ?? 0
  const here = i.received_bytes ?? 0
  if (query.offset !== here) {
    throw conflict(`${here} bytes of the image have arrived: send the part from offset ${here}`)
  }
  const part = body instanceof Uint8Array ? body.length : 0
  if (part > MAX_PART) throw new HttpError(413, 'too_large', `a part is at most ${MAX_PART >> 20} MiB`)
  const end = here + part
  if (end > total) throw badRequest(`the part goes beyond the image's ${total} bytes`)
  if (part === 0) throw badRequest('the part is empty')
  i.received_bytes = end
  if (end === total) {
    const host = uploadHosts.get(i.id) ?? db.hosts.find((h) => h.state === 'ready' && h.connected)?.id
    if (!host) throw conflict('the host the upload went to is gone; delete it and add it again')
    uploadHosts.delete(i.id)
    store(i, host)
    audit('image.upload', ['image', i.id], { name: i.name, bytes: total }, i.tenant_id)
  } else {
    emit('image', i.id)
  }
  return imageView(i)
})

route.delete('/api/v1/images/{id}', ({ params }) => {
  const i = find(db.images, params.id, 'image')
  authorize('image.write', i.tenant_id)
  // VMs made from it keep their disks, which are copies of their own.
  remove(db.images, (x) => x.id === i.id)
  uploadHosts.delete(i.id)
  audit('image.delete', ['image', i.id], { name: i.name }, i.tenant_id)
  emit('image', i.id, 'delete')
  return none()
})
