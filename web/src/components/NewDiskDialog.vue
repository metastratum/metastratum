<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { api, errorText, ok, type Host, type Schemas, type Vm } from '../api/client'
import { useScope } from '../stores/scope'
import { useTasks } from '../stores/tasks'
import ModalDialog from './ModalDialog.vue'

const props = defineProps<{ tenantId: string }>()
const emit = defineEmits<{ close: []; done: [] }>()
const scope = useScope()
const tasks = useTasks()

const name = ref('')
const sizeGib = ref(10)
const poolId = ref<string | null>(null)
const vmId = ref<string>('')
const hostId = ref<string>('')
const pools = ref<Schemas['UsablePool'][]>([])
const vms = ref<Vm[]>([])
// Only those who see hosts choose one; others put local disks on a VM's.
const hosts = ref<Host[]>([])
const error = ref('')
const busy = ref(false)

onMounted(async () => {
  const [p, v, h] = await Promise.all([
    ok(api.GET('/api/v1/tenants/{id}/storage-pools', { params: { path: { id: props.tenantId } } })),
    ok(api.GET('/api/v1/vms', { params: { query: { tenant_id: props.tenantId } } })),
    api.GET('/api/v1/hosts').then((r) => r.data ?? []),
  ])
  pools.value = p
  poolId.value = p.find((x) => x.kind === 'local')?.id ?? p[0]?.id ?? null
  vms.value = v.filter((x) => !x.template && x.status === 'ready' && x.tenant_id === props.tenantId)
  hosts.value = h.filter((x) => x.state === 'ready')
})

const local = computed(() => pools.value.find((p) => p.id === poolId.value)?.kind === 'local')
const needsPlace = computed(() => local.value && !vmId.value && !hostId.value)

async function submit() {
  error.value = ''
  busy.value = true
  try {
    await ok(
      api.POST('/api/v1/disks', {
        body: {
          name: name.value,
          size_gib: sizeGib.value,
          tenant_id: props.tenantId,
          storage_pool_id: poolId.value,
          vm_id: vmId.value || null,
          host_id: local.value && !vmId.value ? hostId.value || null : null,
        },
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
  <ModalDialog title="New disk" @close="emit('close')">
    <form class="form" @submit.prevent="submit">
      <p>
        An empty disk{{ scope.solo ? '' : ` for ${scope.byId.get(tenantId)?.name ?? 'the tenant'}` }}. Attached to a VM, the
        guest finds it under <span class="mono">/dev/disk/by-id/</span>, by its ID: format it there.
      </p>
      <label class="field">
        <span class="label">Name</span>
        <input id="new-disk-name" v-model="name" required maxlength="64" autofocus />
      </label>
      <div class="row two">
        <label class="field">
          <span class="label">Size (GiB)</span>
          <input id="new-disk-size" v-model.number="sizeGib" type="number" min="1" max="16384" required />
        </label>
        <label class="field">
          <span class="label">Storage</span>
          <select id="new-disk-pool" v-model="poolId">
            <option v-for="p in pools" :key="p.id" :value="p.id">{{ p.kind === 'local' ? 'Local, on a host' : p.name }}</option>
          </select>
        </label>
      </div>
      <label class="field">
        <span class="label">Attach to</span>
        <select id="new-disk-vm" v-model="vmId">
          <option value="">No VM</option>
          <option v-for="v in vms" :key="v.id" :value="v.id">{{ v.name }}</option>
        </select>
        <span class="hint">Local storage is a host’s own: the disk is made on the VM’s host.</span>
      </label>
      <label v-if="local && !vmId && hosts.length" class="field">
        <span class="label">Host</span>
        <select id="new-disk-host" v-model="hostId" required>
          <option value="" disabled>Choose a host</option>
          <option v-for="h in hosts" :key="h.id" :value="h.id">{{ h.name }}</option>
        </select>
      </label>
      <p v-else-if="local && !vmId" class="muted">A disk on local storage is made on a VM’s host: choose the VM.</p>
      <p v-if="error" class="error-text" role="alert">{{ error }}</p>
      <footer>
        <button type="button" class="btn" @click="emit('close')">Cancel</button>
        <button class="btn primary" :disabled="busy || needsPlace">Create disk</button>
      </footer>
    </form>
  </ModalDialog>
</template>
