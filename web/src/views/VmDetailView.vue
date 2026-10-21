<script setup lang="ts">
import { computed, defineAsyncComponent, onMounted, onUnmounted, ref, watch } from 'vue'
import { useRouter } from 'vue-router'
import { api, ApiError, errorText, ok, type Disk, type Schemas, type Vm } from '../api/client'
import CloneDialog from '../components/CloneDialog.vue'
import IdChip from '../components/IdChip.vue'
import MigrateDialog from '../components/MigrateDialog.vue'
import ModalDialog from '../components/ModalDialog.vue'
import ResizeDialog from '../components/ResizeDialog.vue'
import StatusLed from '../components/StatusLed.vue'
import VmBackups from '../components/VmBackups.vue'
import VmDisks from '../components/VmDisks.vue'
import VmSnapshots from '../components/VmSnapshots.vue'
import VmActivity from '../components/vm/VmActivity.vue'
import VmInterfaces from '../components/vm/VmInterfaces.vue'
import VmMoreMenu, { type MenuItem } from '../components/vm/VmMoreMenu.vue'
import VmSettings from '../components/vm/VmSettings.vue'
import '../components/vm/cards.css'
import { gib, mib, powerNote, vmState } from '../state'
import { useLiveRefresh } from '../stores/live'
import { useScope } from '../stores/scope'
import { useSession } from '../stores/session'
import { useShell } from '../stores/shell'
import { useTasks } from '../stores/tasks'

// Console clients (noVNC, xterm.js) are large; load them when first opened.
const VncConsole = defineAsyncComponent(() => import('../components/VncConsole.vue'))
const SerialConsole = defineAsyncComponent(() => import('../components/SerialConsole.vue'))

/** A VM's pages: its overview (`section` none), consoles and settings. */
const props = defineProps<{ id: string; section?: 'console' | 'serial' | 'settings' | '' }>()
const router = useRouter()
const scope = useScope()
const session = useSession()
const shell = useShell()
const tasks = useTasks()

const vm = ref<Vm | null>(null)
const missing = ref(false)
const actionError = ref('')

async function load() {
  try {
    vm.value = await ok(api.GET('/api/v1/vms/{id}', { params: { path: { id: props.id } } }))
  } catch (e) {
    if (e instanceof ApiError && e.status === 404) missing.value = true
  }
}
onMounted(load)
// Another VM on the same page (a clone just made): start over.
watch(
  () => props.id,
  () => {
    vm.value = null
    missing.value = false
    void load()
  },
)
useLiveRefresh(['vm', 'host', 'external_ip'], load, (n) => n.kind !== 'vm' || n.id === props.id)
// The breadcrumb ends with its name.
watch(
  () => vm.value?.name,
  (name) => name && (shell.crumb = name),
  { immediate: true },
)

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

// Its cluster, which only those who see hosts may know.
const cluster = ref<string | null>(null)
watch(
  () => [vm.value?.host_id, session.me?.provider_admin] as const,
  async ([host, admin]) => {
    cluster.value = null
    if (!host || !admin) return
    const hosts = await ok(api.GET('/api/v1/hosts')).catch(() => [])
    cluster.value = hosts.find((h) => h.id === host)?.cluster_name ?? null
  },
  { immediate: true },
)

const state = computed(() => (vm.value ? vmState(vm.value) : null))
const note = computed(() => (vm.value ? powerNote(vm.value) : null))
const settled = computed(() => vm.value?.status === 'ready')
const running = computed(() => vm.value?.power === 'running' && vm.value.status === 'ready' && vm.value.host_connected)
// Consoles work while the VM migrates too (on its source, until it moves).
const live = computed(
  () =>
    vm.value?.power === 'running' &&
    vm.value.host_connected &&
    (vm.value.status === 'ready' || vm.value.status === 'migrating'),
)
const stopped = computed(
  () => vm.value?.status === 'ready' && vm.value.power === 'stopped' && vm.value.desired_power === 'stopped',
)
const tenantName = computed(() => (vm.value ? (scope.byId.get(vm.value.tenant_id)?.name ?? null) : null))

const migrating = ref(false)
const resizing = ref(false)
const cloning = ref(false)
const migrationTask = computed(() => tasks.tasks.find((t) => t.id === vm.value?.migration?.task_id))

