<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { api, errorText, ok, type Vm } from '../api/client'
import ModalDialog from './ModalDialog.vue'

const props = defineProps<{ vm: Vm }>()
const emit = defineEmits<{ close: []; done: [vm: Vm] }>()

const GIB = 1024 ** 3
const MEMORY = [512, 1024, 2048, 4096, 8192, 16384, 32768, 65536]

const stopped = computed(
  () => props.vm.status === 'ready' && props.vm.power === 'stopped' && props.vm.desired_power === 'stopped',
)
// A running VM's memory changes through its balloon, within its maximum,
// when the guest's driver reports.
const running = computed(
  () => props.vm.status === 'ready' && props.vm.power === 'running' && props.vm.desired_power === 'running',
)
const balloon = ref<boolean | null>(null)
// Asked again while it says no: a guest still booting, or just migrated,
// reports within seconds.
let timer = 0
async function checkBalloon() {
  if (!running.value || balloon.value) return
  const { data } = await api.GET('/api/v1/vms/{id}/memory', { params: { path: { id: props.vm.id } } })
  balloon.value = data?.driver ?? false
}
onMounted(() => {
  void checkBalloon()
  timer = window.setInterval(checkBalloon, 3000)
})
onUnmounted(() => clearInterval(timer))
const vcpus = ref(props.vm.vcpus)
const maxVcpus = ref(props.vm.max_vcpus)
const vcpusLive = computed(() => running.value && props.vm.cpu_hotplug)
const memory = ref(props.vm.memory_mib)
const maxMemory = ref(props.vm.max_memory_mib)
const sizes = (extra: number[]) => [...new Set([...MEMORY, ...extra])].sort((a, b) => a - b)
const memoryOptions = computed(() =>
  running.value
    ? sizes([props.vm.memory_mib, props.vm.max_memory_mib]).filter((m) => m <= props.vm.max_memory_mib)
    : sizes([props.vm.memory_mib]),
)
const maxOptions = computed(() => sizes([props.vm.max_memory_mib, memory.value]).filter((m) => m >= memory.value))
const memoryLive = computed(() => running.value && balloon.value === true)
const vcpuMax = computed(() => (stopped.value ? 128 : props.vm.max_vcpus))
const maxVcpuOptions = computed(() =>
  [...new Set([2, 4, 8, 16, 32, 64, 128, props.vm.max_vcpus, vcpus.value])].filter((n) => n >= vcpus.value).sort((a, b) => a - b),
)
const current = props.vm.disks.map((d) => Math.ceil(d.size_bytes / GIB))
const disks = ref([...current])

const error = ref('')
const busy = ref(false)
const changed = computed(
  () =>
    vcpus.value !== props.vm.vcpus ||
    maxVcpus.value !== props.vm.max_vcpus ||
    memory.value !== props.vm.memory_mib ||
    maxMemory.value !== props.vm.max_memory_mib ||
    disks.value.some((g, i) => g !== current[i]),
)

const label = (m: number) => (m >= 1024 ? `${m / 1024} GiB` : `${m} MiB`)

async function submit() {
  error.value = ''
  busy.value = true
  let vm = props.vm
  try {
    if (
      vcpus.value !== vm.vcpus ||
      maxVcpus.value !== vm.max_vcpus ||
      memory.value !== vm.memory_mib ||
      maxMemory.value !== vm.max_memory_mib
    ) {
      const body = stopped.value
        ? {
            vcpus: vcpus.value,
            max_vcpus: Math.max(maxVcpus.value, vcpus.value),
            memory_mib: memory.value,
            max_memory_mib: Math.max(maxMemory.value, memory.value),
          }
        : { vcpus: vcpus.value, memory_mib: memory.value }
      vm = await ok(api.PATCH('/api/v1/vms/{id}', { params: { path: { id: vm.id } }, body }))
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
          <input v-model.number="vcpus" type="number" min="1" :max="vcpuMax" required :disabled="!stopped && !vcpusLive" />
        </label>
        <label class="field">
          <span class="label">CPUs can grow to</span>
          <select v-model.number="maxVcpus" :disabled="!stopped">
            <option v-for="n in maxVcpuOptions" :key="n" :value="n">{{ n }}</option>
          </select>
        </label>
        <label class="field">
          <span class="label">Memory</span>
          <select v-model.number="memory" :disabled="!stopped && !memoryLive">
            <option v-for="m in memoryOptions" :key="m" :value="m">{{ label(m) }}</option>
          </select>
        </label>
        <label class="field">
          <span class="label">Memory can grow to</span>
          <select v-model.number="maxMemory" :disabled="!stopped">
            <option v-for="m in maxOptions" :key="m" :value="m">{{ label(m) }}</option>
          </select>
        </label>
      </div>
      <p v-if="vcpusLive" class="hint">
        CPUs are plugged in or out at once, up to {{ vm.max_vcpus }}.
      </p>
      <p v-if="memoryLive" class="hint">
        Memory changes at once, up to {{ label(vm.max_memory_mib) }}: the guest’s balloon gives it back or takes it.
        Stop the VM to change what they can grow to.
      </p>
      <p v-else-if="running && balloon === null" class="hint">Asking the guest whether its memory can change…</p>
      <p v-else-if="running && balloon === false" class="hint">
        The guest’s balloon driver isn’t reporting (virtio_balloon), so its memory can’t change while it runs. Stop the
        VM to change it.
      </p>
      <p v-else-if="!stopped && !vcpusLive" class="hint">Stop the VM to change its CPUs or memory.</p>
      <p v-else-if="stopped" class="hint">
        The host keeps what CPUs and memory can grow to for the VM, and that counts against quotas; while the VM runs,
        they can then change up to there without a restart.
      </p>
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
