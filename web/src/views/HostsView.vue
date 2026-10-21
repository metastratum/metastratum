<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { api, ApiError, errorText, ok, type Cluster, type Host, type Schemas, type Vm } from '../api/client'
import ClusterDialog from '../components/ClusterDialog.vue'
import FencingForm from '../components/FencingForm.vue'
import ModalDialog from '../components/ModalDialog.vue'
import SplitButton from '../components/SplitButton.vue'
import StatusLed from '../components/StatusLed.vue'
import TypeToConfirm from '../components/TypeToConfirm.vue'
import { mib, vmState } from '../state'
import { useLiveRefresh } from '../stores/live'

const hosts = ref<Host[] | null>(null)
const vms = ref<Vm[]>([])
const token = ref<Schemas['JoinTokenView'] | null>(null)
const tokenError = ref('')
const adding = ref(false)
const clusters = ref<Cluster[]>([])
const joinCluster = ref<string | null>(null)
const agentConfig = computed(() => {
  if (!token.value) return ''
  const controllers = token.value.controllers.length ? token.value.controllers : ['https://CONTROLLER:7821']
  return `controllers = [${controllers.map((c) => JSON.stringify(c)).join(', ')}]\njoin_token = ${JSON.stringify(token.value.token)}`
})

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
// What a host reported about itself that would give it more (a new name,
// address or chassis, physical networks, CPUs, memory) waits for this.
const FACT_LABELS: Record<string, string> = {
  name: 'Name',
  address: 'Address',
  ovn_chassis: 'OVN chassis',
  cpu_count: 'CPUs',
  memory_mib: 'Memory',
  physical_networks: 'Physical networks',
}
function pendingChanges(h: Host): { label: string; from: string; to: string }[] {
  const p = (h.pending_facts ?? {}) as Record<string, unknown>
  const show = (k: string, v: unknown) =>
    k === 'memory_mib' ? mib(Number(v)) : Array.isArray(v) ? v.join(', ') || 'none' : String(v)
  const current = h as unknown as Record<string, unknown>
  return Object.keys(p).map((k) => ({ label: FACT_LABELS[k] ?? k, from: show(k, current[k]), to: show(k, p[k]) }))
}
async function acceptFacts() {
  const h = managing.value
  if (!h) return
  manageError.value = ''
  try {
    await ok(api.POST('/api/v1/hosts/{id}/accept-facts', { params: { path: { id: h.id } } }))
    managing.value = null
    await load()
  } catch (e) {
    manageError.value = errorText(e)
  }
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

// A host's VMs, the largest first.
const byHost = computed(() => {
  const m = new Map<string, Vm[]>()
  for (const vm of vms.value) if (vm.host_id) m.set(vm.host_id, [...(m.get(vm.host_id) ?? []), vm])
  for (const list of m.values()) list.sort((a, b) => b.memory_mib - a.memory_mib)
  return m
})

// The hosts by cluster; then those in none, and those retired for good.
type Group = { key: string; title: string; cluster?: Cluster; note?: string; hosts: Host[] }
const groups = computed<Group[]>(() => {
  const all = hosts.value ?? []
  const live = all.filter((h) => h.state !== 'decommissioned')
  const out: Group[] = [...clusters.value]
    .sort((a, b) => a.name.localeCompare(b.name))
    .map((c) => ({ key: c.id, title: c.name, cluster: c, hosts: live.filter((h) => h.cluster_id === c.id) }))
  const loose = live.filter((h) => !h.cluster_id)
  if (loose.length) {
    out.push({ key: 'none', title: 'Not in a cluster', note: 'Nothing is placed on these hosts until they join a cluster.', hosts: loose })
  }
  const retired = all.filter((h) => h.state === 'decommissioned')
  if (retired.length) {
    out.push({ key: 'retired', title: 'Decommissioned', note: 'Taken out of service for good, and kept here for the record.', hosts: retired })
  }
  return out
})
const collapsed = ref(new Set<string>())
function toggle(key: string) {
  const s = new Set(collapsed.value)
  if (!s.delete(key)) s.add(key)
  collapsed.value = s
}
const summary = (g: Group) => {
  const n = g.hosts.reduce((sum, h) => sum + (byHost.value.get(h.id)?.length ?? 0), 0)
  return `${g.hosts.length} host${g.hosts.length === 1 ? '' : 's'} · ${n} VM${n === 1 ? '' : 's'}`
}
const totals = (g: Group) =>
  `${g.hosts.reduce((n, h) => n + h.cpu_count, 0)} cpu · ${mib(g.hosts.reduce((n, h) => n + h.memory_mib, 0))}`

type Tag = { label: string; title?: string; tone?: 'ha' | 'warn' | 'fault' }
function clusterTags(c: Cluster): Tag[] {
  const disk = [c.vm_disk_iops && `${c.vm_disk_iops} IOPS`, c.vm_disk_mbps && `${c.vm_disk_mbps} MB/s`].filter(Boolean)
  return [
    { label: `cpu ×${c.cpu_overcommit}` },
    { label: `mem ×${c.memory_overcommit}` },
    ...(c.vm_net_mbps ? [{ label: `net ${c.vm_net_mbps} Mbit/s`, title: 'Network limit per VM NIC, each way' }] : []),
    ...(disk.length ? [{ label: `disk ${disk.join(' · ')}`, title: 'Disk limits per VM disk' }] : []),
    ...(c.ha_enabled ? [{ label: 'HA', title: 'Failed hosts are fenced and their VMs restarted elsewhere', tone: 'ha' as const }] : []),
  ]
}
/** What keeps a host from taking every VM, or wants a hand. */
function hostTags(h: Host): Tag[] {
  if (h.state === 'decommissioned') return []
  const out: Tag[] = []
  if (!h.kvm) out.push({ label: 'No KVM', tone: 'fault' })
  if (h.state === 'maintenance') out.push({ label: 'Maintenance', title: 'No new VMs are placed here' })
  if (h.state === 'fenced') out.push({ label: 'Fenced', title: 'It failed and was powered off; its VMs were moved', tone: 'fault' })
  if (h.connected && !h.migration_ready) {
    out.push({ label: 'No migration', title: "The agent finds no group QEMU runs in, to give migrations' TLS keys to", tone: 'warn' })
  }
  if (h.connected && !h.tpm) out.push({ label: 'No TPM', title: 'swtpm is not installed: VMs with a TPM do not run here', tone: 'warn' })
  if (h.connected && !h.secure_boot) {
    out.push({ label: 'No Secure Boot', title: 'No OVMF firmware with Secure Boot: VMs with Secure Boot do not run here', tone: 'warn' })
  }
  if (h.pending_facts) {
    out.push({ label: 'Changes waiting', title: 'It reports changes about itself that wait for you to accept them (Manage)', tone: 'warn' })
  }
  if (h.cannot_run.length) {
    out.push({
      label: `${h.cannot_run.length} VM${h.cannot_run.length === 1 ? '' : 's'} can’t run here`,
      title: `Its CPU lacks features these VMs of its cluster use, so they do not move here: ${h.cannot_run.join(', ')}. Fit their CPU to the cluster (Settings) to change that.`,
      tone: 'warn',
    })
  }
  return out
}
const stateOf = (h: Host) =>
  h.state === 'decommissioned'
    ? { tone: 'off' as const, text: 'Decommissioned' }
    : h.connected
      ? { tone: 'on' as const, text: 'Connected' }
      : { tone: 'fault' as const, text: 'Not connected' }
/** Each VM's share of the host's memory. */
const share = (h: Host, vm: Vm) => `${((vm.memory_mib / Math.max(h.memory_mib, 1)) * 100).toFixed(2)}%`

// A host's VMs, a few at a time.
const VM_LIMIT = 4
const allVms = ref(new Set<string>())
function toggleVms(id: string) {
  const s = new Set(allVms.value)
  if (!s.delete(id)) s.add(id)
  allVms.value = s
}
const shownVms = (h: Host) => (byHost.value.get(h.id) ?? []).slice(0, allVms.value.has(h.id) ? undefined : VM_LIMIT)

// Clusters: add, edit, delete.
const clusterDialog = ref<{ cluster?: Cluster; remove?: boolean } | null>(null)
async function clusterSaved() {
  clusterDialog.value = null
  await load()
}

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
    <header class="page-head">
      <h1>Hosts &amp; clusters</h1>
      <div class="actions">
        <button class="btn" @click="clusterDialog = {}">Add cluster</button>
        <button class="btn primary" @click="addHost">Add host</button>
      </div>
    </header>
    <p class="intro">
      A cluster is a group of hosts that VMs are placed on. Tenants are given clusters to use; overcommit sets how much
      more vCPU and memory VMs may be given than the hosts have. Each bar is a host’s memory: VMs take up space in
      proportion to what they were given.
    </p>

    <div v-if="hosts && !hosts.length" class="empty-state">
      <p>No hosts are enrolled. Add a hypervisor so VMs have somewhere to run.</p>
      <button class="btn primary" @click="addHost">Add host</button>
    </div>
    <div v-if="hosts" class="groups">
      <section v-for="g in groups" :key="g.key" class="group">
        <header>
          <button
            type="button"
            class="fold"
            :aria-expanded="!collapsed.has(g.key)"
            :aria-label="`${collapsed.has(g.key) ? 'Show' : 'Hide'} ${g.title} hosts`"
            @click="toggle(g.key)"
          >
            <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true" :class="{ shut: collapsed.has(g.key) }">
              <path d="M3 4.5 6 7.5 9 4.5" fill="none" stroke="currentColor" stroke-width="1.5" />
            </svg>
          </button>
          <h2>{{ g.title }}</h2>
          <span class="mono muted">{{ summary(g) }}</span>
          <span v-if="g.cluster" class="mono muted">{{ totals(g) }}</span>
          <span v-for="t in g.cluster ? clusterTags(g.cluster) : []" :key="t.label" class="tag" :class="t.tone" :title="t.title">
            {{ t.label }}
          </span>
          <div v-if="g.cluster" class="buttons">
            <button class="btn small" @click="clusterDialog = { cluster: g.cluster }">Edit</button>
            <button class="btn small danger" @click="clusterDialog = { cluster: g.cluster, remove: true }">Delete</button>
          </div>
        </header>
        <p v-if="g.note" class="note muted">{{ g.note }}</p>
        <template v-if="!collapsed.has(g.key)">
          <p v-if="!g.hosts.length" class="note muted none">No hosts. Move hosts into it with Manage.</p>
          <div v-else class="scroll">
            <table class="data">
              <thead>
                <tr>
                  <th>Host</th>
                  <th>State</th>
                  <th>Address</th>
                  <th class="num">CPU</th>
                  <th>Memory</th>
                  <th>VMs</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                <tr
                  v-for="h in g.hosts"
                  :key="h.id"
                  :class="{ retired: h.state === 'decommissioned', down: h.state !== 'decommissioned' && !h.connected }"
                >
                  <td>
                    <div class="host">
                      <strong class="mono">{{ h.name }}</strong>
                      <span class="platform">
                        {{ h.cpu_model || 'unknown cpu' }} · libvirt {{ h.libvirt_version }} · agent {{ h.agent_version }}
                      </span>
                      <div v-if="hostTags(h).length" class="tags">
                        <span v-for="t in hostTags(h)" :key="t.label" class="tag" :class="t.tone" :title="t.title">
                          {{ t.label }}
                        </span>
                      </div>
                    </div>
                  </td>
                  <td class="nowrap"><StatusLed :tone="stateOf(h).tone" :label="stateOf(h).text" /></td>
                  <td class="mono">{{ h.address }}</td>
                  <td class="num mono">{{ h.cpu_count }}</td>
                  <td>
                    <div class="memory">
                    <div
                      class="meter"
                      role="meter"
                      :aria-valuenow="Math.round((h.allocated_memory_mib / Math.max(h.memory_mib, 1)) * 100)"
                      aria-valuemin="0"
                      aria-valuemax="100"
                      :aria-label="`${h.name} memory allocated`"
                    >
                      <span
                        v-for="vm in byHost.get(h.id) ?? []"
                        :key="vm.id"
                        :class="{ off: vm.power !== 'running' }"
                        :style="{ width: share(h, vm) }"
                        :title="`${vm.name} · ${mib(vm.memory_mib)}`"
                      />
                    </div>
                    <span class="mono muted">
                      {{ h.state === 'decommissioned' ? '—' : `${mib(h.allocated_memory_mib)} / ${mib(h.memory_mib)}` }}
                    </span>
                    </div>
                  </td>
                  <td>
                    <div class="vms">
                      <RouterLink
                        v-for="vm in shownVms(h)"
                        :key="vm.id"
                        :to="{ name: 'vm', params: { id: vm.id } }"
                        :title="`${vm.name} · ${mib(vm.memory_mib)} · ${vmState(vm).text}`"
                      >
                        <StatusLed :tone="vmState(vm).tone" />{{ vm.name }}
                      </RouterLink>
                      <button
                        v-if="(byHost.get(h.id)?.length ?? 0) > VM_LIMIT"
                        type="button"
                        class="more"
                        :aria-expanded="allVms.has(h.id)"
                        @click="toggleVms(h.id)"
                      >
                        {{ allVms.has(h.id) ? 'Show fewer' : `+${(byHost.get(h.id)?.length ?? 0) - VM_LIMIT} more` }}
                      </button>
                    </div>
                  </td>
                  <td class="end">
                    <button v-if="h.state !== 'decommissioned'" type="button" class="btn small" @click="manage(h)">
                      Manage
                    </button>
                  </td>
                </tr>
              </tbody>
            </table>
          </div>
        </template>
      </section>
    </div>

    <ClusterDialog
      v-if="clusterDialog"
      :cluster="clusterDialog.cluster"
      :remove="clusterDialog.remove"
      @close="clusterDialog = null"
      @saved="clusterSaved"
    />

    <ModalDialog v-if="adding" title="Add host" @close="adding = false">
      <div class="body">
        <p v-if="tokenError" class="error-text">{{ tokenError }}</p>
        <template v-else-if="token">
          <p>
            On the new hypervisor, install the <span class="mono">metastratum-agent</span> package, put this in
            <span class="mono">/etc/metastratum/agent.toml</span>, and start it with
            <span class="mono">systemctl enable --now mst-agent</span>:
          </p>
          <pre class="cmd mono">{{ agentConfig }}</pre>
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

    <ModalDialog v-if="managing" class="drawer" :title="`Manage ${managing.name}`" @close="managing = null">
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
        <div v-if="managing.pending_facts" class="evacuate">
          <div>
            <strong>Changes waiting</strong>
            <p class="muted hint">
              The host reports these about itself. They would let it take more (VMs, or gateways for VPCs), so they
              apply once you accept them; until then the host keeps what it has.
            </p>
            <ul class="changes">
              <li v-for="c in pendingChanges(managing)" :key="c.label">
                {{ c.label }}: <span class="mono">{{ c.from }}</span> → <span class="mono">{{ c.to }}</span>
              </li>
            </ul>
          </div>
          <button type="button" class="btn" @click="acceptFacts">Accept changes</button>
        </div>
        <div class="evacuate">
          <div>
            <strong>Evacuate</strong>
            <p class="muted hint">
              Puts the host in maintenance and live-migrates its running VMs to other hosts of its cluster, one at a
              time. Stopped VMs, and VMs that are not migratable, stay. Images only this host holds are copied to
              other hosts (they are not downloaded again). Follow it in Tasks.
              <template v-if="managing.sole_images.length">
                <br />Only here: {{ managing.sole_images.join(', ') }}.
              </template>
            </p>
          </div>
          <button
            type="button"
            class="btn"
            :disabled="(!managing.cluster_id && !!managing.vm_count) || (!managing.vm_count && !managing.sole_images.length)"
            :title="
              !managing.cluster_id && managing.vm_count
                ? 'The host is in no cluster'
                : !managing.vm_count && !managing.sole_images.length
                  ? 'No VMs or images to move'
                  : undefined
            "
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
.page-head {
  margin-bottom: 8px;
}
.empty-state {
  margin-bottom: 18px;
}
.groups {
  display: grid;
  gap: 18px;
}
.group {
  min-width: 0;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
  background: var(--panel);
}
.group > header {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 10px;
  padding: 14px 16px;
  border-bottom: 1px solid var(--rule);
}
.group h2 {
  font-size: 16px;
}
.fold {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 26px;
  height: 26px;
  margin-left: -6px;
  padding: 0;
  border: 0;
  border-radius: var(--radius);
  background: none;
  color: var(--ink);
  cursor: pointer;
}
.fold:hover {
  background: var(--sunken);
}
.fold svg.shut {
  transform: rotate(-90deg);
}
.buttons {
  display: flex;
  gap: 6px;
  margin-left: auto;
}
.note {
  margin: 0;
  padding: 10px 16px;
  border-bottom: 1px solid var(--rule);
}
.note.none {
  border-bottom: 0;
}
.scroll {
  overflow-x: auto;
}
.data td {
  padding-block: 12px;
}
.data th:first-child,
.data td:first-child {
  padding-left: 16px;
}
.data th:last-child,
.data td:last-child {
  padding-right: 16px;
}
tr.retired {
  opacity: 0.6;
}
tr.down {
  opacity: 0.75;
}
.host {
  display: grid;
  gap: 4px;
}
.host strong {
  font-size: 12px;
  font-weight: 600;
}
.platform {
  font-size: 12px;
  color: var(--muted);
  white-space: nowrap;
}
.tags {
  display: flex;
  flex-wrap: wrap;
  gap: 4px;
}
/* Tags that mean something: good, worth a look, broken. */
.tag.ha {
  border-color: var(--led-on);
  color: color-mix(in srgb, var(--led-on) 70%, var(--ink));
}
.tag.warn {
  border-color: var(--led-busy);
  color: color-mix(in srgb, var(--led-busy) 60%, var(--ink));
}
.tag.fault {
  border-color: var(--led-fault);
  color: color-mix(in srgb, var(--led-fault) 75%, var(--ink));
}
.nowrap {
  white-space: nowrap;
}
.memory {
  display: grid;
  gap: 6px;
  min-width: 220px;
}
/* The host's memory: a segment per VM, in proportion to what it was given. */
.meter {
  display: flex;
  gap: 1px;
  height: 10px;
  overflow: hidden;
  border-radius: 2px;
  background: var(--sunken);
}
.meter span {
  flex: none;
  height: 100%;
  background: var(--ink);
}
.meter span.off {
  background: repeating-linear-gradient(135deg, var(--led-off) 0 2px, var(--sunken) 2px 4px);
}
.memory .mono {
  font-size: 11px;
}
.vms {
  display: flex;
  flex-wrap: wrap;
  gap: 4px 12px;
}
.vms a {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  text-decoration: none;
  white-space: nowrap;
}
.vms a:hover {
  text-decoration: underline;
}
.more {
  height: 22px;
  padding: 0 6px;
  border: 1px solid var(--rule);
  border-radius: 2px;
  background: var(--panel);
  color: var(--ink);
  font: 500 12px var(--font-body);
  white-space: nowrap;
  cursor: pointer;
}
/* Managing a host: a panel at the side, over the page. */
.modal.drawer {
  width: min(540px, 100vw);
  max-width: 100vw;
  height: 100vh;
  max-height: 100vh;
  margin: 0 0 0 auto;
  border-width: 0 0 0 1px;
  border-radius: 0;
}
.report {
  margin: 0;
  padding-left: 18px;
  display: grid;
  gap: 6px;
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
.changes {
  margin: 6px 0 0;
  padding-left: 18px;
  font-size: 13px;
}
</style>
