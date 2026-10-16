<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue'
import { useRouter } from 'vue-router'
import { api, ok, type Vm } from '../api/client'
import CreateVmDialog from '../components/CreateVmDialog.vue'
import StatusLed from '../components/StatusLed.vue'
import { ago, mib, vmState } from '../state'
import { useLiveRefresh } from '../stores/live'
import { useScope } from '../stores/scope'
import { useTasks } from '../stores/tasks'

const router = useRouter()
const tasks = useTasks()
const scope = useScope()
const vms = ref<Vm[] | null>(null)
const creating = ref(false)

async function load() {
  if (!scope.current) return
  const tenant = scope.current
  const list = await ok(api.GET('/api/v1/vms', { params: { query: { tenant_id: tenant, descendants: true } } }))
  if (tenant === scope.current) vms.value = list
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

function created(vm: Vm) {
  creating.value = false
  tasks.poke()
  void router.push({ name: 'vm', params: { id: vm.id } })
}
</script>

<template>
  <section>
    <header class="head">
      <h1>Virtual machines</h1>
      <span v-if="vms" class="count mono">{{ vms.length }}</span>
      <button v-if="scope.can('vm.write')" class="btn primary" @click="creating = true">Create VM</button>
    </header>

    <div v-if="vms && !vms.length" class="empty">
      <p>No virtual machines in {{ scope.tenant?.name ?? 'this tenant' }} yet.<template v-if="scope.can('vm.write')"> Create one from an image to get started.</template></p>
      <button v-if="scope.can('vm.write')" class="btn primary" @click="creating = true">Create VM</button>
    </div>

    <div v-else-if="vms" class="table-wrap">
      <table>
        <thead>
          <tr>
            <th>Name</th>
            <th v-if="mixed">Tenant</th>
            <th>State</th>
            <th>Address</th>
            <th>Host</th>
            <th class="num">vCPU</th>
            <th class="num">Memory</th>
            <th>Created</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="vm in vms" :key="vm.id" @click="router.push({ name: 'vm', params: { id: vm.id } })">
            <td>
              <RouterLink :to="{ name: 'vm', params: { id: vm.id } }" class="name" @click.stop>{{ vm.name }}</RouterLink>
            </td>
            <td v-if="mixed" class="muted">{{ scope.byId.get(vm.tenant_id)?.name ?? '—' }}</td>
            <td><StatusLed :tone="vmState(vm).tone" :label="vmState(vm).text" /></td>
            <td class="mono">{{ vm.nics[0]?.ip ?? '—' }}</td>
            <td class="mono">{{ vm.host_name ?? '—' }}</td>
            <td class="num mono">{{ vm.vcpus }}</td>
            <td class="num mono">{{ mib(vm.memory_mib) }}</td>
            <td class="muted">{{ ago(vm.created_at) }}</td>
          </tr>
        </tbody>
      </table>
    </div>

    <CreateVmDialog v-if="creating" @close="creating = false" @created="created" />
  </section>
</template>

<style scoped>
.head {
  display: flex;
  align-items: baseline;
  gap: 12px;
  margin-bottom: 20px;
}
h1 {
  font-size: 22px;
}
.count {
  color: var(--muted);
}
.head .btn {
  margin-left: auto;
}
.table-wrap {
  overflow-x: auto;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
  background: var(--panel);
}
table {
  width: 100%;
  border-collapse: collapse;
}
th {
  padding: 10px 14px;
  border-bottom: 1px solid var(--rule);
  text-align: left;
  font: 500 11px var(--font-label);
  letter-spacing: 0.12em;
  text-transform: uppercase;
  color: var(--muted);
  white-space: nowrap;
}
td {
  padding: 11px 14px;
  border-bottom: 1px solid var(--rule);
  white-space: nowrap;
}
tbody tr {
  cursor: pointer;
}
tbody tr:last-child td {
  border-bottom: 0;
}
tbody tr:hover {
  background: var(--sunken);
}
.name {
  font-weight: 600;
  text-decoration: none;
}
.num {
  text-align: right;
}
.muted {
  color: var(--muted);
}
.empty {
  display: grid;
  gap: 14px;
  justify-items: start;
  padding: 32px;
  border: 1px dashed var(--rule);
  border-radius: var(--radius);
}
.empty p {
  margin: 0;
  color: var(--muted);
}
</style>
