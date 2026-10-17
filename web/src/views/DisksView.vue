<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue'
import { api, errorText, ok, type Disk } from '../api/client'
import AttachDiskDialog from '../components/AttachDiskDialog.vue'
import EditDiskDialog from '../components/EditDiskDialog.vue'
import MachineTabs from '../components/MachineTabs.vue'
import ModalDialog from '../components/ModalDialog.vue'
import MoveDiskDialog from '../components/MoveDiskDialog.vue'
import NewDiskDialog from '../components/NewDiskDialog.vue'
import StatusLed from '../components/StatusLed.vue'
import { DISK_STATES, diskLabel, diskWhere } from '../disks'
import { useLiveRefresh } from '../stores/live'
import { useScope } from '../stores/scope'
import { useTasks } from '../stores/tasks'

const scope = useScope()
const tasks = useTasks()
const disks = ref<Disk[] | null>(null)
const error = ref('')
const busy = ref(false)

async function load() {
  const tenant = scope.current
  if (!tenant) return
  const d = await ok(api.GET('/api/v1/disks', { params: { query: { tenant_id: tenant } } }))
  if (tenant === scope.current) disks.value = d
}
onMounted(load)
watch(() => scope.current, load)
useLiveRefresh(['disk', 'vm'], load)

const mixed = computed(() => disks.value?.some((d) => d.tenant_id !== scope.current) ?? false)
const canWrite = (d: Disk) => scope.can('vm.write', d.tenant_id)
const canCreate = computed(() => !!scope.current && scope.can('vm.write', scope.current))
const tone = (d: Disk) => (d.state === 'failed' ? 'fault' : d.state === 'ready' ? (d.vm_id ? 'on' : 'off') : 'busy')

const creating = ref(false)
const attaching = ref<Disk | null>(null)
const moving = ref<Disk | null>(null)
const editing = ref<Disk | null>(null)
const detaching = ref<Disk | null>(null)
const deleting = ref<Disk | null>(null)

async function act(fn: () => Promise<unknown>) {
  error.value = ''
  busy.value = true
  try {
    await fn()
    tasks.poke()
    detaching.value = null
    deleting.value = null
    await load()
  } catch (e) {
    error.value = errorText(e)
  } finally {
    busy.value = false
  }
}
const detach = (d: Disk) =>
  act(() =>
    ok(api.DELETE('/api/v1/vms/{id}/disks/{disk_id}', { params: { path: { id: d.vm_id!, disk_id: d.id } } })),
  )
const remove = (d: Disk) => act(() => ok(api.DELETE('/api/v1/disks/{id}', { params: { path: { id: d.id } } })))
const done = () => {
  attaching.value = moving.value = editing.value = null
  creating.value = false
  void load()
}
</script>

<template>
  <section>
    <MachineTabs />
    <header class="page-head">
      <h1>Disks</h1>
      <button v-if="canCreate" class="btn primary" @click="creating = true">New disk</button>
    </header>
    <p class="intro">
      Every VM’s boot disk, and disks of their own: attach them to a VM (plugged in while it runs), detach them, and
      attach them elsewhere. A disk on a host’s own storage attaches to VMs on that host, and moves to another host when
      attached to a VM there; a VM that migrates takes its disks along. Deleting a VM keeps the disks attached to it,
      unless asked otherwise.
    </p>

    <div v-if="disks && !disks.length" class="empty-state">
      <p>No disks{{ scope.solo ? '' : ` in ${scope.tenant?.name ?? 'this tenant'}` }} yet.</p>
    </div>
    <div v-else-if="disks" class="table-wrap">
      <table class="data">
        <thead>
          <tr>
            <th>Disk</th>
            <th v-if="mixed">Tenant</th>
            <th>VM</th>
            <th class="num">Size</th>
            <th>Storage</th>
            <th>State</th>
            <th />
          </tr>
        </thead>
        <tbody>
          <tr v-for="d in disks" :key="d.id">
            <td>
              {{ diskLabel(d) }}
              <span v-if="d.boot" class="tag">Boot</span>
            </td>
            <td v-if="mixed" class="muted">{{ scope.byId.get(d.tenant_id)?.name ?? '—' }}</td>
            <td>
              <RouterLink v-if="d.vm_id" :to="{ name: 'vm', params: { id: d.vm_id } }">{{ d.vm_name }}</RouterLink>
              <span v-else class="muted">None</span>
            </td>
            <td class="num mono">{{ d.size_gib }} GiB</td>
            <td>{{ diskWhere(d) }}</td>
            <td>
              <StatusLed :tone="tone(d)" />
              {{ DISK_STATES[d.state] ?? (d.vm_id ? 'Attached' : 'Free') }}
              <div v-if="d.error" class="error-text">{{ d.error }}</div>
            </td>
            <td class="end">
              <template v-if="canWrite(d) && (d.state === 'ready' || d.state === 'failed')">
                <!-- Busy disks (attaching, moving...) wait. -->
                <button v-if="!d.vm_id && d.state === 'ready'" class="btn small" @click="attaching = d">Attach…</button>
                <button v-if="d.vm_id && !d.boot" class="btn small" @click="detaching = d">Detach…</button>
                <button v-if="!d.vm_id && d.state === 'ready'" class="btn small" @click="moving = d">Move…</button>
                <button v-if="d.state === 'ready'" class="btn small" @click="editing = d">Change…</button>
                <button v-if="!d.vm_id" class="btn small danger" @click="deleting = d">Delete…</button>
              </template>
            </td>
          </tr>
        </tbody>
      </table>
    </div>
    <p v-if="error" class="error-text" role="alert">{{ error }}</p>

    <NewDiskDialog v-if="creating && scope.current" :tenant-id="scope.current" @close="creating = false" @done="done" />
    <AttachDiskDialog v-if="attaching" :disk="attaching" @close="attaching = null" @done="done" />
    <MoveDiskDialog v-if="moving" :disk="moving" @close="moving = null" @done="done" />
    <EditDiskDialog v-if="editing" :disk="editing" @close="editing = null" @done="done" />
    <ModalDialog v-if="detaching" title="Detach disk" @close="detaching = null">
      <form class="form" @submit.prevent="detach(detaching)">
        <p>
          Detach <strong>{{ diskLabel(detaching) }}</strong> from {{ detaching.vm_name }}? A running VM has it unplugged:
          unmount it in the guest first. The disk and what it holds are kept.
        </p>
        <footer>
          <button type="button" class="btn" @click="detaching = null">Cancel</button>
          <button class="btn danger" :disabled="busy">Detach disk</button>
        </footer>
      </form>
    </ModalDialog>
    <ModalDialog v-if="deleting" title="Delete disk" @close="deleting = null">
      <form class="form" @submit.prevent="remove(deleting)">
        <p>
          Delete <strong>{{ diskLabel(deleting) }}</strong> ({{ deleting.size_gib }} GiB) and everything on it? This can’t
          be undone.
        </p>
        <footer>
          <button type="button" class="btn" @click="deleting = null">Cancel</button>
          <button class="btn danger" :disabled="busy">Delete {{ diskLabel(deleting) }}</button>
        </footer>
      </form>
    </ModalDialog>
  </section>
</template>

<style scoped>
.end {
  white-space: nowrap;
}
.end .btn + .btn {
  margin-left: 6px;
}
</style>
