<script setup lang="ts">
import { ref } from 'vue'
import { api, errorText, ok, type Disk } from '../api/client'
import { diskLabel } from '../disks'
import ModalDialog from './ModalDialog.vue'

const props = defineProps<{ disk: Disk; growOnly?: boolean }>()
const emit = defineEmits<{ close: []; done: [] }>()

const name = ref(props.disk.name ?? '')
const sizeGib = ref(props.disk.size_gib)
const error = ref('')
const busy = ref(false)

async function submit() {
  error.value = ''
  const body: { name?: string; size_gib?: number } = {}
  if (!props.growOnly && name.value.trim() && name.value.trim() !== props.disk.name) body.name = name.value.trim()
  if (sizeGib.value !== props.disk.size_gib) body.size_gib = sizeGib.value
  if (!Object.keys(body).length) return emit('close')
  busy.value = true
  try {
    await ok(api.PATCH('/api/v1/disks/{id}', { params: { path: { id: props.disk.id } }, body }))
    emit('done')
  } catch (e) {
    error.value = errorText(e)
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <ModalDialog :title="growOnly ? `Grow ${diskLabel(disk)}` : `Change ${diskLabel(disk)}`" @close="emit('close')">
    <form class="form" @submit.prevent="submit">
      <p>
        Disks only grow. A running guest sees the larger disk at once; grow its partition and file system to use it
        (cloud-init does that for the boot disk at its next start).
      </p>
      <label v-if="!growOnly" class="field">
        <span class="label">Name</span>
        <input id="disk-rename" v-model="name" maxlength="64" />
      </label>
      <label class="field">
        <span class="label">Size (GiB)</span>
        <input id="disk-grow" v-model.number="sizeGib" type="number" :min="disk.size_gib" max="16384" required />
      </label>
      <p v-if="error" class="error-text" role="alert">{{ error }}</p>
      <footer>
        <button type="button" class="btn" @click="emit('close')">Cancel</button>
        <button class="btn primary" :disabled="busy">{{ growOnly ? 'Grow disk' : 'Save' }}</button>
      </footer>
    </form>
  </ModalDialog>
</template>
