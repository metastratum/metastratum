<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue'
import { useRouter } from 'vue-router'
import { api, ok, type Vm } from '../api/client'
import CreateVmDialog from '../components/CreateVmDialog.vue'
import ListPager from '../components/ListPager.vue'
import SegmentedFilter from '../components/SegmentedFilter.vue'
import StatusLed from '../components/StatusLed.vue'
import { usePaging } from '../paging'
import { ago, mib, vmState } from '../state'
import { useLiveRefresh } from '../stores/live'
import { useScope } from '../stores/scope'
import { useSession } from '../stores/session'
import { useShell } from '../stores/shell'
import { useTasks } from '../stores/tasks'

const router = useRouter()
const tasks = useTasks()
const scope = useScope()
const session = useSession()
const shell = useShell()
const vms = ref<Vm[] | null>(null)
const creating = ref(false)

async function load() {
  if (!scope.current) return
  const tenant = scope.current
  const list = await ok(api.GET('/api/v1/vms', { params: { query: { tenant_id: tenant, descendants: true } } }))
  if (tenant === scope.current) {
    vms.value = list
    shell.vmCount = list.length
  }
}
watch(
  () => scope.current,
  () => {
    vms.value = null
    void load()
  },
)
// Name the tenant when the list spans sub-tenants.
const mixed = computed(() => vms.value?.some((v) => v.tenant_id !== scope.current) ?? false)

onMounted(load)
// A host going away changes how its VMs are shown.
useLiveRefresh(['vm', 'host'], load)

// What the host last said: running, off, or asleep (paused and the rest
// show under All only).
type Power = 'running' | 'stopped' | 'asleep'
function power(vm: Vm): Power | null {
  if (vm.power === 'running') return 'running'
  if (vm.power === 'suspended') return 'asleep'
  if (vm.power === 'stopped' || vm.power === 'crashed') return 'stopped'
  return null
}
const filter = ref<'all' | Power>('all')
const name = ref('')
const filters = computed(() => {
  const all = vms.value ?? []
  const count = (p: Power) => all.filter((v) => power(v) === p).length
  return [
    { id: 'all', label: 'All', count: all.length },
    { id: 'running', label: 'Running', count: count('running') },
    { id: 'stopped', label: 'Stopped', count: count('stopped') },
    { id: 'asleep', label: 'Asleep', count: count('asleep') },
  ]
})
const matching = computed(() => {
  const q = name.value.trim().toLowerCase()
  return (vms.value ?? []).filter(
    (v) => (filter.value === 'all' || power(v) === filter.value) && (!q || v.name.toLowerCase().includes(q)),
  )
})
const { page, perPage, shown } = usePaging(matching)

function created(vm: Vm) {
  creating.value = false
  tasks.poke()
  void router.push({ name: 'vm', params: { id: vm.id } })
}
</script>

<template>
  <section>
    <header class="page-head">
      <h1>Virtual machines</h1>
      <span v-if="vms" class="mono muted">{{ vms.length }}</span>
      <div class="actions">
        <button v-if="scope.can('vm.write')" class="btn primary" @click="creating = true">Create VM</button>
      </div>
    </header>

    <div v-if="vms && !vms.length" class="empty-state">
      <p>No virtual machines{{ scope.solo ? '' : ` in ${scope.tenant?.name ?? 'this tenant'}` }} yet.<template v-if="scope.can('vm.write')"> Create one from an image to get started.</template></p>
      <button v-if="scope.can('vm.write')" class="btn primary" @click="creating = true">Create VM</button>
    </div>

    <template v-else-if="vms">
      <div class="toolbar">
        <SegmentedFilter v-model="filter" :options="filters" label="Filter by state" />
        <label class="search">
          <span class="label">Name</span>
          <input v-model="name" type="search" placeholder="Filter machines" />
        </label>
      </div>
      <div class="table-wrap">
        <table class="data">
          <thead>
            <tr>
              <th>Name</th>
              <th>State</th>
              <th v-if="mixed">Tenant</th>
              <th>Address</th>
              <th>Host</th>
              <th class="num">vCPU</th>
              <th class="num">Memory</th>
              <th>Created</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="vm in shown" :key="vm.id" @click="router.push({ name: 'vm', params: { id: vm.id } })">
              <td>
                <RouterLink :to="{ name: 'vm', params: { id: vm.id } }" class="name" @click.stop>{{ vm.name }}</RouterLink>
                <span v-if="vm.template" class="tag">Template</span>
              </td>
              <td class="nowrap"><StatusLed :tone="vmState(vm).tone" :label="vmState(vm).text" /></td>
              <td v-if="mixed" class="muted">{{ scope.byId.get(vm.tenant_id)?.name ?? '—' }}</td>
              <td class="mono">{{ vm.nics[0]?.ip ?? '—' }}</td>
              <td class="mono">
                <RouterLink v-if="vm.host_name && session.me?.provider_admin" to="/hosts" @click.stop>{{ vm.host_name }}</RouterLink>
                <template v-else>{{ vm.host_name ?? '—' }}</template>
              </td>
              <td class="num mono">{{ vm.vcpus }}</td>
              <td class="num mono nowrap">{{ mib(vm.memory_mib) }}</td>
              <td class="muted">{{ ago(vm.created_at) }}</td>
            </tr>
          </tbody>
        </table>
        <p v-if="!matching.length" class="none muted">No machines match.</p>
      </div>
      <ListPager v-model:page="page" v-model:per-page="perPage" :total="matching.length" class="pager" />
    </template>

    <CreateVmDialog v-if="creating" @close="creating = false" @created="created" />
  </section>
</template>

<style scoped>
.page-head {
  margin-bottom: 18px;
}
.name {
  font-weight: 600;
  text-decoration: none;
}
.name + .tag {
  margin-left: 8px;
}
tbody tr {
  cursor: pointer;
}
tbody tr:hover td {
  background: var(--sunken);
}
.nowrap {
  white-space: nowrap;
}
.none {
  margin: 0;
  padding: 28px;
}
</style>
