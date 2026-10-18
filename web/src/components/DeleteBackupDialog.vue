<script setup lang="ts">
import { computed, ref } from 'vue'
import { api, errorText, ok, type Backup } from '../api/client'
import { builtOn } from '../backups'
import { ago } from '../state'
import { useTasks } from '../stores/tasks'
import ModalDialog from './ModalDialog.vue'

const props = defineProps<{ backup: Backup; backups: Backup[] }>()
const emit = defineEmits<{ close: [] }>()
const tasks = useTasks()

const error = ref('')
const busy = ref(false)
const child = computed(() => builtOn(props.backup, props.backups))

async function remove() {
  error.value = ''
  busy.value = true
  try {
    await ok(api.DELETE('/api/v1/backups/{id}', { params: { path: { id: props.backup.id } } }))
    tasks.poke()
    emit('close')
  } catch (e) {
    error.value = errorText(e)
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <ModalDialog title="Delete backup" @close="emit('close')">
    <form class="form" @submit.prevent="remove">
      <p>
        Delete the backup of <strong>{{ backup.vm_name }}</strong> taken {{ ago(backup.created_at) }}? Its files are
        removed from the repository; this can’t be undone.
      </p>
      <p v-if="child">
        The backup taken {{ ago(child.created_at) }} builds on it: this one is merged into that one first, which stays
        whole.
      </p>
      <p v-if="error" class="error-text" role="alert">{{ error }}</p>
      <footer>
        <button type="button" class="btn" @click="emit('close')">Cancel</button>
        <button class="btn danger" :disabled="busy">Delete backup</button>
      </footer>
    </form>
  </ModalDialog>
</template>
