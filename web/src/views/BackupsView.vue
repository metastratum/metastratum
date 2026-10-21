<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue'
import { api, ok, type Backup } from '../api/client'
import { backupKind, backupTone } from '../backups'
import DeleteBackupDialog from '../components/DeleteBackupDialog.vue'
import ListPager from '../components/ListPager.vue'
import RestoreBackupDialog from '../components/RestoreBackupDialog.vue'
import StatusLed from '../components/StatusLed.vue'
import { usePaging } from '../paging'
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

// One VM's backups (deleted VMs' too, by name), or all.
const vm = ref('')
const vmNames = computed(() => [...new Set((backups.value ?? []).map((b) => b.vm_name))].sort())
const matching = computed(() => (backups.value ?? []).filter((b) => !vm.value || b.vm_name === vm.value))
const { page, perPage, shown } = usePaging(matching)

function stateLabel(b: Backup): string {
  if (b.state === 'ready') return b.consistent ? 'Consistent' : 'Crash-consistent'
  return b.state === 'running' ? 'Taking' : b.state === 'deleting' ? 'Deleting' : 'Failed'
}
function stateTitle(b: Backup): string | undefined {
  if (b.state !== 'ready') return undefined
  return b.consistent
    ? 'The VM was stopped, or its guest agent froze its file systems'
    : 'Taken without a guest agent: as after a crash, which journaling file systems recover from'
}
const deleting = ref<Backup | null>(null)
</script>

<template>
  <section>
    <header class="page-head">
      <h1>Backups</h1>
      <span v-if="backups" class="mono muted">{{ backups.length }}</span>
    </header>
    <p class="intro">
      A backup outlives its VM: restore it as a new VM from here. Back VMs up, or set their schedule, on each VM’s page.
    </p>

    <div v-if="backups && !backups.length" class="empty-state">
      <p>No backups{{ scope.solo ? '' : ` in ${scope.tenant?.name ?? 'this tenant'}` }} yet.</p>
    </div>
    <template v-else-if="backups">
      <div class="toolbar">
        <label class="vm">
          <span class="label">VM</span>
          <select v-model="vm">
            <option value="">All VMs</option>
            <option v-for="n in vmNames" :key="n" :value="n">{{ n }}</option>
          </select>
        </label>
      </div>
      <div class="table-wrap">
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
            <tr v-for="b in shown" :key="b.id">
              <td class="nowrap">
                <RouterLink v-if="b.vm_id && vmIds.has(b.vm_id)" :to="{ name: 'vm', params: { id: b.vm_id } }" class="name">
                  {{ b.vm_name }}
                </RouterLink>
                <template v-else><span class="name">{{ b.vm_name }}</span> <span class="muted">(deleted)</span></template>
              </td>
              <td v-if="mixed" class="muted">{{ scope.byId.get(b.tenant_id)?.name ?? '—' }}</td>
              <td>
                <div class="taken">
                  <span :title="new Date(b.created_at).toLocaleString()" class="nowrap">{{ ago(b.created_at) }}</span>
                  <span v-if="b.state !== 'failed'" class="tag" :title="backupKind(b, backups).title">{{
                    backupKind(b, backups).label
                  }}</span>
                  <span v-if="b.scheduled" class="tag">Scheduled</span>
                </div>
              </td>
              <td class="num mono nowrap">{{ b.stored_bytes != null ? gib(b.stored_bytes) : '—' }}</td>
              <td>
                <span :title="stateTitle(b)"><StatusLed :tone="backupTone(b)" :label="stateLabel(b)" /></span>
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
        <p v-if="!matching.length" class="none muted">No backups match.</p>
      </div>
      <ListPager v-model:page="page" v-model:per-page="perPage" :total="matching.length" class="pager" />
    </template>
    <RestoreBackupDialog v-if="restoring" :backup="restoring" @close="restoring = null" />
    <DeleteBackupDialog v-if="deleting && backups" :backup="deleting" :backups="backups" @close="deleting = null" />
  </section>
</template>

<style scoped>
.vm {
  display: flex;
  align-items: center;
  gap: 8px;
}
.vm select {
  min-width: 180px;
  height: 32px;
  padding: 0 8px;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
  background: var(--panel);
  color: var(--ink);
  font: 400 14px var(--font-body);
}
.name {
  font-weight: 600;
  text-decoration: none;
}
.taken {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 4px 8px;
}
.nowrap {
  white-space: nowrap;
}
.none {
  margin: 0;
  padding: 28px;
}
.error-text {
  margin-top: 2px;
  font-size: 12px;
}
.end {
  white-space: nowrap;
}
.end .btn + .btn {
  margin-left: 6px;
}
</style>