async function cancelMigration() {
  actionError.value = ''
  try {
    await ok(api.POST('/api/v1/vms/{id}/migrate/cancel', { params: { path: { id: props.id } } }))
    await load()
  } catch (e) {
    actionError.value = errorText(e)
  }
}

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

// The buttons beside its name: what its state calls for.
type Button = { label: string; title?: string; primary?: boolean; disabled: boolean; run: () => void }
const buttons = computed<Button[]>(() => {
  const v = vm.value
  if (!v) return []
  if (v.status === 'migrating' || v.power === 'running' || v.power === 'shutting_down') {
    return [
      { label: 'Shut down', title: 'Ask the guest OS to shut down', disabled: !running.value, run: () => act('stop') },
      { label: 'Reboot', disabled: !running.value, run: () => act('reboot') },
      { label: 'Power off', title: 'Cut power immediately', disabled: !running.value, run: () => act('force') },
    ]
  }
  if (v.template) {
    return [
      {
        label: 'Clone…',
        title: stopped.value ? 'A new VM with copies of its disks' : 'Stop the VM to clone it',
        primary: true,
        disabled: !stopped.value,
        run: () => (cloning.value = true),
      },
    ]
  }
  return [
    {
      label: v.power === 'suspended' ? 'Wake' : 'Start',
      primary: true,
      disabled: !settled.value || !v.host_connected,
      run: () => act('start'),
    },
  ]
})

// The rest, in its More menu.
const menu = computed<MenuItem[]>(() => {
  const v = vm.value
  if (!v) return []
  const items: MenuItem[] = []
  if (v.template) items.push({ label: 'Start', hint: 'A template is not started: clone it', disabled: true, run: () => {} })
  if (session.me?.provider_admin) {
    items.push({
      label: 'Migrate…',
      hint: !v.migratable
        ? 'This VM is not migratable'
        : running.value
          ? 'Move the running VM to another host'
          : 'Only a running VM moves',
      disabled: !running.value || !v.migratable,
      run: () => (migrating.value = true),
    })
  }
  items.push({
    label: 'Resize…',
    hint: 'Change its CPUs and memory, or grow its disks',
    disabled: v.status !== 'ready',
    run: () => (resizing.value = true),
  })
  if (!v.template) {
    items.push({
      label: 'Clone…',
      hint: stopped.value ? 'A new VM with copies of its disks' : 'Stop the VM to clone it',
      disabled: !stopped.value,
      run: () => (cloning.value = true),
    })
  }
  items.push({
    label: 'Delete VM…',
    hint: v.status === 'migrating' ? 'Not while it migrates' : undefined,
    danger: true,
    sep: true,
    disabled: v.status === 'deleting' || v.status === 'migrating',
    run: () => (confirmingDelete.value = true),
  })
  return items
})

// Deleting it. Disks of their own attached to it (named) are kept, unless asked.
const confirmingDelete = ref(false)
const ownDisks = ref<Disk[]>([])
const deleteDisks = ref(false)
watch(confirmingDelete, async (on) => {
  deleteDisks.value = false
  if (!on) return
  const disks = await ok(api.GET('/api/v1/vms/{id}/disks', { params: { path: { id: props.id } } })).catch(() => [])
  ownDisks.value = disks.filter((d) => !d.boot && d.name)
})
async function remove() {
  try {
    await ok(
      api.DELETE('/api/v1/vms/{id}', {
        params: { path: { id: props.id }, query: { delete_disks: deleteDisks.value } },
      }),
    )
    tasks.poke()
    confirmingDelete.value = false
    await router.push({ name: 'vms' })
  } catch (e) {
    confirmingDelete.value = false
    actionError.value = e instanceof ApiError ? e.message : 'The request did not reach the controller.'
  }
}

// The overview's picture of its screen.
const preview = ref<{ ctrlAltDel: () => void } | null>(null)
const previewStatus = ref<'connecting' | 'connected' | 'closed'>('connecting')
const page = (section?: string) => ({ name: 'vm', params: { id: props.id, section: section ?? '' } })
</script>

