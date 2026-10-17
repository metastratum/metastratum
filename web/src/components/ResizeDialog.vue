<script setup lang="ts">
import { computed, ref } from 'vue'
import { api, errorText, ok, type Vm } from '../api/client'
import ModalDialog from './ModalDialog.vue'

const props = defineProps<{ vm: Vm }>()
const emit = defineEmits<{ close: []; done: [vm: Vm] }>()

const GIB = 1024 ** 3
const MEMORY = [512, 1024, 2048, 4096, 8192, 16384, 32768, 65536]

const stopped = computed(
  () => props.vm.status === 'ready' && props.vm.power === 'stopped' && props.vm.desired_power === 'stopped',
)
const vcpus = ref(props.vm.vcpus)
const memory = ref(props.vm.memory_mib)
const memoryOptions = computed(() => [...new Set([...MEMORY, props.vm.memory_mib])].sort((a, b) => a - b))
const current = props.vm.disks.map((d) => Math.ceil(d.size_bytes / GIB))
const disks = ref([...current])

const error = ref('')
const busy = ref(false)
const changed = computed(
  () =>
    vcpus.value !== props.vm.vcpus ||
    memory.value !== props.vm.memory_mib ||
    disks.value.some((g, i) => g !== current[i]),
)

const label = (m: number) => (m >= 1024 ? `${m / 1024} GiB` : `${m} MiB`)

async function submit() {
  error.value = ''
  busy.value = true
  let vm = props.vm
  try {
    if (vcpus.value !== vm.vcpus || memory.value !== vm.memory_mib) {
      vm = await ok(
        api.PATCH('/api/v1/vms/{id}', {
          params: { path: { id: vm.id } },
          body: { vcpus: vcpus.value, memory_mib: memory.value },
        }),
      )
    }
    for (const [i, gib] of disks.value.entries()) {
      if (gib === current[i]) continue
      vm = await ok(
        api.POST('/api/v1/vms/{id}/disks/{index}/resize', {
          params: { path: { id: vm.id, index: i } },
          body: { size_gib: gib },
        }),
      )
    }
    emit('done', vm)
  } catch (e) {
    error.value = errorText(e)
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <ModalDialog :title="`Resize ${vm.name}`" @close="emit('close')">
    <form class="form" @submit.prevent="submit">
      <div class="row">
        <label class="field">
          <span class="label">vCPUs</span>
          <input v-model.number="vcpus" type="number" min="1" max="128" required :disabled="!stopped" />
        </label>
        <label class="field">
          <span class="label">Memory</span>
          <select v-model.number="memory" :disabled="!stopped">
            <option v-for="m in memoryOptions" :key="m" :value="m">{{ label(m) }}</option>
          </select>
        </label>
      </div>
      <p v-if="!stopped" class="hint">Stop the VM to change its CPUs or memory.</p>
      <label v-for="(d, i) in vm.disks" :key="d.volume_id" class="field">
        <span class="label">{{ i === 0 ? 'Boot disk' : `Disk ${i + 1}` }} (GiB)</span>
        <input v-model.number="disks[i]" type="number" :min="current[i]" max="16384" required />
      </label>
      <p class="hint">
        Disks only grow. A running VM sees the larger disk at once; its partition and file system grow at its next
        boot.
      </p>
      <p v-if="error" class="error-text" role="alert">{{ error }}</p>
      <footer>
        <button type="button" class="btn" @click="emit('close')">Cancel</button>
        <button class="btn primary" :disabled="busy || !changed">{{ busy ? 'Resizing…' : 'Resize' }}</button>
      </footer>
    </form>
  </ModalDialog>
</template>

<style scoped>
.row {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 12px;
}
</style>
