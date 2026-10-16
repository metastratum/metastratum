<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { api, errorText, ok, type Host, type Vm } from '../api/client'
import ModalDialog from './ModalDialog.vue'

const props = defineProps<{ vm: Vm }>()
const emit = defineEmits<{ close: []; started: [taskId: string] }>()

const hosts = ref<Host[] | null>(null)
const choice = ref<string | null>(null)
const error = ref('')
const busy = ref(false)

onMounted(async () => {
  try {
    hosts.value = await ok(api.GET('/api/v1/hosts'))
  } catch (e) {
    error.value = errorText(e)
  }
})

const source = computed(() => hosts.value?.find((h) => h.id === props.vm.host_id))
/** Other hosts of the VM's cluster, and why any cannot take it. */
const options = computed(() =>
  (hosts.value ?? [])
    .filter((h) => h.id !== props.vm.host_id && h.cluster_id && h.cluster_id === source.value?.cluster_id)
    .map((h) => ({
      host: h,
      why: !h.connected
        ? 'not connected'
        : h.state !== 'ready'
          ? h.state
          : !h.migration_ready
            ? 'not set up for migration'
            : h.memory_mib - h.allocated_memory_mib < props.vm.memory_mib
              ? 'not enough memory'
              : '',
    })),
)
const anyUsable = computed(() => options.value.some((o) => !o.why))
const local = computed(() => props.vm.disks.some((d) => d.pool_id === '00000000-0000-0000-0000-000000000001'))

async function submit() {
  error.value = ''
  busy.value = true
  try {
    const t = await ok(
      api.POST('/api/v1/vms/{id}/migrate', { params: { path: { id: props.vm.id } }, body: { host_id: choice.value } }),
    )
    emit('started', t.task_id)
  } catch (e) {
    error.value = errorText(e)
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <ModalDialog :title="`Migrate ${vm.name}`" @close="emit('close')">
    <form class="form" @submit.prevent="submit">
      <p>
        Moves the running VM from <strong>{{ vm.host_name }}</strong> to another host of its cluster. The guest keeps
        running; it pauses for a fraction of a second at the switch.
      </p>
      <p v-if="local" class="muted">Its disk is on the host, so it is copied along; that takes longer.</p>
      <fieldset v-if="hosts" class="hosts">
        <legend class="label">Destination</legend>
        <label class="check">
          <input v-model="choice" type="radio" :value="null" :disabled="!anyUsable" />
          Let hypermon choose
        </label>
        <label v-for="o in options" :key="o.host.id" class="check" :class="{ off: o.why }">
          <input v-model="choice" type="radio" :value="o.host.id" :disabled="!!o.why" />
          <span class="mono">{{ o.host.name }}</span>
          <span v-if="o.why" class="muted">— {{ o.why }}</span>
        </label>
        <p v-if="!options.length" class="muted">No other host is in this VM’s cluster.</p>
      </fieldset>
      <p v-if="error" class="error-text" role="alert">{{ error }}</p>
      <footer>
        <button type="button" class="btn" @click="emit('close')">Cancel</button>
        <button class="btn primary" :disabled="busy || !anyUsable">Migrate</button>
      </footer>
    </form>
  </ModalDialog>
</template>

<style scoped>
.hosts {
  display: grid;
  gap: 8px;
  margin: 0;
  padding: 12px 14px;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
}
.hosts p {
  margin: 0;
}
.off {
  color: var(--muted);
}
</style>