<template>
  <section v-if="missing" class="gone">
    <h1>This VM no longer exists</h1>
    <RouterLink to="/vms" class="btn">Back to machines</RouterLink>
  </section>

  <section v-else-if="vm && state" class="vm">
    <div class="top">
      <div class="head">
        <div class="title">
          <h1>{{ vm.name }}</h1>
          <StatusLed :tone="state.tone" :label="state.text" />
          <span v-if="vm.template" class="tag">Template</span>
          <span v-if="!scope.solo && tenantName" class="tag">{{ tenantName }}</span>
        </div>
        <div class="actions">
          <button
            v-for="b in buttons"
            :key="b.label"
            type="button"
            class="btn"
            :class="{ primary: b.primary }"
            :disabled="b.disabled"
            :title="b.title"
            @click="b.run()"
          >
            {{ b.label }}
          </button>
          <VmMoreMenu :items="menu" />
        </div>
      </div>
      <p v-if="actionError" class="error-text" role="alert">{{ actionError }}</p>
      <p v-if="vm.error" class="error-text">{{ vm.error }}</p>
      <p v-if="note" class="muted">{{ note }}</p>
      <div v-if="vm.migration" class="moving" role="status">
        <span class="label">Moving</span>
        <span class="mono">{{ vm.host_name }} → <strong>{{ vm.migration.dest_host_name }}</strong></span>
        <span class="muted">{{ migrationTask?.step ?? vm.migration.state }}</span>
        <div
          class="bar"
          role="progressbar"
          aria-label="Migration"
          :aria-valuenow="Math.round((migrationTask?.progress ?? 0) * 100)"
          aria-valuemin="0"
          aria-valuemax="100"
        >
          <div :style="{ width: `${(migrationTask?.progress ?? 0) * 100}%` }" />
        </div>
        <button
          v-if="session.me?.provider_admin"
          type="button"
          class="btn small"
          :disabled="vm.migration.cancel_requested"
          @click="cancelMigration"
        >
          {{ vm.migration.cancel_requested ? 'Cancelling…' : 'Cancel' }}
        </button>
      </div>
    </div>

    <nav class="sections" aria-label="VM sections">
      <RouterLink :to="page()" :aria-current="!section ? 'page' : undefined">Overview</RouterLink>
      <template v-if="live || section === 'console' || section === 'serial'">
        <RouterLink :to="page('console')" :aria-current="section === 'console' ? 'page' : undefined">Console</RouterLink>
        <RouterLink :to="page('serial')" :aria-current="section === 'serial' ? 'page' : undefined">Serial</RouterLink>
      </template>
      <template v-else>
        <span aria-disabled="true" title="Start the VM to use its console">Console</span>
        <span aria-disabled="true" title="Start the VM to use its console">Serial</span>
      </template>
      <RouterLink :to="page('settings')" :aria-current="section === 'settings' ? 'page' : undefined">Settings</RouterLink>
    </nav>

    <template v-if="section === 'console' || section === 'serial'">
      <p v-if="!live" class="vm-none">Start the VM to use its console.</p>
      <!-- Keyed by host: after a migration the console reconnects there. -->
      <VncConsole v-else-if="section === 'console'" :key="`vnc-${vm.id}-${vm.host_id}`" :vm-id="vm.id" />
      <SerialConsole v-else :key="`serial-${vm.id}-${vm.host_id}`" :vm-id="vm.id" />
    </template>

    <VmSettings
      v-else-if="section === 'settings'"
      :vm="vm"
      :running="running"
      @update="(v) => (vm = v)"
      @stop="act('stop')"
      @delete="confirmingDelete = true"
    />

    <div v-else class="overview">
      <div class="main">
        <section class="vm-card">
          <div class="vm-card-head">
            <h2>Resources</h2>
            <button
              type="button"
              class="btn small"
              :disabled="vm.status !== 'ready'"
              title="Change its CPUs and memory, or grow its disks"
              @click="resizing = true"
            >
              Resize…
            </button>
          </div>
          <div class="resources">
            <div>
              <span class="label">vCPU</span>
              <span class="big mono">{{ vm.vcpus }}</span>
              <span v-if="vm.max_vcpus > vm.vcpus" class="sub">Can grow to {{ vm.max_vcpus }} while it runs</span>
            </div>
            <div>
              <span class="label">Memory</span>
              <span class="big mono">{{ mib(vm.memory_mib) }}</span>
              <span v-if="vm.max_memory_mib > vm.memory_mib" class="sub">
                Can grow to {{ mib(vm.max_memory_mib) }} while it runs
              </span>
              <span
                v-if="guestUsed !== null && memoryStats"
                class="used"
                :title="`The guest uses ${mib(guestUsed)} of the ${mib(memoryStats.available_mib ?? 0)} it sees`"
              >
                <span class="bar small"
                  ><span :style="{ width: `${Math.min(100, (guestUsed / (memoryStats.available_mib || 1)) * 100)}%` }"
                /></span>
                <span class="mono muted">{{ mib(guestUsed) }} used</span>
              </span>
            </div>
            <div>
              <span class="label">{{ vm.disks.length > 1 ? 'Disks' : 'Disk' }}</span>
              <span class="big mono">{{ vm.disks[0] ? gib(vm.disks[0].size_bytes) : '—' }}</span>
              <span class="sub">
                Boot disk<template v-if="vm.disks.length > 1">; {{ vm.disks.length - 1 }} more below</template>
              </span>
            </div>
          </div>
        </section>
        <VmInterfaces :vm="vm" @update="(v) => (vm = v)" />
        <VmDisks :key="`disks-${vm.id}`" :vm="vm" />
        <section class="vm-card">
          <h2>Protection</h2>
          <div class="protection">
            <VmSnapshots :key="`snapshots-${vm.id}`" :vm="vm" />
            <VmBackups :key="`backups-${vm.id}`" :vm="vm" />
          </div>
        </section>
      </div>

      <aside class="side">
        <section class="vm-card">
          <div class="vm-card-head">
            <h2>Console</h2>
            <StatusLed
              v-if="live"
              class="label"
              :tone="previewStatus === 'connected' ? 'on' : previewStatus === 'connecting' ? 'busy' : 'fault'"
              :label="previewStatus === 'connected' ? 'Connected' : previewStatus === 'connecting' ? 'Connecting…' : 'Disconnected'"
            />
          </div>
          <template v-if="live">
            <div class="preview">
              <VncConsole
                ref="preview"
                :key="`preview-${vm.id}-${vm.host_id}`"
                :vm-id="vm.id"
                preview
                @status="(s) => (previewStatus = s)"
              />
              <RouterLink :to="page('console')" class="cover" aria-label="Open console" />
            </div>
            <div class="buttons">
              <RouterLink :to="page('console')" class="btn small">Open console</RouterLink>
              <RouterLink :to="page('serial')" class="btn small">Serial</RouterLink>
              <button
                type="button"
                class="btn small"
                :disabled="previewStatus !== 'connected'"
                @click="preview?.ctrlAltDel()"
              >
                Send Ctrl+Alt+Del
              </button>
            </div>
          </template>
          <p v-else class="vm-none">Start the VM to use its console.</p>
        </section>

        <section class="vm-card">
          <h2>Identity</h2>
          <dl class="kv">
            <dt>VM ID</dt>
            <dd><IdChip :value="vm.id" short /></dd>
            <template v-if="!scope.solo">
              <dt>Tenant</dt>
              <dd class="stack">
                <span v-if="tenantName">{{ tenantName }}</span>
                <IdChip :value="vm.tenant_id" short />
              </dd>
            </template>
            <dt>Firmware</dt>
            <dd>{{ vm.firmware === 'uefi' ? 'UEFI' : 'BIOS' }}</dd>
            <dt>CPU model</dt>
            <dd>{{ vm.cpu_model ?? 'Its host’s, as is' }}</dd>
            <template v-if="vm.guest_os === 'windows'">
              <dt>System</dt>
              <dd>
                Windows<template v-if="vm.account">, account {{ vm.account }}</template>
                <span v-if="vm.first_boot_pending" class="muted"> · first start pending</span>
              </dd>
            </template>
            <dt>Created</dt>
            <dd>{{ new Date(vm.created_at).toLocaleString() }}</dd>
          </dl>
          <RouterLink :to="page('settings')" class="more">All settings →</RouterLink>
        </section>

        <section class="vm-card">
          <div class="vm-card-head">
            <h2>Placement</h2>
            <button
              v-if="session.me?.provider_admin"
              type="button"
              class="btn small"
              :disabled="!running || !vm.migratable"
              :title="!vm.migratable ? 'This VM is not migratable' : running ? 'Move the running VM to another host' : 'Only a running VM moves'"
              @click="migrating = true"
            >
              Migrate…
            </button>
          </div>
          <dl class="kv">
            <dt>Host</dt>
            <dd class="mono">
              <RouterLink v-if="vm.host_name && session.me?.provider_admin" to="/hosts">{{ vm.host_name }}</RouterLink>
              <template v-else>{{ vm.host_name ?? '—' }}</template>
            </dd>
            <template v-if="cluster">
              <dt>Cluster</dt>
              <dd class="mono">{{ cluster }}</dd>
            </template>
            <dt>Live migration</dt>
            <dd>{{ vm.migratable ? 'Allowed' : 'Off: host CPU and disk caching' }}</dd>
          </dl>
        </section>

        <VmActivity :vm-id="vm.id" :tenant-id="vm.tenant_id" />
      </aside>
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
    <ModalDialog v-if="confirmingDelete" :title="`Delete ${vm.name}?`" @close="confirmingDelete = false">
      <form class="form" @submit.prevent="remove">
        <p class="muted">Its backups are kept and can be restored as a new VM.</p>
        <label v-if="ownDisks.length" class="check">
          <input id="delete-disks" v-model="deleteDisks" type="checkbox" />
          Its disks too: {{ ownDisks.map((d) => d.name).join(', ') }}
        </label>
        <footer>
          <button type="button" class="btn" @click="confirmingDelete = false">Keep it</button>
          <button class="btn danger">Delete {{ vm.name }} and its {{ deleteDisks ? 'disks' : 'disk' }}</button>
        </footer>
      </form>
    </ModalDialog>
  </section>
