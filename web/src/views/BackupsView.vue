<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue'
import { api, ok, type Backup } from '../api/client'
import { backupKind, backupTone } from '../backups'
import DeleteBackupDialog from '../components/DeleteBackupDialog.vue'
import MachineTabs from '../components/MachineTabs.vue'
import RestoreBackupDialog from '../components/RestoreBackupDialog.vue'
import StatusLed from '../components/StatusLed.vue'
import { ago, gib } from '../state'
import { useLiveRefresh } from '../stores/live'
import { useScope } from '../stores/scope'

const scope = useScope()
const backups = ref<Backup[] | null>(null)
const vmIds = ref(new Set<string>())

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
const restoring = ref<Backup | null>(null)
const deleting = ref<Backup | null>(null)
</script>

<template>
  <section>
    <MachineTabs />
    <header class="page-head">
      <h1>Backups</h1>
    </header>
    <p class="intro">
      Copies of VMs’ disks, kept apart from the storage they run on. A backup outlives its VM: restore it as a new VM
      from here. An incremental backup holds only what changed since the one it builds on, yet restores the whole VM.
      Back VMs up, or set their schedule, on each VM’s page.
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
              <span v-if="b.state !== 'failed'" class="tag" :title="backupKind(b, backups).title">{{
                backupKind(b, backups).label
              }}</span>
              <span v-if="b.scheduled" class="tag">Scheduled</span>
            </td>
            <td class="num mono">{{ b.stored_bytes != null ? gib(b.stored_bytes) : '—' }}</td>
            <td>
              <StatusLed :tone="backupTone(b)" />
              {{
                b.state === 'ready'
                  ? b.consistent
                    ? 'Consistent'
                    : 'Crash-consistent'
                  : b.state === 'running'
                    ? 'Taking'
                    : b.state === 'deleting'
                      ? 'Deleting'
                      : 'Failed'
              }}
              <div v-if="b.error" class="error-text">{{ b.error }}</div>
            </td>
            <td class="end">
              <button v-if="b.state === 'ready' && canWrite(b)" class="btn small" @click="restoring = b">
                Restore…
              </button>
              <button
                v-if="b.state !== 'running' && (b.state !== 'deleting' || b.error) && canWrite(b)"
                class="btn small danger"
                @click="deleting = b"
              >
                Delete…
              </button>
            </td>
          </tr>
        </tbody>
      </table>
    </div>
    <RestoreBackupDialog v-if="restoring" :backup="restoring" @close="restoring = null" />
    <DeleteBackupDialog v-if="deleting && backups" :backup="deleting" :backups="backups" @close="deleting = null" />
  </section>
</template>

<style scoped>
.end .btn + .btn {
  margin-left: 6px;
}
</style>
