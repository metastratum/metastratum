<script setup lang="ts">
import { computed, ref } from 'vue'
import { api, errorText, ok, type Vm } from '../api/client'
import ModalDialog from './ModalDialog.vue'

const props = defineProps<{ vm: Vm }>()
const emit = defineEmits<{ close: []; created: [vm: Vm] }>()

const name = ref(`${props.vm.name}-copy`.slice(0, 63))
const start = ref(true)
const password = ref('')
const error = ref('')
const busy = ref(false)
const nameOk = computed(() => /^[A-Za-z0-9]([A-Za-z0-9-]{0,61}[A-Za-z0-9])?$/.test(name.value))

async function submit() {
  error.value = ''
  busy.value = true
  try {
    const res = await ok(
      api.POST('/api/v1/vms/{id}/clone', {
        params: { path: { id: props.vm.id } },
        body: { name: name.value, start: start.value, password: password.value || null },
      }),
    )
    emit('created', res as unknown as Vm)
  } catch (e) {
    error.value = errorText(e)
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <ModalDialog :title="`Clone ${vm.name}`" @close="emit('close')">
    <form class="form" @submit.prevent="submit">
      <p>
        A new VM with copies of {{ vm.name }}’s disks, its CPUs and memory, in the same storage, on a new network
        interface. It boots as a new machine: cloud-init sets up its network, host name and SSH host keys again; the
        disks keep everything else.
      </p>
      <label class="field">
        <span class="label">Name</span>
        <input v-model="name" required maxlength="63" autofocus />
        <span class="hint" :class="{ 'error-text': name && !nameOk }">Letters, digits and hyphens.</span>
      </label>
      <label class="field">
        <span class="label">Console password</span>
        <input v-model="password" type="password" autocomplete="new-password" />
        <span class="hint">Optional: a new password for the image’s default user.</span>
      </label>
      <label class="check"><input v-model="start" type="checkbox" /> Start it once created</label>
      <p v-if="error" class="error-text" role="alert">{{ error }}</p>
      <footer>
        <button type="button" class="btn" @click="emit('close')">Cancel</button>
        <button class="btn primary" :disabled="busy || !nameOk">{{ busy ? 'Cloning…' : 'Clone' }}</button>
      </footer>
    </form>
  </ModalDialog>
</template>
