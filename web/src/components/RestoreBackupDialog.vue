<script setup lang="ts">
import { ref } from 'vue'
import { useRouter } from 'vue-router'
import { api, errorText, ok, type Backup } from '../api/client'
import { gib } from '../state'
import { useTasks } from '../stores/tasks'
import ModalDialog from './ModalDialog.vue'

const props = defineProps<{ backup: Backup }>()
const emit = defineEmits<{ close: [] }>()
const router = useRouter()
const tasks = useTasks()

const name = ref(`${props.backup.vm_name}-restored`.slice(0, 63))
const start = ref(true)
const error = ref('')
const busy = ref(false)

async function submit() {
  error.value = ''
  busy.value = true
  try {
    const vm = await ok(
      api.POST('/api/v1/backups/{id}/restore', {
        params: { path: { id: props.backup.id } },
        body: { name: name.value, start: start.value },
      }),
    )
    tasks.poke()
    emit('close')
    void router.push({ name: 'vm', params: { id: vm.id } })
  } catch (e) {
    error.value = errorText(e)
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <ModalDialog :title="`Restore ${backup.vm_name}`" @close="emit('close')">
    <form class="form" @submit.prevent="submit">
      <p>
        A new VM with the backup’s disks ({{ backup.disks.map((d) => gib(d)).join(', ') }}), {{ backup.vcpus }} vCPU{{
          backup.vcpus === 1 ? '' : 's'
        }}
        and {{ gib(backup.memory_mib * 1024 * 1024) }} of memory, on a new network interface. It starts as a new
        instance, so cloud-init sets up its network again; the disks keep everything else. The backup stays.
      </p>
      <label class="field">
        <span class="label">Name</span>
        <input id="restore-name" v-model="name" required maxlength="63" autofocus />
      </label>
      <label class="check">
        <input id="restore-start" v-model="start" type="checkbox" />
        Start it once restored
      </label>
      <p v-if="error" class="error-text" role="alert">{{ error }}</p>
      <footer>
        <button type="button" class="btn" @click="emit('close')">Cancel</button>
        <button class="btn primary" :disabled="busy">Restore as new VM</button>
      </footer>
    </form>
  </ModalDialog>
</template>
