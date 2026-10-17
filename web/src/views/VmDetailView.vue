<script setup lang="ts">
import { computed, defineAsyncComponent, onMounted, onUnmounted, ref, watch } from 'vue'
import { useRouter } from 'vue-router'
import { api, ApiError, errorText, ok, type Schemas, type SecurityGroup, type Task, type Vm } from '../api/client'
import IdChip from '../components/IdChip.vue'
import MigrateDialog from '../components/MigrateDialog.vue'
import ResizeDialog from '../components/ResizeDialog.vue'
import CloneDialog from '../components/CloneDialog.vue'
import StatusLed from '../components/StatusLed.vue'
import VmBackups from '../components/VmBackups.vue'
import VmSnapshots from '../components/VmSnapshots.vue'
import { ago, gib, mib, powerNote, shortId, vmState } from '../state'
import { useLiveRefresh } from '../stores/live'
import { useScope } from '../stores/scope'
import { useSession } from '../stores/session'
import { taskLabel, useTasks } from '../stores/tasks'

// Console clients (noVNC, xterm.js) are large; load them when first opened.
const VncConsole = defineAsyncComponent(() => import('../components/VncConsole.vue'))
const SerialConsole = defineAsyncComponent(() => import('../components/SerialConsole.vue'))

const props = defineProps<{ id: string }>()
const router = useRouter()
const scope = useScope()
const tasks = useTasks()

const vm = ref<Vm | null>(null)

// What the guest uses of its memory, from its balloon, while it runs.
const memoryStats = ref<Schemas['MemoryView'] | null>(null)
async function loadMemory() {
  const v = vm.value
  if (!v || v.power !== 'running' || document.hidden) {
    if (v?.power !== 'running') memoryStats.value = null
    return
  }
  const { data } = await api.GET('/api/v1/vms/{id}/memory', { params: { path: { id: v.id } } })
  if (vm.value?.id === v.id) memoryStats.value = data ?? null
}
const guestUsed = computed(() => {
  const m = memoryStats.value
  return m?.driver && m.available_mib != null && m.usable_mib != null ? m.available_mib - m.usable_mib : null
})
let memoryTimer = 0
onMounted(() => (memoryTimer = window.setInterval(loadMemory, 10_000)))
onUnmounted(() => clearInterval(memoryTimer))
watch(() => [vm.value?.id, vm.value?.power, vm.value?.memory_mib], loadMemory, { immediate: true })
const history = ref<Task[]>([])
const missing = ref(false)
const tab = ref<'console' | 'serial' | 'details'>('details')
const actionError = ref('')
const confirmingDelete = ref(false)

const state = computed(() => (vm.value ? vmState(vm.value) : null))
const running = computed(() => vm.value?.power === 'running' && vm.value.status === 'ready' && vm.value.host_connected)
// Consoles work while the VM migrates too (on its source, until it moves).
const live = computed(
  () =>
    vm.value?.power === 'running' &&
    vm.value.host_connected &&
    (vm.value.status === 'ready' || vm.value.status === 'migrating'),
)
const session = useSession()
const migrating = ref(false)
const resizing = ref(false)
const cloning = ref(false)
const migrationTask = computed(() => tasks.tasks.find((t) => t.id === vm.value?.migration?.task_id))
// Live migration can change only while the VM is stopped.
const stopped = computed(
  () => vm.value?.status === 'ready' && vm.value.power === 'stopped' && vm.value.desired_power === 'stopped',
)
const switching = ref(false)
async function setTemplate(on: boolean) {
  actionError.value = ''
  switching.value = true
  try {
    vm.value = await ok(api.PATCH('/api/v1/vms/{id}', { params: { path: { id: props.id } }, body: { template: on } }))
  } catch (e) {
    actionError.value = errorText(e)
  } finally {
    switching.value = false
  }
}
async function setFreePageReporting(on: boolean) {
  actionError.value = ''
  switching.value = true
  try {
    vm.value = await ok(
      api.PATCH('/api/v1/vms/{id}', { params: { path: { id: props.id } }, body: { free_page_reporting: on } }),
    )
  } catch (e) {
    actionError.value = errorText(e)
  } finally {
    switching.value = false
  }
}
async function setMigratable(on: boolean) {
  actionError.value = ''
  switching.value = true
  try {
    vm.value = await ok(api.PATCH('/api/v1/vms/{id}', { params: { path: { id: props.id } }, body: { migratable: on } }))
  } catch (e) {
    actionError.value = errorText(e)
  } finally {
    switching.value = false
  }
}

