<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { api, errorText, ok, type Disk, type Schemas, type Vm } from '../api/client'
import { attachDisk, diskWhere, movesTo } from '../disks'
import { useTasks } from '../stores/tasks'
import ModalDialog from './ModalDialog.vue'

const props = defineProps<{ vm: Vm }>()
const emit = defineEmits<{ close: []; done: [] }>()
const tasks = useTasks()

const kind = ref<'new' | 'existing'>('new')
const name = ref(`${props.vm.name}-data`.slice(0, 64))
const sizeGib = ref(10)
const poolId = ref<string | null>(null)
const pools = ref<Schemas['UsablePool'][]>([])
const free = ref<Disk[]>([])
const diskId = ref<string | null>(null)
const error = ref('')
const busy = ref(false)

onMounted(async () => {
  const [p, d] = await Promise.all([
    ok(api.GET('/api/v1/tenants/{id}/storage-pools', { params: { path: { id: props.vm.tenant_id } } })),
    ok(api.GET('/api/v1/disks', { params: { query: { tenant_id: props.vm.tenant_id } } })),
  ])
  pools.value = p
  poolId.value = p.find((x) => x.kind === 'local')?.id ?? p[0]?.id ?? null
  free.value = d.filter((x) => !x.vm_id && x.state === 'ready' && x.tenant_id === props.vm.tenant_id)
  diskId.value = free.value[0]?.id ?? null
})

const chosen = computed(() => free.value.find((d) => d.id === diskId.value) ?? null)
const running = computed(() => props.vm.power !== 'stopped')

async function submit() {
  error.value = ''
  busy.value = true
  try {
    if (kind.value === 'new') {
      await ok(
        api.POST('/api/v1/disks', {
          body: { name: name.value, size_gib: sizeGib.value, storage_pool_id: poolId.value, vm_id: props.vm.id },
        }),
      )
    } else if (chosen.value) {
      await attachDisk(chosen.value, props.vm)
    }
    tasks.poke()
    emit('done')
  } catch (e) {
    error.value = errorText(e)
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <ModalDialog :title="`Add a disk to ${vm.name}`" @close="emit('close')">
    <form class="form" @submit.prevent="submit">
      <p>
        {{
          running
            ? 'It is plugged in while the VM runs: the guest sees it at once.'
            : 'The VM has it from its next start.'
        }}
        The guest finds it under <span class="mono">/dev/disk/by-id/</span>, by the disk’s ID.
      </p>
      <fieldset class="choice">
        <label class="check">
          <input v-model="kind" type="radio" value="new" />
          A new, empty disk
        </label>
        <label class="check">
          <input v-model="kind" type="radio" value="existing" :disabled="!free.length" />
          A disk attached to no VM{{ free.length ? '' : ' (there is none)' }}
        </label>
      </fieldset>
      <template v-if="kind === 'new'">
        <label class="field">
          <span class="label">Name</span>
          <input id="disk-name" v-model="name" required maxlength="64" />
        </label>
        <div class="row two">
          <label class="field">
            <span class="label">Size (GiB)</span>
            <input id="disk-size" v-model.number="sizeGib" type="number" min="1" max="16384" required />
          </label>
          <label v-if="pools.length > 1" class="field">
            <span class="label">Storage</span>
            <select id="disk-pool" v-model="poolId">
              <option v-for="p in pools" :key="p.id" :value="p.id">
                {{ p.kind === 'local' ? `Local, on ${vm.host_name ?? 'its host'}` : p.name }}
              </option>
            </select>
          </label>
        </div>
      </template>
      <template v-else>
        <label class="field">
          <span class="label">Disk</span>
          <select id="disk-existing" v-model="diskId">
            <option v-for="d in free" :key="d.id" :value="d.id">
              {{ d.name ?? d.id }} · {{ d.size_gib }} GiB · {{ diskWhere(d) }}
            </option>
          </select>
        </label>
        <p v-if="chosen && movesTo(chosen, vm.host_id)" class="muted">
          It is on another host’s storage: it is copied to {{ vm.host_name }} first, then attached.
        </p>
      </template>
      <p v-if="error" class="error-text" role="alert">{{ error }}</p>
      <footer>
        <button type="button" class="btn" @click="emit('close')">Cancel</button>
        <button class="btn primary" :disabled="busy || (kind === 'existing' && !chosen)">
          {{ kind === 'new' ? 'Add disk' : chosen && movesTo(chosen, vm.host_id) ? 'Move and attach' : 'Attach disk' }}
        </button>
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
</style>
