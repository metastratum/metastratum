<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { api, errorText, ok, type Disk, type Host, type Schemas } from '../api/client'
import { diskLabel, diskWhere } from '../disks'
import { useTasks } from '../stores/tasks'
import ModalDialog from './ModalDialog.vue'

const props = defineProps<{ disk: Disk }>()
const emit = defineEmits<{ close: []; done: [] }>()
const tasks = useTasks()

const pools = ref<Schemas['UsablePool'][]>([])
const hosts = ref<Host[]>([])
const poolId = ref(props.disk.storage_pool_id)
const hostId = ref('')
const error = ref('')
const busy = ref(false)

onMounted(async () => {
  const [p, h] = await Promise.all([
    ok(api.GET('/api/v1/tenants/{id}/storage-pools', { params: { path: { id: props.disk.tenant_id } } })),
    api.GET('/api/v1/hosts').then((r) => r.data ?? []),
  ])
  pools.value = p
  hosts.value = h.filter((x) => x.state === 'ready' && x.connected)
  hostId.value = hosts.value.find((x) => x.id !== props.disk.host_id)?.id ?? ''
})

const local = computed(() => pools.value.find((p) => p.id === poolId.value)?.kind === 'local')
const there = computed(
  () => poolId.value === props.disk.storage_pool_id && (!local.value || hostId.value === props.disk.host_id),
)

async function submit() {
  error.value = ''
  busy.value = true
  try {
    await ok(
      api.POST('/api/v1/disks/{id}/move', {
        params: { path: { id: props.disk.id } },
        body: { storage_pool_id: poolId.value, host_id: local.value ? hostId.value || null : null },
      }),
    )
    tasks.poke()
    emit('done')
  } catch (e) {
    error.value = errorText(e)
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <ModalDialog :title="`Move ${diskLabel(disk)}`" @close="emit('close')">
    <form class="form" @submit.prevent="submit">
      <p>
        It is now: {{ diskWhere(disk) }}. It is copied where it goes, with everything it holds, then removed here. A disk
        on a host’s own storage attaches to VMs on that host; on shared storage, to VMs on any host that reaches it.
      </p>
      <label class="field">
        <span class="label">Storage</span>
        <select id="move-pool" v-model="poolId">
          <option v-for="p in pools" :key="p.id" :value="p.id">{{ p.kind === 'local' ? 'Local, on a host' : p.name }}</option>
        </select>
      </label>
      <label v-if="local && hosts.length" class="field">
        <span class="label">Host</span>
        <select id="move-host" v-model="hostId" required>
          <option v-for="h in hosts" :key="h.id" :value="h.id">
            {{ h.name }}{{ h.id === disk.host_id ? ' (where it is)' : '' }}
          </option>
        </select>
      </label>
      <p v-else-if="local" class="muted">To move it to another host’s storage, attach it to a VM there: it moves along.</p>
      <p v-if="error" class="error-text" role="alert">{{ error }}</p>
      <footer>
        <button type="button" class="btn" @click="emit('close')">Cancel</button>
        <button class="btn primary" :disabled="busy || there || (local && !hosts.length)">Move disk</button>
      </footer>
    </form>
  </ModalDialog>
</template>
