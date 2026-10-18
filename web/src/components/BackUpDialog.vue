<script setup lang="ts">
import { computed, ref } from 'vue'
import { api, errorText, ok, type Backup, type Schemas, type Vm } from '../api/client'
import { ago } from '../state'
import { useTasks } from '../stores/tasks'
import ModalDialog from './ModalDialog.vue'

const props = defineProps<{ vm: Vm; repos: Schemas['UsableRepository'][]; backups: Backup[] }>()
const emit = defineEmits<{ close: [] }>()
const tasks = useTasks()

const repositoryId = ref(props.repos[0]?.id ?? '')
const mode = ref<Schemas['BackupMode']>('incremental')
const error = ref('')
const busy = ref(false)

// What an incremental backup builds on: the newest complete one there.
const base = computed(() =>
  props.backups.find((b) => b.state === 'ready' && b.repository_id === repositoryId.value),
)

async function submit() {
  error.value = ''
  busy.value = true
  try {
    await ok(
      api.POST('/api/v1/vms/{id}/backups', {
        params: { path: { id: props.vm.id } },
        body: { repository_id: repositoryId.value || null, mode: mode.value },
      }),
    )
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
  <ModalDialog :title="`Back up ${vm.name}`" @close="emit('close')">
    <form class="form" @submit.prevent="submit">
      <p>
        {{
          vm.power === 'running'
            ? 'The backup is of this moment; the VM carries on meanwhile.'
            : 'The VM is stopped: its disks are copied as they are.'
        }}
      </p>
      <label v-if="repos.length > 1" class="field">
        <span class="label">Repository</span>
        <select id="backup-now-repository" v-model="repositoryId">
          <option v-for="r in repos" :key="r.id" :value="r.id">{{ r.name }}</option>
        </select>
      </label>
      <fieldset class="choice">
        <label class="check">
          <input id="backup-incremental" v-model="mode" type="radio" value="incremental" />
          <span>
            Incremental
            <span class="muted">
              — only what changed since
              <template v-if="base">the backup of {{ ago(base.created_at) }}</template>
              <template v-else>the last backup</template>, which it builds on. Restoring it gives the whole VM.
            </span>
          </span>
        </label>
        <label class="check">
          <input id="backup-full" v-model="mode" type="radio" value="full" />
          <span>Full <span class="muted">— every disk copied whole: a backup that stands alone.</span></span>
        </label>
      </fieldset>
      <p v-if="mode === 'incremental' && !base" class="muted">
        There is no backup of the VM {{ repos.length > 1 ? 'in this repository ' : '' }}to build on yet, so this one is
        taken in full.
      </p>
      <p v-if="error" class="error-text" role="alert">{{ error }}</p>
      <footer>
        <button type="button" class="btn" @click="emit('close')">Cancel</button>
        <button class="btn primary" :disabled="busy">Back up</button>
      </footer>
    </form>
  </ModalDialog>
</template>

<style scoped>
.choice {
  display: grid;
  gap: 6px;
  padding: 0;
  margin: 0;
  border: 0;
}
.choice .check {
  align-items: start;
}
</style>