</template>

<style scoped>
.vm {
  display: flex;
  flex-direction: column;
  gap: 20px;
}
.top {
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.top p {
  margin: 0;
}
.head {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 16px;
}
.title {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 14px;
  min-width: 0;
}
h1 {
  font-size: 26px;
  overflow-wrap: anywhere;
}
.actions {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 6px;
  margin-left: auto;
}
.moving {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px 14px;
  padding: 10px 14px;
  border-radius: var(--radius);
  background: var(--sunken);
}
.bar {
  flex: 1;
  min-width: 140px;
  height: 6px;
  overflow: hidden;
  border-radius: 3px;
  background: var(--rule);
}
.bar > div {
  height: 100%;
  background: var(--led-busy);
  transition: width 300ms;
}
.sections {
  display: flex;
  gap: 2px;
  overflow-x: auto;
  border-bottom: 1px solid var(--rule);
}
.sections a,
.sections span {
  padding: 8px 14px;
  margin-bottom: -1px;
  border-bottom: 2px solid transparent;
  color: var(--muted);
  font: 500 12px var(--font-label);
  letter-spacing: 0.12em;
  text-transform: uppercase;
  text-decoration: none;
  white-space: nowrap;
}
.sections a:hover {
  color: var(--ink);
}
.sections a[aria-current='page'] {
  border-bottom-color: var(--tape);
  color: var(--ink);
}
.sections span[aria-disabled='true'] {
  opacity: 0.5;
  cursor: default;
}
.overview {
  display: flex;
  flex-wrap: wrap;
  align-items: flex-start;
  gap: 20px;
}
.main {
  display: flex;
  flex: 999 1 560px;
  flex-direction: column;
  gap: 16px;
  min-width: 0;
}
.side {
  display: flex;
  flex: 1 1 300px;
  flex-direction: column;
  gap: 16px;
  min-width: 0;
}
.resources {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(min(160px, 100%), 1fr));
  gap: 16px;
}
.resources > div {
  display: grid;
  align-content: start;
  gap: 4px;
}
.big {
  font-size: 18px;
}
.sub {
  font-size: 12px;
  color: var(--muted);
}
.used {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 11px;
}
.bar.small {
  flex: 1;
  max-width: 120px;
  min-width: 0;
  background: var(--sunken);
}
.bar.small > span {
  display: block;
  height: 100%;
  background: var(--ink);
}
.protection {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(min(300px, 100%), 1fr));
  gap: 24px;
}
.preview {
  position: relative;
  overflow: hidden;
}
.cover {
  position: absolute;
  inset: 0;
}
.buttons {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
}
a.btn {
  text-decoration: none;
}
.kv {
  display: grid;
  grid-template-columns: 104px 1fr;
  gap: 8px 12px;
  margin: 0;
}
.kv dt {
  color: var(--muted);
}
.kv dd {
  margin: 0;
  min-width: 0;
  overflow-wrap: anywhere;
}
.kv .stack {
  display: grid;
  justify-items: start;
  gap: 4px;
}
.more {
  font-size: 13px;
}
.gone {
  display: grid;
  gap: 12px;
  justify-items: start;
  padding-top: 48px;
}
</style>
