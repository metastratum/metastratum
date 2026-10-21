<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue'
import { api, errorText, ok, type Disk } from '../api/client'
import AttachDiskDialog from '../components/AttachDiskDialog.vue'
import EditDiskDialog from '../components/EditDiskDialog.vue'
import ListPager from '../components/ListPager.vue'
import ModalDialog from '../components/ModalDialog.vue'
import MoveDiskDialog from '../components/MoveDiskDialog.vue'
import NewDiskDialog from '../components/NewDiskDialog.vue'
import SegmentedFilter from '../components/SegmentedFilter.vue'
import StatusLed from '../components/StatusLed.vue'
import { DISK_STATES, diskLabel, diskWhere } from '../disks'
import { usePaging } from '../paging'
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

// Attached to a VM, or free; and by the disk's or its VM's name.
const filter = ref<'all' | 'attached' | 'free'>('all')
const name = ref('')
const filters = computed(() => {
  const all = disks.value ?? []
  return [
    { id: 'all', label: 'All', count: all.length },
    { id: 'attached', label: 'Attached', count: all.filter((d) => d.vm_id).length },
    { id: 'free', label: 'Free', count: all.filter((d) => !d.vm_id).length },
  ]
})
const matching = computed(() => {
  const q = name.value.trim().toLowerCase()
  return (disks.value ?? []).filter(
    (d) =>
      (filter.value === 'all' || (filter.value === 'attached') === !!d.vm_id) &&
      (!q || `${diskLabel(d)} ${d.vm_name ?? ''}`.toLowerCase().includes(q)),
  )
})
const { page, perPage, shown } = usePaging(matching)

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
    <header class="page-head">
      <h1>Disks</h1>
      <span v-if="disks" class="mono muted">{{ disks.length }}</span>
      <div class="actions">
        <button v-if="canCreate" class="btn primary" @click="creating = true">New disk</button>
      </div>
    </header>
    <p class="intro">
      A disk on a host’s own storage attaches to VMs on that host, and moves along when attached to a VM elsewhere.
    </p>

    <div v-if="disks && !disks.length" class="empty-state">
      <p>No disks{{ scope.solo ? '' : ` in ${scope.tenant?.name ?? 'this tenant'}` }} yet.</p>
    </div>
    <template v-else-if="disks">
      <div class="toolbar">
        <SegmentedFilter v-model="filter" :options="filters" label="Filter by attachment" />
        <label class="search">
          <span class="label">Name</span>
          <input v-model="name" type="search" placeholder="Filter disks or VMs" />
        </label>
      </div>
      <div class="table-wrap">
        <table class="data">
          <thead>
            <tr>
              <th>Disk</th>
              <th>State</th>
              <th v-if="mixed">Tenant</th>
              <th>VM</th>
              <th class="num">Size</th>
              <th>Storage</th>
              <th />
            </tr>
          </thead>
          <tbody>
            <tr v-for="d in shown" :key="d.id">
              <td>
                <span :class="{ named: d.name }">{{ diskLabel(d) }}</span>
                <span v-if="d.boot" class="tag">Boot</span>
              </td>
              <td>
                <StatusLed :tone="tone(d)" :label="DISK_STATES[d.state] ?? (d.vm_id ? 'Attached' : 'Free')" />
                <div v-if="d.error" class="error-text">{{ d.error }}</div>
              </td>
              <td v-if="mixed" class="muted">{{ scope.byId.get(d.tenant_id)?.name ?? '—' }}</td>
              <td>
                <RouterLink v-if="d.vm_id" :to="{ name: 'vm', params: { id: d.vm_id } }">{{ d.vm_name }}</RouterLink>
                <span v-else class="muted">None</span>
              </td>
              <td class="num mono nowrap">{{ d.size_gib }} GiB</td>
              <td>{{ diskWhere(d) }}</td>
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
        <p v-if="!matching.length" class="none muted">No disks match.</p>
      </div>
      <ListPager v-model:page="page" v-model:per-page="perPage" :total="matching.length" class="pager" />
    </template>
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
.named {
  font-weight: 600;
}
.tag {
  margin-left: 8px;
}
.nowrap {
  white-space: nowrap;
}
.none {
  margin: 0;
  padding: 28px;
}
.error-text {
  margin-top: 2px;
  font-size: 12px;
}
.end {
  white-space: nowrap;
}
.end .btn + .btn {
  margin-left: 6px;
}
</style>
