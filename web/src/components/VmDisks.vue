<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { api, errorText, ok, type Disk, type Vm } from '../api/client'
import { DISK_STATES, diskWhere } from '../disks'
import { useLiveRefresh } from '../stores/live'
import { useScope } from '../stores/scope'
import { useTasks } from '../stores/tasks'
import AddDiskDialog from './AddDiskDialog.vue'
import EditDiskDialog from './EditDiskDialog.vue'
import IdChip from './IdChip.vue'
import ModalDialog from './ModalDialog.vue'
import StatusLed from './StatusLed.vue'

const props = defineProps<{ vm: Vm }>()
const scope = useScope()
const tasks = useTasks()

const disks = ref<Disk[] | null>(null)
const error = ref('')
const busy = ref(false)
const canWrite = computed(() => scope.can('vm.write', props.vm.tenant_id))
const settled = computed(() => props.vm.status === 'ready' && props.vm.power === props.vm.desired_power)

async function load() {
  disks.value = await ok(api.GET('/api/v1/vms/{id}/disks', { params: { path: { id: props.vm.id } } }))
}
onMounted(load)
useLiveRefresh(['disk', 'vm'], load, (n) => n.kind === 'disk' || n.id === props.vm.id)

const adding = ref(false)
const editing = ref<Disk | null>(null)
const detaching = ref<Disk | null>(null)

async function detach(d: Disk) {
  error.value = ''
  busy.value = true
  try {
    await ok(api.DELETE('/api/v1/vms/{id}/disks/{disk_id}', { params: { path: { id: props.vm.id, disk_id: d.id } } }))
    tasks.poke()
    detaching.value = null
    await load()
  } catch (e) {
    error.value = errorText(e)
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <section class="disks">
    <div class="head">
      <h2 class="label">Disks</h2>
      <button v-if="canWrite" class="btn small" :disabled="!settled" @click="adding = true">Add disk…</button>
    </div>
    <ul v-if="disks" class="list">
      <li v-for="d in disks" :key="d.id">
        <StatusLed :tone="d.state === 'failed' ? 'fault' : d.state === 'ready' ? 'on' : 'busy'" />
        <span class="main">
          <span class="name">{{ d.boot ? 'Boot disk' : (d.name ?? 'Disk') }}</span>
          <span class="muted">{{ d.size_gib }} GiB · {{ diskWhere(d) }}</span>
          <span v-if="d.guest_path" class="path mono" :title="`In the guest: ${d.guest_path}`">{{ d.guest_path }}</span>
        </span>
        <span v-if="canWrite" class="actions">
          <button class="btn small" :disabled="busy || !settled || d.state !== 'ready'" @click="editing = d">Grow…</button>
          <button v-if="!d.boot" class="btn small" :disabled="busy || !settled || d.state !== 'ready'" @click="detaching = d">
            Detach…
          </button>
        </span>
        <span class="meta">
          <IdChip :value="d.id" short />
          <span v-if="DISK_STATES[d.state]" class="tag">{{ DISK_STATES[d.state] }}</span>
          <span v-if="d.error" class="error-text">{{ d.error }}</span>
        </span>
      </li>
    </ul>
    <p v-if="error" class="error-text" role="alert">{{ error }}</p>

    <AddDiskDialog v-if="adding" :vm="vm" @close="adding = false" @done="((adding = false), load())" />
    <EditDiskDialog v-if="editing" :disk="editing" grow-only @close="editing = null" @done="((editing = null), load())" />
    <ModalDialog v-if="detaching" title="Detach disk" @close="detaching = null">
      <form class="form" @submit.prevent="detach(detaching)">
        <p>
          Detach <strong>{{ detaching.name ?? 'the disk' }}</strong> from {{ vm.name }}?
          <template v-if="vm.power !== 'stopped'">
            It is unplugged while the VM runs: unmount it in the guest first, or what is still being written is lost.
          </template>
          The disk and what it holds are kept, to attach again (Machines → Disks).
        </p>
        <footer>
          <button type="button" class="btn" @click="detaching = null">Cancel</button>
          <button class="btn danger" :disabled="busy">Detach disk</button>
        </footer>
      </form>
    </ModalDialog>
  </section>
</template>

<style scoped>
.disks {
  display: grid;
  align-content: start;
  gap: 10px;
}
.head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
}
.list {
  display: grid;
  gap: 8px;
  padding: 0;
  margin: 0;
  list-style: none;
}
.list li {
  display: grid;
  grid-template-columns: auto 1fr auto;
  align-items: start;
  gap: 4px 10px;
}
.main {
  display: grid;
  gap: 2px;
  min-width: 0;
}
.name {
  font-weight: 500;
}
.path {
  overflow: hidden;
  font-size: 11px;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: var(--muted);
}
.actions {
  display: flex;
  gap: 6px;
}
.meta {
  display: flex;
  flex-wrap: wrap;
  grid-column: 2 / -1;
  align-items: center;
  gap: 6px;
}
</style>