async function cancelMigration() {
  actionError.value = ''
  try {
    await ok(api.POST('/api/v1/vms/{id}/migrate/cancel', { params: { path: { id: props.id } } }))
    await load()
  } catch (e) {
    actionError.value = errorText(e)
  }
}
const note = computed(() => (vm.value ? powerNote(vm.value) : null))
const settled = computed(() => vm.value?.status === 'ready')

async function load() {
  try {
    vm.value = await ok(api.GET('/api/v1/vms/{id}', { params: { path: { id: props.id } } }))
    history.value = await ok(api.GET('/api/v1/tasks', { params: { query: { target_id: props.id, limit: 10 } } }))
  } catch (e) {
    if (e instanceof ApiError && e.status === 404) missing.value = true
  }
}

// Security groups of the VM's tenant, to name and change a NIC's groups.
const groups = ref<SecurityGroup[]>([])
const editingNic = ref<string | null>(null)
const nicGroups = ref<string[]>([])
const nicError = ref('')
watch(
  () => vm.value?.tenant_id,
  async (tenant) => {
    if (!tenant) return
    groups.value = await ok(api.GET('/api/v1/security-groups', { params: { query: { tenant_id: tenant } } })).catch(
      () => [],
    )
  },
)
const groupName = (id: string) => groups.value.find((g) => g.id === id)?.name ?? shortId(id)
function editNic(n: Vm['nics'][number]) {
  nicError.value = ''
  nicGroups.value = [...n.security_group_ids]
  editingNic.value = n.id
}
async function saveNic(nic: string) {
  nicError.value = ''
  try {
    vm.value = await ok(
      api.PUT('/api/v1/vms/{id}/nics/{nic}/security-groups', {
        params: { path: { id: props.id, nic } },
        body: { security_group_ids: nicGroups.value },
      }),
    )
    editingNic.value = null
  } catch (e) {
    nicError.value = errorText(e)
  }
}

// Open on the console when the VM is running; otherwise show details.
watch(
  () => vm.value?.id,
  () => {
    if (vm.value && tab.value === 'details' && running.value) tab.value = 'console'
  },
)

onMounted(load)
// Another VM on the same page (a clone just made): start over.
watch(
  () => props.id,
  () => {
    vm.value = null
    missing.value = false
    tab.value = 'details'
    void load()
  },
)
// Task notices don't say which VM they are about; their history is cheap.
useLiveRefresh(['vm', 'task', 'host', 'external_ip'], load, (n) => n.kind !== 'vm' || n.id === props.id)

type Action = 'start' | 'stop' | 'force' | 'reboot'
async function act(action: Action) {
  actionError.value = ''
  const path = { params: { path: { id: props.id } } }
  try {
    if (action === 'start') await ok(api.POST('/api/v1/vms/{id}/start', path))
    if (action === 'reboot') await ok(api.POST('/api/v1/vms/{id}/reboot', path))
    if (action === 'stop' || action === 'force') {
      await ok(api.POST('/api/v1/vms/{id}/stop', { params: { path: { id: props.id }, query: { force: action === 'force' } } }))
    }
    tasks.poke()
    await load()
  } catch (e) {
    actionError.value = e instanceof ApiError ? e.message : 'The request did not reach the controller.'
  }
}

async function remove() {
  try {
    await ok(api.DELETE('/api/v1/vms/{id}', { params: { path: { id: props.id } } }))
    tasks.poke()
    await router.push({ name: 'vms' })
  } catch (e) {
    actionError.value = e instanceof ApiError ? e.message : 'The request did not reach the controller.'
  }
}
</script>

