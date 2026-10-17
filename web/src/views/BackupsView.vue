<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue'
import { api, errorText, ok, type Backup } from '../api/client'
import MachineTabs from '../components/MachineTabs.vue'
import ModalDialog from '../components/ModalDialog.vue'
import RestoreBackupDialog from '../components/RestoreBackupDialog.vue'
import StatusLed from '../components/StatusLed.vue'
import { ago, gib } from '../state'
import { useLiveRefresh } from '../stores/live'
import { useScope } from '../stores/scope'
import { useTasks } from '../stores/tasks'

const scope = useScope()
const tasks = useTasks()
const backups = ref<Backup[] | null>(null)
const vmIds = ref(new Set<string>())
const error = ref('')
const busy = ref(false)

async function load() {
  const tenant = scope.current
  if (!tenant) return
  const [b, vms] = await Promise.all([
    ok(api.GET('/api/v1/backups', { params: { query: { tenant_id: tenant } } })),
    ok(api.GET('/api/v1/vms', { params: { query: { tenant_id: tenant, descendants: true } } })),
  ])
  if (tenant !== scope.current) return
  backups.value = b
  vmIds.value = new Set(vms.map((v) => v.id))
}
onMounted(load)
watch(() => scope.current, load)
useLiveRefresh(['backup', 'vm'], load)

const mixed = computed(() => backups.value?.some((b) => b.tenant_id !== scope.current) ?? false)
const canWrite = (b: Backup) => scope.can('vm.write', b.tenant_id)
const tone = (b: Backup) => (b.state === 'failed' ? 'fault' : b.state === 'running' ? 'busy' : 'on')

const restoring = ref<Backup | null>(null)
const deleting = ref<Backup | null>(null)

async function remove() {
  const b = deleting.value
  if (!b) return
  error.value = ''
  busy.value = true
  try {
    await ok(api.DELETE('/api/v1/backups/{id}', { params: { path: { id: b.id } } }))
    deleting.value = null
    tasks.poke()
  } catch (e) {
    error.value = errorText(e)
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <section>
    <MachineTabs />
    <header class="page-head">
      <h1>Backups</h1>
    </header>
    <p class="intro">
      Copies of VMs’ disks, kept apart from the storage they run on. A backup outlives its VM: restore it as a new VM
      from here. Back VMs up, or set their schedule, on each VM’s page.
    </p>

    <div v-if="backups && !backups.length" class="empty-state">
      <p>No backups{{ scope.solo ? '' : ` in ${scope.tenant?.name ?? 'this tenant'}` }} yet.</p>
    </div>
    <div v-else-if="backups" class="table-wrap">
      <table class="data">
        <thead>
          <tr>
            <th>VM</th>
            <th v-if="mixed">Tenant</th>
            <th>Taken</th>
            <th class="num">Size</th>
            <th>State</th>
            <th />
          </tr>
        </thead>
        <tbody>
          <tr v-for="b in backups" :key="b.id">
            <td>
              <RouterLink v-if="b.vm_id && vmIds.has(b.vm_id)" :to="{ name: 'vm', params: { id: b.vm_id } }">
                {{ b.vm_name }}
              </RouterLink>
              <template v-else>{{ b.vm_name }} <span class="muted">(deleted)</span></template>
            </td>
            <td v-if="mixed" class="muted">{{ scope.byId.get(b.tenant_id)?.name ?? '—' }}</td>
            <td :title="new Date(b.created_at).toLocaleString()">
              {{ ago(b.created_at) }}
              <span v-if="b.scheduled" class="tag">Scheduled</span>
            </td>
            <td class="num mono">{{ b.stored_bytes != null ? gib(b.stored_bytes) : '—' }}</td>
            <td>
              <StatusLed :tone="tone(b)" />
              {{
                b.state === 'ready'
                  ? b.consistent
                    ? 'Consistent'
                    : 'Crash-consistent'
                  : b.state === 'running'
                    ? 'Taking'
                    : 'Failed'
              }}
              <div v-if="b.error" class="error-text">{{ b.error }}</div>
            </td>
            <td class="end">
              <button v-if="b.state === 'ready' && canWrite(b)" class="btn small" @click="restoring = b">
                Restore…
              </button>
              <button v-if="b.state !== 'running' && canWrite(b)" class="btn small danger" @click="deleting = b">
                Delete…
              </button>
            </td>
          </tr>
        </tbody>
      </table>
    </div>
    <p v-if="error" class="error-text" role="alert">{{ error }}</p>

    <RestoreBackupDialog v-if="restoring" :backup="restoring" @close="restoring = null" />
    <ModalDialog v-if="deleting" title="Delete backup" @close="deleting = null">
      <form class="form" @submit.prevent="remove">
        <p>
          Delete the backup of <strong>{{ deleting.vm_name }}</strong> taken {{ ago(deleting.created_at) }}? Its files
          are removed from the repository; this can’t be undone.
        </p>
        <footer>
          <button type="button" class="btn" @click="deleting = null">Cancel</button>
          <button class="btn danger" :disabled="busy">Delete backup</button>
        </footer>
      </form>
    </ModalDialog>
  </section>
</template>

<style scoped>
.end .btn + .btn {
  margin-left: 6px;
}
</style>
