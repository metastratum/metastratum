<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { api, errorText, ok, type Cluster, type Pool } from '../api/client'
import InfraTabs from '../components/InfraTabs.vue'
import ModalDialog from '../components/ModalDialog.vue'
import SplitButton from '../components/SplitButton.vue'
import TypeToConfirm from '../components/TypeToConfirm.vue'
import { useLiveRefresh } from '../stores/live'

const pools = ref<Pool[] | null>(null)
const clusters = ref<Cluster[]>([])
const error = ref('')

async function load() {
  ;[pools.value, clusters.value] = await Promise.all([
    ok(api.GET('/api/v1/storage-pools')),
    ok(api.GET('/api/v1/clusters')),
  ])
}
onMounted(load)
useLiveRefresh(['storage_pool', 'cluster'], load)

const clusterName = (id: string) => clusters.value.find((c) => c.id === id)?.name ?? id.slice(0, 8)
type Config = Record<string, unknown>
const cfg = (p: Pool) => (p.config ?? {}) as Config

/** The storage behind a pool, as the controller compares them. */
function backend(p: Pool): string | null {
  const c = cfg(p)
  if (p.kind === 'nfs') return `nfs:${c.server}:${String(c.export ?? '').replace(/\/+$/, '')}`
  if (p.kind === 'rbd') return `rbd:${c.pool}@${[...((c.monitors as string[] | undefined) ?? [])].sort().join(',')}`
  return null
}
/** Other pools on the same storage: deleting one leaves the data for them. */
const sharing = (p: Pool) =>
  (pools.value ?? []).filter((o) => o.id !== p.id && backend(o) !== null && backend(o) === backend(p))
const confirmed = ref(false)
function where(p: Pool): string {
  const c = cfg(p)
  if (p.kind === 'nfs') return `${c.server}:${c.export}`
  if (p.kind === 'rbd') return `${c.pool} @ ${(c.monitors as string[] | undefined)?.join(', ')}`
  return 'Each host’s own disk'
}

