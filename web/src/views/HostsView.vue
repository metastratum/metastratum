<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { api, ApiError, errorText, ok, type Cluster, type Host, type Vm } from '../api/client'
import FencingForm from '../components/FencingForm.vue'
import HostFaceplate from '../components/HostFaceplate.vue'
import InfraTabs from '../components/InfraTabs.vue'
import ModalDialog from '../components/ModalDialog.vue'
import SplitButton from '../components/SplitButton.vue'
import TypeToConfirm from '../components/TypeToConfirm.vue'
import { useLiveRefresh } from '../stores/live'

const hosts = ref<Host[] | null>(null)
const inService = computed(() => (hosts.value ?? []).filter((h) => h.state !== 'decommissioned'))
const retired = computed(() => (hosts.value ?? []).filter((h) => h.state === 'decommissioned'))
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
async function evacuate() {
  const h = managing.value
  if (!h) return
  manageError.value = ''
  try {
    await ok(api.POST('/api/v1/hosts/{id}/evacuate', { params: { path: { id: h.id } } }))
    managing.value = null
    await load()
  } catch (e) {
    manageError.value = errorText(e)
  }
}

// Decommissioning: it can't be undone, so the host's name must be typed,
// and what it did is shown afterwards.
type DecommissionReport = {
  host_cleanup: string[]
  host_failures: string[]
  certificates_revoked: number
  notes: string[]
}
// Unlink retires the host in metastratum only; Delete also wipes metastratum from
// the machine.
const decommissioning = ref<Host | null>(null)
const wipe = ref(false)
const decommissionConfirmed = ref(false)
const decommissionBusy = ref(false)
const decommissionError = ref('')
const decommissionReport = ref<DecommissionReport | null>(null)
function startDecommission(h: Host, wiping: boolean) {
  wipe.value = wiping
  decommissionConfirmed.value = false
  managing.value = null
  decommissionError.value = ''
  decommissionReport.value = null
  decommissioning.value = h
}
async function decommission() {
  const h = decommissioning.value
  if (!h) return
  decommissionError.value = ''
  decommissionBusy.value = true
  try {
    decommissionReport.value = await ok(
      api.POST('/api/v1/hosts/{id}/decommission', { params: { path: { id: h.id }, query: { wipe: wipe.value } } }),
    )
    await load()
  } catch (e) {
    decommissionError.value = errorText(e)
  } finally {
    decommissionBusy.value = false
  }
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
    <template v-else-if="hosts">
      <div class="rack">
        <HostFaceplate v-for="h in inService" :key="h.id" :host="h" :vms="byHost.get(h.id) ?? []" @manage="manage(h)" />
      </div>
      <template v-if="retired.length">
        <h2 class="retired-head">Decommissioned</h2>
        <p class="muted">Taken out of service for good, and kept here for the record.</p>
        <div class="rack retired">
          <HostFaceplate v-for="h in retired" :key="h.id" :host="h" :vms="[]" />
        </div>
      </template>
    </template>

    <ModalDialog v-if="adding" title="Add host" @close="adding = false">
      <div class="body">
        <p v-if="tokenError" class="error-text">{{ tokenError }}</p>
        <template v-else-if="token">
          <p>On the new hypervisor, install <span class="mono">mst-agent</span> and start it once with this token:</p>
          <pre class="cmd mono">mst-agent --controller https://CONTROLLER:8443 \
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

    <ModalDialog
      v-if="decommissioning"
      :title="`${wipe ? 'Delete' : 'Unlink'} ${decommissioning.name}`"
      @close="decommissioning = null"
    >
      <div v-if="decommissionReport" class="form">
        <p>
          <strong>{{ decommissioning.name }}</strong> is decommissioned;
          {{ decommissionReport.certificates_revoked }} certificates were revoked.
        </p>
        <template v-if="decommissionReport.host_cleanup.length">
          <span class="label">Removed from the host</span>
          <ul class="report">
            <li v-for="s in decommissionReport.host_cleanup" :key="s">{{ s }}</li>
          </ul>
        </template>
        <template v-if="decommissionReport.host_failures.length">
          <span class="label error-text">Failed on the host</span>
          <ul class="report error-text">
            <li v-for="s in decommissionReport.host_failures" :key="s">{{ s }}</li>
          </ul>
        </template>
        <ul v-if="decommissionReport.notes.length" class="report">
          <li v-for="n in decommissionReport.notes" :key="n">{{ n }}</li>
        </ul>
        <footer><button class="btn primary" @click="decommissioning = null">Done</button></footer>
      </div>
      <form v-else class="form" @submit.prevent="decommission">
        <template v-if="!wipe">
          <p>
            Unlink <strong>{{ decommissioning.name }}</strong>? This can’t be undone: metastratum retires the host, and
            leaves the machine as it is.
          </p>
          <ul class="report">
            <li>The host leaves its cluster and is listed as decommissioned.</li>
            <li class="error-text">
              Its certificates are revoked: its agent can no longer connect, and other hosts no longer trust it.
            </li>
            <li>
              Nothing on the machine changes: metastratum’s files, mounts, TLS settings and its agent stay, and so does
              OVN.
            </li>
            <li>
              To use the machine with metastratum again, remove the agent’s identity
              (<span class="mono">/var/lib/metastratum-agent</span>) and enroll it anew.
            </li>
          </ul>
          <p class="muted">To also remove metastratum from the machine, choose Delete instead.</p>
        </template>
        <template v-else>
          <p>
            Delete <strong>{{ decommissioning.name }}</strong>? This can’t be undone: metastratum retires the host and is
            removed from the machine.
          </p>
          <ul class="report">
            <li>The host leaves its cluster and its certificates are revoked.</li>
            <li class="error-text">
              Storage pools are unmounted, and cached images, local volumes, cloud-init seeds, metastratum’s TLS files
              and libvirt secrets are deleted.
            </li>
            <li>
              OVN stays as you set it up; only what metastratum changed goes back (the southbound address, and
              metastratum’s TLS settings).
            </li>
            <li>The agent removes its identity, then stops and disables itself.</li>
            <li v-if="!decommissioning.connected">
              The host is not connected, so nothing on the machine can be removed now; it is only retired here.
            </li>
          </ul>
          <p class="muted">To leave the machine as it is, choose Unlink instead.</p>
        </template>
        <TypeToConfirm v-model="decommissionConfirmed" :name="decommissioning.name" />
        <p v-if="decommissionError" class="error-text" role="alert">{{ decommissionError }}</p>
        <footer>
          <button type="button" class="btn" @click="decommissioning = null">Cancel</button>
          <button class="btn danger" :disabled="!decommissionConfirmed || decommissionBusy">
            {{ decommissionBusy ? 'Working…' : `${wipe ? 'Delete' : 'Unlink'} ${decommissioning.name}` }}
          </button>
        </footer>
      </form>
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
        <p v-if="managing.state === 'fenced'" class="error-text hint">
          It failed and was powered off; its VMs moved. Once it is repaired and its agent is back, saving puts it back
          in service.
        </p>
        <div class="evacuate">
          <div>
            <strong>Evacuate</strong>
            <p class="muted hint">
              Puts the host in maintenance and live-migrates its running VMs to other hosts of its cluster, one at a
              time. Stopped VMs, and VMs that are not migratable, stay. Follow it in Tasks.
            </p>
          </div>
          <button
            type="button"
            class="btn"
            :disabled="!managing.cluster_id || !managing.vm_count"
            :title="!managing.cluster_id ? 'The host is in no cluster' : !managing.vm_count ? 'No VMs to move' : undefined"
            @click="evacuate"
          >
            Evacuate {{ managing.name }}
          </button>
        </div>
        <div v-if="managing.state !== 'decommissioned'" class="evacuate">
          <div>
            <strong>Decommission</strong>
            <p class="muted hint">
              Takes the host out of service for good. Unlink retires it here and leaves the machine alone; Delete also
              removes metastratum from the machine. It must have no VMs.
            </p>
          </div>
          <SplitButton
            label="Unlink…"
            :items="[{ key: 'delete', label: 'Delete…', danger: true }]"
            :menu-label="`More ways to decommission ${managing.name}`"
            :disabled="!!managing.vm_count"
            :title="managing.vm_count ? 'Move or delete its VMs first' : undefined"
            @main="startDecommission(managing, false)"
            @select="startDecommission(managing, true)"
          />
        </div>
        <p v-if="manageError" class="error-text" role="alert">{{ manageError }}</p>
        <footer>
          <button type="button" class="btn" @click="managing = null">Cancel</button>
          <button class="btn primary">Save</button>
        </footer>
      </form>
      <div class="fence-wrap"><FencingForm :host="managing" /></div>
    </ModalDialog>
  </section>
</template>

<style scoped>
.retired-head {
  margin: 28px 0 4px;
}
.rack.retired {
  opacity: 0.6;
}
.report {
  margin: 0;
  padding-left: 18px;
  display: grid;
  gap: 6px;
}
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
.fence-wrap {
  padding: 0 18px 18px;
}
.evacuate {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 10px 16px;
  padding: 12px 14px;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
}
.evacuate > div {
  flex: 1;
  min-width: 220px;
}
</style>
