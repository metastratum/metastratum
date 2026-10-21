<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { api, errorText, ok, type FloatingIp, type Schemas, type Vm } from '../api/client'
import ModalDialog from '../components/ModalDialog.vue'
import ListPager from '../components/ListPager.vue'
import { usePaging } from '../paging'
import { useLiveRefresh } from '../stores/live'
import { useScope } from '../stores/scope'
import { useSession } from '../stores/session'

const scope = useScope()
const session = useSession()
const ips = ref<FloatingIp[] | null>(null)
const vms = ref<Vm[]>([])
const externals = ref<Schemas['UsableExternalNetwork'][]>([])
const error = ref('')

async function load() {
  if (!scope.current) return
  const tenant = scope.current
  try {
    const [f, v, e] = await Promise.all([
      ok(api.GET('/api/v1/floating-ips', { params: { query: { tenant_id: tenant } } })),
      ok(api.GET('/api/v1/vms', { params: { query: { tenant_id: tenant } } })),
      ok(api.GET('/api/v1/tenants/{id}/external-networks', { params: { path: { id: tenant } } })),
    ])
    if (tenant === scope.current) [ips.value, vms.value, externals.value] = [f, v, e]
  } catch (e) {
    error.value = errorText(e)
  }
}
watch(() => scope.current, load, { immediate: true })
useLiveRefresh(['external_ip', 'vm'], load)

const canWrite = computed(() => scope.can('network.write'))

/** Every NIC of the tenant's VMs, labelled for a picker. */
const nics = computed(() =>
  vms.value.flatMap((vm) =>
    vm.nics.map((n) => ({ id: n.id, label: `${vm.name} (${n.ip}${n.network_name ? ` on ${n.network_name}` : ''})` })),
  ),
)

/** The address a NIC has on its network: where its floating IP forwards. */
const nicIp = computed(() => new Map(vms.value.flatMap((vm) => vm.nics.map((n) => [n.id, n.ip] as const))))

const { page, perPage, shown } = usePaging(computed(() => ips.value ?? []))

const dialog = ref<{ kind: 'allocate' } | { kind: 'attach'; ip: FloatingIp } | null>(null)
const form = ref({ external: '', nic: '' as string | null })
const formError = ref('')
const busy = ref(false)

function open(d: NonNullable<typeof dialog.value>) {
  formError.value = ''
  form.value = { external: externals.value[0]?.id ?? '', nic: d.kind === 'attach' ? (d.ip.nic_id ?? null) : null }
  dialog.value = d
}

async function submit() {
  const d = dialog.value
  if (!d) return
  formError.value = ''
  busy.value = true
  try {
    if (d.kind === 'allocate') {
      await ok(
        api.POST('/api/v1/floating-ips', {
          body: { tenant_id: scope.current, external_network_id: form.value.external, nic_id: form.value.nic || null },
        }),
      )
    } else {
      await ok(api.PATCH('/api/v1/floating-ips/{id}', { params: { path: { id: d.ip.id } }, body: { nic_id: form.value.nic || null } }))
    }
    dialog.value = null
    await load()
  } catch (e) {
    formError.value = errorText(e)
  } finally {
    busy.value = false
  }
}

async function release(ip: FloatingIp) {
  error.value = ''
  try {
    await ok(api.DELETE('/api/v1/floating-ips/{id}', { params: { path: { id: ip.id } } }))
    await load()
  } catch (e) {
    error.value = errorText(e)
  }
}
</script>

<template>
  <section>
    <header class="page-head">
      <h1>Floating IPs</h1>
      <span v-if="ips" class="mono muted">{{ ips.length }}</span>
      <div v-if="canWrite" class="actions">
        <button
          class="btn primary"
          :disabled="!externals.length"
          :title="externals.length ? undefined : 'No external network is available'"
          @click="open({ kind: 'allocate' })"
        >
          Allocate address
        </button>
      </div>
    </header>
    <p class="intro">
      An external address that forwards to a VM. Its VPC must route to that network; its security groups decide what
      gets through.
    </p>
    <p v-if="error" class="error-text" role="alert">{{ error }}</p>

    <div v-if="ips && !ips.length" class="empty-state">
      <p v-if="externals.length">No floating IPs yet.</p>
      <p v-else-if="scope.solo">There is no external network yet: create one under Infrastructure → External networks.</p>
      <p v-else>No external network is available to {{ scope.tenant?.name }}. Ask an administrator of the tenant above it.</p>
    </div>
    <template v-else-if="ips">
      <div class="table-wrap">
        <table class="data">
          <thead>
            <tr>
              <th>Address</th>
              <th>Network</th>
              <th>Forwards to</th>
              <th />
            </tr>
          </thead>
          <tbody>
            <tr v-for="ip in shown" :key="ip.id">
              <td class="mono strong">{{ ip.ip }}</td>
              <td>
                <RouterLink v-if="session.me?.provider_admin" to="/external-networks">{{ ip.external_network_name }}</RouterLink>
                <template v-else>{{ ip.external_network_name }}</template>
              </td>
              <td>
                <template v-if="ip.vm_id">
                  <RouterLink :to="{ name: 'vm', params: { id: ip.vm_id } }">{{ ip.vm_name }}</RouterLink>
                  <span v-if="ip.nic_id && nicIp.get(ip.nic_id)" class="mono muted target">{{ nicIp.get(ip.nic_id) }}</span>
                </template>
                <span v-else class="muted">Nothing</span>
              </td>
              <td class="end">
                <template v-if="canWrite">
                  <button class="btn small" @click="open({ kind: 'attach', ip })">{{ ip.vm_id ? 'Move' : 'Attach' }}</button>
                  <button class="btn small danger" @click="release(ip)">Release</button>
                </template>
              </td>
            </tr>
          </tbody>
        </table>
      </div>
      <ListPager v-model:page="page" v-model:per-page="perPage" :total="ips.length" class="pager" />
    </template>

    <ModalDialog v-if="dialog" :title="dialog.kind === 'allocate' ? 'Allocate a floating IP' : `Forward ${dialog.ip.ip}`" @close="dialog = null">
      <form class="form" @submit.prevent="submit">
        <label v-if="dialog.kind === 'allocate'" class="field">
          <span class="label">External network</span>
          <select v-model="form.external" required>
            <option v-for="e in externals" :key="e.id" :value="e.id">{{ e.name }} ({{ e.cidr }})</option>
          </select>
        </label>
        <label class="field">
          <span class="label">Forward to</span>
          <select v-model="form.nic">
            <option :value="null">Nothing for now</option>
            <option v-for="n in nics" :key="n.id" :value="n.id">{{ n.label }}</option>
          </select>
        </label>
        <p v-if="formError" class="error-text" role="alert">{{ formError }}</p>
        <footer>
          <button type="button" class="btn" @click="dialog = null">Cancel</button>
          <button class="btn primary" :disabled="busy">{{ dialog.kind === 'allocate' ? 'Allocate' : 'Save' }}</button>
        </footer>
      </form>
    </ModalDialog>
  </section>
</template>

<style scoped>
.page-head {
  margin-bottom: 4px;
}
.strong {
  font-weight: 600;
}
.target {
  margin-left: 8px;
}
.end .btn + .btn {
  margin-left: 6px;
}
</style>