// Add, edit, delete.
type Dialog = { kind: 'edit'; pool?: Pool } | { kind: 'unlink' | 'delete'; pool: Pool }
function remove(pool: Pool, kind: 'unlink' | 'delete') {
  error.value = ''
  confirmed.value = false
  dialog.value = { kind, pool }
}
const dialog = ref<Dialog | null>(null)
const busy = ref(false)
const form = ref({
  name: '',
  kind: 'nfs',
  cluster_ids: [] as string[],
  server: '',
  export: '',
  options: '',
  monitors: '',
  pool: '',
  user: '',
  key: '',
})
function edit(p?: Pool) {
  const c = p ? cfg(p) : {}
  form.value = {
    name: p?.name ?? '',
    kind: p?.kind ?? 'nfs',
    cluster_ids: [...(p?.cluster_ids ?? [])],
    server: (c.server as string) ?? '',
    export: (c.export as string) ?? '',
    options: (c.options as string) ?? '',
    monitors: ((c.monitors as string[]) ?? []).join(', '),
    pool: (c.pool as string) ?? '',
    user: (c.user as string) ?? '',
    key: '',
  }
  error.value = ''
  dialog.value = { kind: 'edit', pool: p }
}
function config(): Config {
  const f = form.value
  if (f.kind === 'nfs') return { server: f.server.trim(), export: f.export.trim(), ...(f.options.trim() && { options: f.options.trim() }) }
  return {
    monitors: f.monitors.split(/[\s,]+/).filter(Boolean),
    pool: f.pool.trim(),
    user: f.user.trim(),
    // Omitted on edit to keep the stored key.
    ...(f.key.trim() && { key: f.key.trim() }),
  }
}
async function submit() {
  const d = dialog.value
  if (!d) return
  error.value = ''
  busy.value = true
  try {
    if (d.kind === 'unlink' || d.kind === 'delete') {
      await ok(
        api.DELETE('/api/v1/storage-pools/{id}', {
          params: { path: { id: d.pool.id }, query: { delete_data: d.kind === 'delete' } },
        }),
      )
    } else {
      const body = { name: form.value.name, kind: form.value.kind, cluster_ids: form.value.cluster_ids, config: config() }
      if (d.pool) await ok(api.PUT('/api/v1/storage-pools/{id}', { params: { path: { id: d.pool.id } }, body }))
      else await ok(api.POST('/api/v1/storage-pools', { body }))
    }
    dialog.value = null
    await load()
  } catch (e) {
    error.value = errorText(e)
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <section>
    <InfraTabs />
    <header class="page-head">
      <h1>Storage</h1>
      <div class="actions"><button class="btn primary" @click="edit()">Add storage pool</button></div>
    </header>
    <p class="intro">
      Where VM disks live. Local disks are on each host. Shared pools (NFS, Ceph RBD) are reachable from every host
      in the clusters they are attached to, so tenants given them can keep disks off the host.
    </p>

    <div v-if="pools" class="table-wrap">
      <table class="data">
        <thead>
          <tr>
            <th>Name</th>
            <th>Type</th>
            <th>Location</th>
            <th>Clusters</th>
            <th class="num">Volumes</th>
            <th />
          </tr>
        </thead>
        <tbody>
          <tr v-for="p in pools" :key="p.id">
            <td><strong>{{ p.name }}</strong></td>
            <td><span class="tag">{{ p.kind }}</span></td>
            <td class="mono">{{ where(p) }}</td>
            <td>
              <template v-if="p.kind === 'local'"><span class="muted">All hosts</span></template>
              <template v-else-if="p.cluster_ids.length">{{ p.cluster_ids.map(clusterName).join(', ') }}</template>
              <span v-else class="error-text">None — unusable</span>
            </td>
            <td class="num mono">{{ p.volumes }}</td>
            <td class="end">
              <template v-if="p.kind !== 'local'">
                <button class="btn small" @click="edit(p)">Edit</button>
                <SplitButton
                  label="Unlink…"
                  small
                  :items="[{ key: 'delete', label: 'Delete…', danger: true }]"
                  :menu-label="`More ways to remove ${p.name}`"
                  @main="remove(p, 'unlink')"
                  @select="remove(p, 'delete')"
                />
              </template>
            </td>
          </tr>
        </tbody>
      </table>
    </div>

    <ModalDialog
      v-if="dialog"
      :title="
        dialog.kind === 'unlink'
          ? `Unlink ${dialog.pool.name}`
          : dialog.kind === 'delete'
            ? `Delete ${dialog.pool.name}`
            : dialog.pool
              ? `Edit ${dialog.pool.name}`
              : 'Add storage pool'
      "
      @close="dialog = null"
    >
      <form class="form" @submit.prevent="submit">
        <template v-if="dialog.kind === 'unlink'">
          <p>Unlink <strong>{{ dialog.pool.name }}</strong>? It must hold no VM disks.</p>
          <ul class="consequences">
            <li>metastratum forgets the pool: tenants can no longer put disks on it.</li>
            <li>Every host unmounts it and forgets its credentials.</li>
            <li>
              Its storage is left exactly as it is: metastratum’s cached images and anything else on
              <span class="mono">{{ where(dialog.pool) }}</span> stay. Adding the pool again later finds them there.
            </li>
          </ul>
          <p class="muted">To also remove metastratum’s data from the storage, choose Delete instead.</p>
        </template>
        <template v-else-if="dialog.kind === 'delete'">
          <p>
            Delete <strong>{{ dialog.pool.name }}</strong>? It must hold no VM disks. This can’t be undone:
          </p>
          <ul class="consequences">
            <li>metastratum forgets the pool, and every host unmounts it and forgets its credentials.</li>
            <li v-if="sharing(dialog.pool).length">
              The data on its storage stays: {{ sharing(dialog.pool).map((p) => p.name).join(', ') }} uses the same
              storage.
            </li>
            <li v-else-if="dialog.pool.kind === 'nfs'" class="error-text">
              metastratum’s files on <span class="mono">{{ cfg(dialog.pool).server }}:{{ cfg(dialog.pool).export }}</span>
              are deleted: its cached images (<span class="mono">images/</span>) and any volumes left. Other files
              there are left alone.
            </li>
            <li v-else class="error-text">
              metastratum’s images in Ceph pool <span class="mono">{{ cfg(dialog.pool).pool }}</span> are deleted
              (<span class="mono">img-*</span>). Other images there are left alone.
            </li>
          </ul>
          <p class="muted">To keep the data, choose Unlink instead.</p>
          <TypeToConfirm v-model="confirmed" :name="dialog.pool.name" />
        </template>
        <template v-else>
          <div class="row">
            <label class="field">
              <span class="label">Name</span>
              <input v-model="form.name" required maxlength="64" autofocus />
            </label>
            <label class="field">
              <span class="label">Type</span>
              <select v-model="form.kind" :disabled="!!dialog.pool">
                <option value="nfs">NFS</option>
                <option value="rbd">Ceph RBD</option>
              </select>
            </label>
          </div>
          <template v-if="form.kind === 'nfs'">
            <div class="row">
              <label class="field">
                <span class="label">Server</span>
                <input v-model="form.server" required placeholder="10.0.0.5" class="mono" />
              </label>
              <label class="field">
                <span class="label">Export</span>
                <input v-model="form.export" required placeholder="/srv/vms" class="mono" />
              </label>
            </div>
            <label class="field">
              <span class="label">Mount options</span>
              <input v-model="form.options" placeholder="vers=4.2,hard,noatime" class="mono" />
            </label>
          </template>
          <template v-else>
            <label class="field">
              <span class="label">Monitors</span>
              <input v-model="form.monitors" required placeholder="10.0.0.10:6789, 10.0.0.11:6789" class="mono" />
            </label>
            <div class="row">
              <label class="field">
                <span class="label">Pool</span>
                <input v-model="form.pool" required class="mono" />
              </label>
              <label class="field">
                <span class="label">Ceph user</span>
                <input v-model="form.user" required placeholder="metastratum" class="mono" />
              </label>
            </div>
            <label class="field">
              <span class="label">Key</span>
              <input
                v-model="form.key"
                type="password"
                autocomplete="off"
                :required="!dialog.pool"
                :placeholder="dialog.pool ? 'Unchanged' : 'AQB…'"
                class="mono"
              />
              <span class="hint">The cephx key of that user. Never shown again.</span>
            </label>
          </template>
          <fieldset class="clusters">
            <legend class="label">Attached to</legend>
            <label v-for="c in clusters" :key="c.id" class="check">
              <input v-model="form.cluster_ids" type="checkbox" :value="c.id" />
              {{ c.name }}
            </label>
            <p v-if="!clusters.length" class="muted">Add a cluster first.</p>
            <p class="hint muted">Every host in these clusters must be able to reach the storage.</p>
          </fieldset>
        </template>
        <p v-if="error" class="error-text" role="alert">{{ error }}</p>
        <footer>
          <button type="button" class="btn" @click="dialog = null">Cancel</button>
          <button
            class="btn"
            :class="dialog.kind === 'delete' ? 'danger' : 'primary'"
            :disabled="busy || (dialog.kind === 'delete' && !confirmed)"
          >
            {{ dialog.kind === 'delete' ? 'Delete pool' : dialog.kind === 'unlink' ? 'Unlink pool' : 'Save' }}
          </button>
        </footer>
      </form>
    </ModalDialog>
  </section>
</template>

<style scoped>
.consequences {
  margin: 0;
  padding-left: 18px;
  display: grid;
  gap: 6px;
}
.end .btn + .btn {
  margin-left: 6px;
}
.clusters {
  display: grid;
  gap: 8px;
  margin: 0;
  padding: 12px 14px;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
}
.clusters p {
  margin: 0;
}
.hint {
  font-size: 12px;
}
</style>
