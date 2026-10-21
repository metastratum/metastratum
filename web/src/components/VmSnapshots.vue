<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { api, errorText, ok, type Snapshot, type Vm } from '../api/client'
import { ago } from '../state'
import { useLiveRefresh } from '../stores/live'
import { useScope } from '../stores/scope'
import { useTasks } from '../stores/tasks'
import ModalDialog from './ModalDialog.vue'
import StatusLed from './StatusLed.vue'

const props = defineProps<{ vm: Vm }>()
const scope = useScope()
const tasks = useTasks()

const snapshots = ref<Snapshot[] | null>(null)
const error = ref('')
const busy = ref(false)
const canWrite = computed(() => scope.can('vm.write', props.vm.tenant_id))
const working = computed(() =>
  snapshots.value?.some((s) => ['creating', 'reverting', 'deleting'].includes(s.state)),
)
const stopped = computed(() => props.vm.power === 'stopped' && props.vm.desired_power === 'stopped')

async function load() {
  snapshots.value = await ok(api.GET('/api/v1/vms/{id}/snapshots', { params: { path: { id: props.vm.id } } }))
}
onMounted(load)
useLiveRefresh(['snapshot'], load)

const taking = ref(false)
const name = ref('')

async function take() {
  error.value = ''
  busy.value = true
  try {
    await ok(
      api.POST('/api/v1/vms/{id}/snapshots', {
        params: { path: { id: props.vm.id } },
        body: { name: name.value.trim() || null },
      }),
    )
    taking.value = false
    name.value = ''
    tasks.poke()
    await load()
  } catch (e) {
    error.value = errorText(e)
  } finally {
    busy.value = false
  }
}

const reverting = ref<Snapshot | null>(null)
const deleting = ref<Snapshot | null>(null)

async function act(fn: () => Promise<unknown>) {
  error.value = ''
  busy.value = true
  try {
    await fn()
    reverting.value = null
    deleting.value = null
    tasks.poke()
    await load()
  } catch (e) {
    error.value = errorText(e)
  } finally {
    busy.value = false
  }
}

const revert = (s: Snapshot) =>
  act(() => ok(api.POST('/api/v1/snapshots/{id}/revert', { params: { path: { id: s.id } } })))
const remove = (s: Snapshot) =>
  act(() => ok(api.DELETE('/api/v1/snapshots/{id}', { params: { path: { id: s.id } } })))

// A few at first; all of them, scrolling, on request.
const FEW = 3
const all = ref(false)
const shown = computed(() => (all.value ? snapshots.value : snapshots.value?.slice(0, FEW)) ?? [])

const tone = (s: Snapshot) => (s.state === 'failed' ? 'fault' : s.state === 'ready' ? 'on' : 'busy')
const doing: Record<string, string> = { creating: 'Taking', reverting: 'Reverting', deleting: 'Deleting' }
</script>

<template>
  <div class="snapshots">
    <div class="vm-sub-head">
      <span class="label">Snapshots</span>
      <button
        v-if="canWrite && !taking"
        type="button"
        class="btn small"
        :disabled="busy || working || vm.status !== 'ready'"
        @click="taking = true"
      >
        Take snapshot
      </button>
    </div>

    <form v-if="taking" class="take" @submit.prevent="take">
      <label class="field">
        <span class="name-label">Name</span>
        <input id="snapshot-name" v-model="name" maxlength="64" placeholder="The date and time" />
      </label>
      <button type="button" class="btn" @click="taking = false">Cancel</button>
      <button class="btn primary" :disabled="busy">Take</button>
    </form>

    <div v-if="snapshots?.length" class="vm-rows">
      <ul :class="{ all }">
        <li v-for="s in shown" :key="s.id">
          <StatusLed :tone="tone(s)" />
          <strong class="name">{{ s.name }}</strong>
          <span class="muted" :title="new Date(s.created_at).toLocaleString()">{{ ago(s.created_at) }}</span>
          <span v-if="canWrite && s.state !== 'creating'" class="end">
            <button
              v-if="s.state === 'ready'"
              type="button"
              class="btn small"
              :disabled="busy || working"
              :title="stopped ? undefined : 'Stop the VM to revert it'"
              @click="reverting = s"
            >
              Revert…
            </button>
            <button
              v-if="s.state === 'ready' || s.state === 'failed'"
              type="button"
              class="btn small danger"
              :disabled="busy || working"
              @click="deleting = s"
            >
              Delete…
            </button>
          </span>
          <span class="meta">
            <span v-if="doing[s.state]" class="tag">{{ doing[s.state] }}</span>
            <span
              v-if="s.state === 'ready' && s.consistent != null"
              class="tag"
              :title="
                s.consistent
                  ? 'The VM was stopped, or its guest agent froze its file systems'
                  : 'Taken without a guest agent: as after a crash, which journaling file systems recover from'
              "
              >{{ s.consistent ? 'Consistent' : 'Crash-consistent' }}</span
            >
            <span v-if="s.error" class="error-text">{{ s.error }}</span>
          </span>
        </li>
      </ul>
      <button v-if="snapshots.length > FEW" type="button" class="more-rows" @click="all = !all">
        {{ all ? 'Show fewer' : `Show ${snapshots.length - FEW} more snapshots` }}
      </button>
    </div>
    <p v-else-if="snapshots && !taking" class="vm-none">
      No snapshots. A snapshot keeps the VM’s disks as they are now, to put them back to later.
    </p>
    <p v-if="error" class="error-text" role="alert">{{ error }}</p>

    <ModalDialog v-if="reverting" title="Revert to snapshot" @close="reverting = null">
      <form class="form" @submit.prevent="revert(reverting)">
        <p v-if="stopped">
          Put <strong>{{ vm.name }}</strong>’s disks back as they were in <strong>{{ reverting.name }}</strong>,
          taken {{ ago(reverting.created_at) }}? What was written since is lost. Its other snapshots stay.
        </p>
        <p v-else>Stop <strong>{{ vm.name }}</strong> first: only a stopped VM can be reverted.</p>
        <footer>
          <button type="button" class="btn" @click="reverting = null">Cancel</button>
          <button class="btn danger" :disabled="busy || !stopped">Revert</button>
        </footer>
      </form>
    </ModalDialog>
    <ModalDialog v-if="deleting" title="Delete snapshot" @close="deleting = null">
      <form class="form" @submit.prevent="remove(deleting)">
        <p>
          Delete the snapshot <strong>{{ deleting.name }}</strong> of {{ vm.name }}? The VM’s disks stay as they are;
          you can no longer put them back to it.
        </p>
        <footer>
          <button type="button" class="btn" @click="deleting = null">Cancel</button>
          <button class="btn danger" :disabled="busy">Delete snapshot</button>
        </footer>
      </form>
    </ModalDialog>
  </div>
</template>

<style scoped>
.snapshots {
  display: grid;
  align-content: start;
  gap: 10px;
}
.take {
  display: flex;
  flex-wrap: wrap;
  align-items: end;
  gap: 8px;
}
.take .field {
  flex: 1;
  min-width: 140px;
}
.name-label {
  font-size: 12px;
  color: var(--muted);
}
.name {
  font-weight: 500;
  overflow-wrap: anywhere;
}
</style>