<template>
  <section v-if="missing" class="gone">
    <h1>This VM no longer exists</h1>
    <RouterLink to="/vms" class="btn">Back to machines</RouterLink>
  </section>

  <section v-else-if="vm && state">
    <RouterLink to="/vms" class="back label">← Machines</RouterLink>
    <header class="head">
      <div class="title">
        <h1>{{ vm.name }}</h1>
        <StatusLed :tone="state.tone" :label="state.text" />
        <span v-if="vm.template" class="tag">Template</span>
      </div>
      <div class="actions">
        <button
          class="btn"
          :disabled="!settled || !vm.host_connected || vm.power === 'running' || vm.template"
          :title="vm.template ? 'A template is not started: clone it' : undefined"
          @click="act('start')"
        >
          Start
        </button>
        <button class="btn" :disabled="!running" title="Ask the guest OS to shut down" @click="act('stop')">Shut down</button>
        <button class="btn" :disabled="!running" title="Cut power immediately" @click="act('force')">Power off</button>
        <button class="btn" :disabled="!running" @click="act('reboot')">Reboot</button>
        <button
          v-if="session.me?.provider_admin"
          class="btn"
          :disabled="!running || !vm.migratable"
          :title="vm.migratable ? 'Move the running VM to another host' : 'This VM is not migratable'"
          @click="migrating = true"
        >
          Migrate
        </button>
        <button
          class="btn"
          :disabled="vm.status !== 'ready'"
          title="Change its CPUs and memory, or grow its disks"
          @click="resizing = true"
        >
          Resize
        </button>
        <button
          class="btn"
          :disabled="!stopped"
          :title="stopped ? 'A new VM with copies of its disks' : 'Stop the VM to clone it'"
          @click="cloning = true"
        >
          Clone
        </button>
        <template v-if="!confirmingDelete">
          <button
            class="btn danger"
            :disabled="vm.status === 'deleting' || vm.status === 'migrating'"
            @click="confirmingDelete = true"
          >
            Delete VM
          </button>
        </template>
        <template v-else>
          <button class="btn danger" @click="remove">Delete {{ vm.name }} and its disk</button>
          <button class="btn" @click="confirmingDelete = false">Keep it</button>
        </template>
      </div>
    </header>
    <p v-if="actionError" class="error-text" role="alert">{{ actionError }}</p>
    <p v-if="vm.error" class="error-text">{{ vm.error }}</p>
    <p v-if="note" class="note">{{ note }}</p>
    <div v-if="vm.migration" class="moving" role="status">
      <span class="label">Moving</span>
      <span>
        {{ vm.host_name }} → <strong>{{ vm.migration.dest_host_name }}</strong>
        <span class="muted step">{{ migrationTask?.step ?? vm.migration.state }}</span>
      </span>
      <progress :value="migrationTask?.progress ?? 0" max="1" />
      <button v-if="session.me?.provider_admin" class="btn small" :disabled="vm.migration.cancel_requested" @click="cancelMigration">
        {{ vm.migration.cancel_requested ? 'Cancelling…' : 'Cancel' }}
      </button>
    </div>

    <dl class="facts">
      <div><dt class="label">Address</dt><dd class="mono">{{ vm.nics[0]?.ip ?? '—' }}</dd></div>
      <div v-if="vm.nics[0]?.floating_ip">
        <dt class="label">Floating IP</dt><dd class="mono">{{ vm.nics[0].floating_ip }}</dd>
      </div>
      <div><dt class="label">Host</dt><dd class="mono">{{ vm.host_name ?? '—' }}</dd></div>
      <div>
        <dt class="label">vCPU</dt>
        <dd class="mono">
          {{ vm.vcpus }}<span v-if="vm.max_vcpus > vm.vcpus" class="muted" title="What it can grow to while it runs">
            / {{ vm.max_vcpus }}</span>
        </dd>
      </div>
      <div>
        <dt class="label">Memory</dt>
        <dd class="mono">
          {{ mib(vm.memory_mib) }}<span v-if="vm.max_memory_mib > vm.memory_mib" class="muted" title="What it can grow to while it runs">
            / {{ mib(vm.max_memory_mib) }}</span>
          <span v-if="guestUsed !== null && memoryStats" class="guest" :title="`The guest uses ${mib(guestUsed)} of the ${mib(memoryStats.available_mib ?? 0)} it sees`">
            <meter :value="guestUsed" min="0" :max="memoryStats.available_mib ?? 1" />
            <span class="muted">{{ mib(guestUsed) }} used</span>
          </span>
        </dd>
      </div>
      <div><dt class="label">Disk</dt><dd class="mono">{{ vm.disks[0] ? gib(vm.disks[0].size_bytes) : '—' }}</dd></div>
    </dl>

    <div class="tabs" role="tablist">
      <button role="tab" :aria-selected="tab === 'console'" :disabled="!live" @click="tab = 'console'">Console</button>
      <button role="tab" :aria-selected="tab === 'serial'" :disabled="!live" @click="tab = 'serial'">Serial</button>
      <button role="tab" :aria-selected="tab === 'details'" @click="tab = 'details'">Details</button>
    </div>

    <div class="panel">
      <template v-if="tab !== 'details' && !live">
        <p class="muted">Start the VM to use its console.</p>
      </template>
      <!-- Keyed by host: after a migration the console reconnects there. -->
      <VncConsole v-else-if="tab === 'console'" :key="`vnc-${vm.id}-${vm.host_id}`" :vm-id="vm.id" />
      <SerialConsole v-else-if="tab === 'serial'" :key="`serial-${vm.id}-${vm.host_id}`" :vm-id="vm.id" />
      <div v-else class="details">
        <section>
          <h2 class="label">Identity</h2>
          <dl class="kv">
            <dt>VM ID</dt><dd><IdChip :value="vm.id" /></dd>
            <template v-if="!scope.solo"><dt>Tenant</dt><dd><IdChip :value="vm.tenant_id" /></dd></template>
            <dt>Firmware</dt><dd>{{ vm.firmware === 'uefi' ? 'UEFI' : 'BIOS' }}</dd>
            <dt>Live migration</dt>
            <dd class="migratable">
              {{ vm.migratable ? 'Allowed' : 'Off: host CPU and disk caching' }}
              <button
                class="btn small"
                :disabled="!stopped || switching"
                :title="stopped ? undefined : 'Stop the VM to change this'"
                @click="setMigratable(!vm.migratable)"
              >
                {{ switching ? 'Changing…' : vm.migratable ? 'Turn off' : 'Allow' }}
              </button>
            </dd>
            <dt>Free memory</dt>
            <dd class="migratable">
              {{ vm.free_page_reporting ? 'Handed back to the host' : 'Kept by the guest' }}
              <button
                class="btn small"
                :disabled="!stopped || switching"
                :title="
                  stopped
                    ? vm.free_page_reporting
                      ? 'The guest keeps memory it frees: no faults to get it back, but the host cannot use it'
                      : 'The guest hands memory it frees back to the host, which can use it elsewhere'
                    : 'Stop the VM to change this'
                "
                @click="setFreePageReporting(!vm.free_page_reporting)"
              >
                {{ vm.free_page_reporting ? 'Keep it' : 'Hand it back' }}
              </button>
            </dd>
            <dt>Template</dt>
            <dd class="migratable">
              {{ vm.template ? 'Yes: cloned, never started' : 'No' }}
              <button
                class="btn small"
                :disabled="!stopped || switching"
                :title="stopped ? undefined : 'Stop the VM to change this'"
                @click="setTemplate(!vm.template)"
              >
                {{ vm.template ? 'Make it a VM' : 'Make template' }}
              </button>
            </dd>
            <dt>Created</dt><dd>{{ new Date(vm.created_at).toLocaleString() }}</dd>
          </dl>
        </section>
        <section>
          <h2 class="label">Network interfaces</h2>
          <dl v-for="n in vm.nics" :key="n.id" class="kv">
            <dt>Address</dt><dd class="mono">{{ n.ip }}</dd>
            <dt>Network</dt>
            <dd><RouterLink to="/networks">{{ n.network_name ?? shortId(n.network_id) }}</RouterLink></dd>
            <dt>Floating IP</dt>
            <dd class="mono">
              <RouterLink v-if="n.floating_ip" to="/floating-ips">{{ n.floating_ip }}</RouterLink>
              <span v-else class="muted">None</span>
            </dd>
            <dt>Security groups</dt>
            <dd>
              <template v-if="editingNic !== n.id">
                <span v-for="g in n.security_group_ids" :key="g" class="tag group">{{ groupName(g) }}</span>
                <span v-if="!n.security_group_ids.length" class="error-text">None: no traffic passes</span>
                <button v-if="groups.length" class="btn small" @click="editNic(n)">Change</button>
              </template>
              <form v-else class="nic-groups" @submit.prevent="saveNic(n.id)">
                <label v-for="g in groups" :key="g.id" class="check">
                  <input v-model="nicGroups" type="checkbox" :value="g.id" />
                  {{ g.name }}
                </label>
                <span class="buttons">
                  <button type="button" class="btn small" @click="editingNic = null">Cancel</button>
                  <button class="btn small primary">Save</button>
                </span>
                <span v-if="nicError" class="error-text">{{ nicError }}</span>
              </form>
            </dd>
            <dt>MAC</dt><dd class="mono">{{ n.mac }}</dd>
            <dt>Interface</dt><dd><IdChip :value="n.id" short /></dd>
          </dl>
        </section>
        <section>
          <h2 class="label">Disks</h2>
          <dl v-for="(d, i) in vm.disks" :key="d.volume_id" class="kv">
            <dt>{{ i === 0 ? 'Boot disk' : `Disk ${i + 1}` }}</dt><dd class="mono">{{ gib(d.size_bytes) }}</dd>
            <dt>Volume</dt><dd><IdChip :value="d.volume_id" short /></dd>
          </dl>
        </section>
        <VmSnapshots :key="`snapshots-${vm.id}`" :vm="vm" />
        <VmBackups :key="`backups-${vm.id}`" :vm="vm" />
        <section>
          <h2 class="label">Recent activity</h2>
          <ol class="history">
            <li v-for="t in history" :key="t.id">
              <StatusLed :tone="t.state === 'failed' ? 'fault' : t.state === 'succeeded' ? 'on' : 'busy'" />
              <span>{{ taskLabel(t) }}</span>
              <span class="muted">{{ ago(t.created_at) }}</span>
              <span v-if="t.error" class="error-text">{{ t.error }}</span>
            </li>
          </ol>
        </section>
      </div>
    </div>
    <CloneDialog
      v-if="cloning"
      :vm="vm"
      @close="cloning = false"
      @created="(v) => ((cloning = false), tasks.poke(), router.push({ name: 'vm', params: { id: v.id } }))"
    />
    <ResizeDialog v-if="resizing" :vm="vm" @close="resizing = false" @done="(v) => ((resizing = false), (vm = v))" />
    <MigrateDialog
      v-if="migrating"
      :vm="vm"
      @close="migrating = false"
      @started="((migrating = false), tasks.poke(), load())"
    />
  </section>
