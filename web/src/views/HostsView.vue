<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { api, ApiError, errorText, ok, type Cluster, type Host, type Vm } from '../api/client'
import HostFaceplate from '../components/HostFaceplate.vue'
import InfraTabs from '../components/InfraTabs.vue'
import ModalDialog from '../components/ModalDialog.vue'
import { useLiveRefresh } from '../stores/live'

const hosts = ref<Host[] | null>(null)
const vms = ref<Vm[]>([])
const token = ref<{ token: string; expires_at: string } | null>(null)
const tokenError = ref('')
const adding = ref(false)
const clusters = ref<Cluster[]>([])
const joinCluster = ref<string | null>(null)

async function load() {
  ;[hosts.value, vms.value, clusters.value] = await Promise.all([
    ok(api.GET('/api/v1/hosts')),
    ok(api.GET('/api/v1/vms', { params: { query: { descendants: true } } })),
    ok(api.GET('/api/v1/clusters')),
  ])
}

// Moving a host between clusters, and maintenance mode.
const managing = ref<Host | null>(null)
const manageForm = ref({ cluster_id: null as string | null, maintenance: false })
const manageError = ref('')
function manage(h: Host) {
  manageForm.value = { cluster_id: h.cluster_id ?? null, maintenance: h.state === 'maintenance' }
  manageError.value = ''
  managing.value = h
}
async function saveHost() {
  const h = managing.value
  if (!h) return
  manageError.value = ''
  try {
    await ok(
      api.PATCH('/api/v1/hosts/{id}', {
        params: { path: { id: h.id } },
        body: {
          ...(manageForm.value.cluster_id !== (h.cluster_id ?? null) && { cluster_id: manageForm.value.cluster_id }),
          state: manageForm.value.maintenance ? 'maintenance' : 'ready',
        },
      }),
    )
    managing.value = null
    await load()
  } catch (e) {
    manageError.value = errorText(e)
  }
}

onMounted(load)
useLiveRefresh(['host', 'vm', 'cluster'], load)

const byHost = computed(() => {
  const m = new Map<string, Vm[]>()
  for (const vm of vms.value) if (vm.host_id) m.set(vm.host_id, [...(m.get(vm.host_id) ?? []), vm])
  return m
})

function addHost() {
  adding.value = true
  token.value = null
  tokenError.value = ''
  joinCluster.value = clusters.value.length === 1 ? clusters.value[0]!.id : null
}
async function createToken() {
  tokenError.value = ''
  try {
    token.value = await ok(
      api.POST('/api/v1/hosts/join-tokens', { body: { ttl_minutes: 60, cluster_id: joinCluster.value } }),
    )
  } catch (e) {
    tokenError.value = e instanceof ApiError ? e.message : 'Could not create a join token.'
  }
}
</script>

<template>
  <section>
    <InfraTabs />
    <header class="head">
      <h1>Hosts</h1>
      <button class="btn primary" @click="addHost">Add host</button>
    </header>
    <p class="intro">Each bar is a host’s memory. VMs take up space in proportion to what they were given.</p>

    <div v-if="hosts && !hosts.length" class="empty">
      <p>No hosts are enrolled. Add a hypervisor so VMs have somewhere to run.</p>
      <button class="btn primary" @click="addHost">Add host</button>
    </div>
    <div v-else-if="hosts" class="rack">
      <HostFaceplate v-for="h in hosts" :key="h.id" :host="h" :vms="byHost.get(h.id) ?? []" @manage="manage(h)" />
    </div>

    <ModalDialog v-if="adding" title="Add host" @close="adding = false">
      <div class="body">
        <p v-if="tokenError" class="error-text">{{ tokenError }}</p>
        <template v-else-if="token">
          <p>On the new hypervisor, install <span class="mono">hm-agent</span> and start it once with this token:</p>
          <pre class="cmd mono">hm-agent --controller https://CONTROLLER:8443 \
  --join-token {{ token.token }}</pre>
          <p class="muted">
            The token works for one host and expires {{ new Date(token.expires_at).toLocaleTimeString() }}. The host
            appears here as soon as it connects.
          </p>
        </template>
        <form v-else class="join" @submit.prevent="createToken">
          <label class="field">
            <span class="label">Cluster</span>
            <select v-model="joinCluster">
              <option :value="null">None yet</option>
              <option v-for="c in clusters" :key="c.id" :value="c.id">{{ c.name }}</option>
            </select>
            <span class="hint">The host joins this cluster when it enrolls. Without one, no VMs are placed on it.</span>
          </label>
          <button class="btn primary">Create join token</button>
        </form>
        <footer v-if="token || tokenError">
          <button class="btn" @click="adding = false">Done</button>
        </footer>
      </div>
    </ModalDialog>

    <ModalDialog v-if="managing" :title="`Manage ${managing.name}`" @close="managing = null">
      <form class="form" @submit.prevent="saveHost">
        <label class="field">
          <span class="label">Cluster</span>
          <select v-model="manageForm.cluster_id">
            <option :value="null">None</option>
            <option v-for="c in clusters" :key="c.id" :value="c.id">{{ c.name }}</option>
          </select>
          <span class="hint">A host with VMs can’t change clusters until they are moved or deleted.</span>
        </label>
        <label class="check">
          <input v-model="manageForm.maintenance" type="checkbox" />
          Maintenance: place no new VMs here
        </label>
        <p class="muted hint">VMs already on the host keep running.</p>
        <p v-if="manageError" class="error-text" role="alert">{{ manageError }}</p>
        <footer>
          <button type="button" class="btn" @click="managing = null">Cancel</button>
          <button class="btn primary">Save</button>
        </footer>
      </form>
    </ModalDialog>
  </section>
</template>

<style scoped>
.head {
  display: flex;
  align-items: baseline;
  justify-content: space-between;
  margin-bottom: 8px;
}
h1 {
  font-size: 22px;
}
.intro {
  margin: 0 0 20px;
  color: var(--muted);
}
.rack {
  display: grid;
  gap: 10px;
}
.empty {
  display: grid;
  gap: 14px;
  justify-items: start;
  padding: 32px;
  border: 1px dashed var(--rule);
  border-radius: var(--radius);
}
.empty p {
  margin: 0;
  color: var(--muted);
}
.body {
  display: grid;
  gap: 12px;
  padding: 18px;
}
.body p {
  margin: 0;
}
.cmd {
  margin: 0;
  padding: 12px;
  overflow-x: auto;
  border-radius: var(--radius);
  background: var(--sunken);
  white-space: pre;
}
.muted {
  color: var(--muted);
}
footer {
  display: flex;
  justify-content: flex-end;
}
.join {
  display: grid;
  gap: 14px;
  justify-items: start;
}
.join .field {
  width: 100%;
}
.hint {
  margin: 0;
  font-size: 12px;
}
</style>
