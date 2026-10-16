<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { api, errorText, ok, type Cluster, type Host } from '../api/client'
import InfraTabs from '../components/InfraTabs.vue'
import ModalDialog from '../components/ModalDialog.vue'
import { mib } from '../state'
import { useLiveRefresh } from '../stores/live'

const clusters = ref<Cluster[] | null>(null)
const hosts = ref<Host[]>([])
const error = ref('')

async function load() {
  ;[clusters.value, hosts.value] = await Promise.all([ok(api.GET('/api/v1/clusters')), ok(api.GET('/api/v1/hosts'))])
}
onMounted(load)
useLiveRefresh(['cluster', 'host'], load)

const members = computed(() => {
  const m = new Map<string, Host[]>()
  for (const h of hosts.value) if (h.cluster_id) m.set(h.cluster_id, [...(m.get(h.cluster_id) ?? []), h])
  return m
})
const unassigned = computed(() => hosts.value.filter((h) => !h.cluster_id))
const capacity = (hs: Host[]) => ({
  cpus: hs.reduce((n, h) => n + h.cpu_count, 0),
  memory: hs.reduce((n, h) => n + h.memory_mib, 0),
})

// Create, edit, delete.
type Dialog = { kind: 'edit'; cluster?: Cluster } | { kind: 'delete'; cluster: Cluster }
const dialog = ref<Dialog | null>(null)
const form = ref({ name: '', cpu_overcommit: 4, memory_overcommit: 1 })
const busy = ref(false)
function edit(c?: Cluster) {
  form.value = c
    ? { name: c.name, cpu_overcommit: c.cpu_overcommit, memory_overcommit: c.memory_overcommit }
    : { name: '', cpu_overcommit: 4, memory_overcommit: 1 }
  error.value = ''
  dialog.value = { kind: 'edit', cluster: c }
}
async function submit() {
  const d = dialog.value
  if (!d) return
  error.value = ''
  busy.value = true
  try {
    if (d.kind === 'delete') {
      await ok(api.DELETE('/api/v1/clusters/{id}', { params: { path: { id: d.cluster.id } } }))
    } else if (d.cluster) {
      await ok(api.PUT('/api/v1/clusters/{id}', { params: { path: { id: d.cluster.id } }, body: form.value }))
    } else {
      await ok(api.POST('/api/v1/clusters', { body: form.value }))
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
      <h1>Clusters</h1>
      <div class="actions"><button class="btn primary" @click="edit()">Add cluster</button></div>
    </header>
    <p class="intro">
      A cluster is a group of hosts that VMs are placed on. Tenants are given clusters to use; overcommit sets how
      much more vCPU and memory VMs may be given than the hosts have.
    </p>

    <div v-if="clusters && !clusters.length" class="empty-state">
      <p>No clusters yet. Add one, then move hosts into it from the Hosts page.</p>
      <button class="btn primary" @click="edit()">Add cluster</button>
    </div>
    <ul v-else-if="clusters" class="clusters">
      <li v-for="c in clusters" :key="c.id" class="cluster">
        <header>
          <h2>{{ c.name }}</h2>
          <span class="mono muted">
            {{ capacity(members.get(c.id) ?? []).cpus }} cpu ·
            {{ mib(capacity(members.get(c.id) ?? []).memory) }}
          </span>
          <span class="tag">cpu ×{{ c.cpu_overcommit }}</span>
          <span class="tag">mem ×{{ c.memory_overcommit }}</span>
          <div class="buttons">
            <button class="btn small" @click="edit(c)">Edit</button>
            <button class="btn small danger" @click="(error = ''), (dialog = { kind: 'delete', cluster: c })">Delete</button>
          </div>
        </header>
        <ul class="members">
          <li v-for="h in members.get(c.id) ?? []" :key="h.id" class="mono">
            <span class="dot" :class="{ off: !h.connected, maint: h.state === 'maintenance' }" aria-hidden="true" />
            {{ h.name }}
            <span v-if="h.state === 'maintenance'" class="muted">maintenance</span>
          </li>
          <li v-if="!(members.get(c.id) ?? []).length" class="muted">No hosts.</li>
        </ul>
      </li>
    </ul>
    <p v-if="unassigned.length" class="muted unassigned">
      Not in a cluster, so nothing is placed on them:
      <span class="mono">{{ unassigned.map((h) => h.name).join(', ') }}</span>.
      <RouterLink to="/hosts">Assign them</RouterLink>.
    </p>

    <ModalDialog
      v-if="dialog"
      :title="dialog.kind === 'delete' ? 'Delete cluster' : dialog.cluster ? `Edit ${dialog.cluster.name}` : 'Add cluster'"
      @close="dialog = null"
    >
      <form class="form" @submit.prevent="submit">
        <p v-if="dialog.kind === 'delete'">
          Delete <strong>{{ dialog.cluster.name }}</strong>? It must have no hosts left, and tenants lose access to it.
        </p>
        <template v-else>
          <label class="field">
            <span class="label">Name</span>
            <input v-model="form.name" required maxlength="64" autofocus />
          </label>
          <div class="row">
            <label class="field">
              <span class="label">vCPU overcommit</span>
              <input v-model.number="form.cpu_overcommit" type="number" min="1" max="64" step="0.5" required />
              <span class="hint">vCPUs per physical CPU.</span>
            </label>
            <label class="field">
              <span class="label">Memory overcommit</span>
              <input v-model.number="form.memory_overcommit" type="number" min="1" max="4" step="0.1" required />
              <span class="hint">1 means never more than the host has.</span>
            </label>
          </div>
        </template>
        <p v-if="error" class="error-text" role="alert">{{ error }}</p>
        <footer>
          <button type="button" class="btn" @click="dialog = null">Cancel</button>
          <button class="btn" :class="dialog.kind === 'delete' ? 'danger' : 'primary'" :disabled="busy">
            {{ dialog.kind === 'delete' ? 'Delete cluster' : 'Save' }}
          </button>
        </footer>
      </form>
    </ModalDialog>
  </section>
</template>

<style scoped>
.clusters {
  display: grid;
  gap: 10px;
  margin: 0;
  padding: 0;
  list-style: none;
}
.cluster {
  padding: 14px 16px;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
  background: var(--panel);
}
.cluster header {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 10px;
}
.cluster h2 {
  font-size: 16px;
}
.buttons {
  display: flex;
  gap: 6px;
  margin-left: auto;
}
.members {
  display: flex;
  flex-wrap: wrap;
  gap: 6px 18px;
  margin: 12px 0 0;
  padding: 0;
  list-style: none;
}
.members li {
  display: flex;
  align-items: center;
  gap: 6px;
}
.dot {
  width: 7px;
  height: 7px;
  border-radius: 50%;
  background: var(--led-on);
}
.dot.maint {
  background: var(--led-busy);
}
.dot.off {
  background: var(--led-fault);
}
.unassigned {
  margin-top: 16px;
}
</style>