</template>

<style scoped>
.back {
  display: inline-block;
  margin-bottom: 10px;
  text-decoration: none;
}
.head {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  justify-content: space-between;
  gap: 16px;
}
.title {
  display: flex;
  align-items: center;
  gap: 16px;
}
h1 {
  font-size: 26px;
}
.actions {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
}
.note {
  color: var(--muted);
}
.guest {
  display: flex;
  align-items: center;
  gap: 6px;
  margin-top: 4px;
  font-size: 12px;
}
.guest meter {
  width: 72px;
  height: 6px;
}
.facts {
  display: flex;
  flex-wrap: wrap;
  gap: 8px 36px;
  margin: 22px 0;
}
.facts div {
  display: grid;
  gap: 2px;
}
.facts dd {
  margin: 0;
  font-size: 13px;
}
.tabs {
  display: flex;
  gap: 2px;
  border-bottom: 1px solid var(--rule);
}
.tabs button {
  padding: 8px 14px;
  border: 0;
  border-bottom: 2px solid transparent;
  background: none;
  color: var(--muted);
  font: 500 12px var(--font-label);
  letter-spacing: 0.12em;
  text-transform: uppercase;
  cursor: pointer;
}
.tabs button[aria-selected='true'] {
  color: var(--ink);
  border-bottom-color: var(--tape);
}
.tabs button:disabled {
  opacity: 0.4;
  cursor: default;
}
.panel {
  padding: 18px 0;
}
.details {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(260px, 1fr));
  gap: 24px;
}
.details h2 {
  margin-bottom: 8px;
}
.kv {
  display: grid;
  grid-template-columns: 96px 1fr;
  gap: 6px 12px;
  margin: 0 0 12px;
}
.kv dt {
  color: var(--muted);
}
.kv dd {
  margin: 0;
}
.history {
  display: grid;
  gap: 8px;
  margin: 0;
  padding: 0;
  list-style: none;
}
.history li {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 10px;
}
.muted {
  color: var(--muted);
}
.gone {
  display: grid;
  gap: 12px;
  justify-items: start;
  padding-top: 48px;
}
.group {
  margin-right: 6px;
}
.nic-groups {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 6px 14px;
}
.nic-groups .buttons {
  display: flex;
  gap: 6px;
}
.moving {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px 14px;
  margin-top: 14px;
  padding: 10px 14px;
  border-left: 3px solid var(--led-busy);
  background: var(--sunken);
}
.migratable .btn {
  margin-left: 8px;
}
.moving .step {
  margin-left: 10px;
}
.moving progress {
  flex: 1;
  min-width: 140px;
  accent-color: var(--led-busy);
}
</style>
