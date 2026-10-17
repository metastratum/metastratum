<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { api, errorText, ok, type Disk, type Vm } from '../api/client'
import { attachDisk, diskLabel, movesTo } from '../disks'
import { useTasks } from '../stores/tasks'
import ModalDialog from './ModalDialog.vue'

const props = defineProps<{ disk: Disk }>()
const emit = defineEmits<{ close: []; done: [] }>()
const tasks = useTasks()

const vms = ref<Vm[]>([])
const vmId = ref<string | null>(null)
const error = ref('')
const busy = ref(false)

onMounted(async () => {
  const v = await ok(api.GET('/api/v1/vms', { params: { query: { tenant_id: props.disk.tenant_id } } }))
  vms.value = v.filter((x) => x.tenant_id === props.disk.tenant_id && x.status === 'ready')
  // Those on its host first: they take it as it is.
  vms.value.sort((a, b) => Number(movesTo(props.disk, a.host_id)) - Number(movesTo(props.disk, b.host_id)))
  vmId.value = vms.value[0]?.id ?? null
})

const vm = computed(() => vms.value.find((v) => v.id === vmId.value) ?? null)
const moves = computed(() => !!vm.value && movesTo(props.disk, vm.value.host_id))

async function submit() {
  if (!vm.value) return
  error.value = ''
  busy.value = true
  try {
    if ((await attachDisk(props.disk, vm.value)) === 'moving') tasks.poke()
    emit('done')
  } catch (e) {
    error.value = errorText(e)
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <ModalDialog :title="`Attach ${diskLabel(disk)}`" @close="emit('close')">
    <form class="form" @submit.prevent="submit">
      <p>
        A running VM has it plugged in at once; a stopped one from its next start. The guest finds it under
        <span class="mono">/dev/disk/by-id/</span>, by the disk’s ID.
      </p>
      <label class="field">
        <span class="label">VM</span>
        <select id="attach-vm" v-model="vmId" required>
          <option v-for="v in vms" :key="v.id" :value="v.id">
            {{ v.name }}{{ movesTo(disk, v.host_id) ? ` (on ${v.host_name ?? 'another host'})` : '' }}
          </option>
        </select>
      </label>
      <p v-if="moves" class="muted">
        The disk is on {{ disk.host_name }}’s storage: it is copied to {{ vm?.host_name }} first, then attached.
      </p>
      <p v-if="!vms.length" class="muted">The tenant has no VM to attach it to.</p>
      <p v-if="error" class="error-text" role="alert">{{ error }}</p>
      <footer>
        <button type="button" class="btn" @click="emit('close')">Cancel</button>
        <button class="btn primary" :disabled="busy || !vm">{{ moves ? 'Move and attach' : 'Attach disk' }}</button>
      </footer>
    </form>
  </ModalDialog>
</template>
