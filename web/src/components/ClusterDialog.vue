<script setup lang="ts">
import { ref } from 'vue'
import { api, errorText, ok, type Cluster } from '../api/client'
import ModalDialog from './ModalDialog.vue'

/** Adds a cluster, edits one (`cluster`), or deletes it (`remove`). */
const props = defineProps<{ cluster?: Cluster; remove?: boolean }>()
const emit = defineEmits<{ close: []; saved: [] }>()

type Limit = number | '' | null
const c = props.cluster
const form = ref({
  name: c?.name ?? '',
  cpu_overcommit: c?.cpu_overcommit ?? 4,
  memory_overcommit: c?.memory_overcommit ?? 1,
  ha_enabled: c?.ha_enabled ?? false,
  vm_net_mbps: (c?.vm_net_mbps ?? null) as Limit,
  vm_disk_iops: (c?.vm_disk_iops ?? null) as Limit,
  vm_disk_mbps: (c?.vm_disk_mbps ?? null) as Limit,
})
/** An empty field: no limit. */
const limit = (v: Limit) => (typeof v === 'number' && v > 0 ? v : null)
const error = ref('')
const busy = ref(false)

async function submit() {
  error.value = ''
  busy.value = true
  try {
    const f = form.value
    const body = {
      ...f,
      vm_net_mbps: limit(f.vm_net_mbps),
      vm_disk_iops: limit(f.vm_disk_iops),
      vm_disk_mbps: limit(f.vm_disk_mbps),
    }
    if (props.remove && c) {
      await ok(api.DELETE('/api/v1/clusters/{id}', { params: { path: { id: c.id } } }))
    } else if (c) {
      await ok(api.PUT('/api/v1/clusters/{id}', { params: { path: { id: c.id } }, body }))
    } else {
      await ok(api.POST('/api/v1/clusters', { body }))
    }
    emit('saved')
  } catch (e) {
    error.value = errorText(e)
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <ModalDialog
    :title="remove ? 'Delete cluster' : cluster ? `Edit ${cluster.name}` : 'Add cluster'"
    @close="emit('close')"
  >
    <form class="form" @submit.prevent="submit">
      <p v-if="remove && cluster">
        Delete <strong>{{ cluster.name }}</strong>? It must have no hosts left, and tenants lose access to it.
      </p>
      <template v-else>
        <label class="field">
          <span class="label">Name</span>
          <input v-model="form.name" required maxlength="64" autofocus />
        </label>
        <div class="row">
          <label class="field">
            <span class="label">vCPU overcommit</span>
            <input v-model.number="form.cpu_overcommit" type="number" min="1" max="64" step="0.5" required />
            <span class="hint">vCPUs per physical CPU.</span>
          </label>
          <label class="field">
            <span class="label">Memory overcommit</span>
            <input v-model.number="form.memory_overcommit" type="number" min="1" max="4" step="0.1" required />
            <span class="hint">1 means never more than the host has.</span>
          </label>
        </div>
        <label class="check">
          <input v-model="form.ha_enabled" type="checkbox" />
          High availability: when a host fails, power it off and restart its VMs on other hosts
        </label>
        <p class="hint muted">Needs fencing on every host (Manage). Only VMs on shared storage can restart elsewhere.</p>
        <fieldset class="limits">
          <legend class="label">Limits for each VM</legend>
          <p class="hint muted">So VMs sharing hosts and storage can’t starve each other. Leave empty for no limit.</p>
          <div class="row">
            <label class="field">
              <span class="label">Network (Mbit/s)</span>
              <input v-model.number="form.vm_net_mbps" type="number" min="1" max="400000" placeholder="No limit" />
              <span class="hint">Per NIC, each way.</span>
            </label>
            <label class="field">
              <span class="label">Disk IOPS</span>
              <input v-model.number="form.vm_disk_iops" type="number" min="1" max="10000000" placeholder="No limit" />
              <span class="hint">Per disk.</span>
            </label>
            <label class="field">
              <span class="label">Disk (MB/s)</span>
              <input v-model.number="form.vm_disk_mbps" type="number" min="1" max="100000" placeholder="No limit" />
              <span class="hint">Per disk.</span>
            </label>
          </div>
        </fieldset>
      </template>
      <p v-if="error" class="error-text" role="alert">{{ error }}</p>
      <footer>
        <button type="button" class="btn" @click="emit('close')">Cancel</button>
        <button class="btn" :class="remove ? 'danger' : 'primary'" :disabled="busy">
          {{ remove ? 'Delete cluster' : 'Save' }}
        </button>
      </footer>
    </form>
  </ModalDialog>
</template>

<style scoped>
.limits {
  display: grid;
  gap: 8px;
  margin: 0;
  padding: 10px 14px 14px;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
}
.limits legend {
  padding: 0 6px;
}
.hint {
  margin: 0;
  font-size: 12px;
}
</style>
