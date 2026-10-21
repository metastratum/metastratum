<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { api, errorText, ok, type LoadBalancer, type Vm, type Vpc } from '../api/client'
import ModalDialog from '../components/ModalDialog.vue'
import { useLiveRefresh } from '../stores/live'
import { useScope } from '../stores/scope'

const scope = useScope()
const lbs = ref<LoadBalancer[] | null>(null)
const vpcs = ref<Vpc[]>([])
const vms = ref<Vm[]>([])
const error = ref('')

async function load() {
  if (!scope.current) return
  const tenant = scope.current
  try {
    const query = { params: { query: { tenant_id: tenant } } }
    const [l, v, m] = await Promise.all([
      ok(api.GET('/api/v1/load-balancers', query)),
      ok(api.GET('/api/v1/vpcs', query)),
      ok(api.GET('/api/v1/vms', query)),
    ])
    if (tenant === scope.current) [lbs.value, vpcs.value, vms.value] = [l, v, m]
  } catch (e) {
    error.value = errorText(e)
  }
}
watch(() => scope.current, load, { immediate: true })
useLiveRefresh(['load_balancer', 'vpc', 'network', 'vm'], load)

const canWrite = computed(() => scope.can('network.write'))
const networkName = (id: string | null | undefined) =>
  vpcs.value.flatMap((v) => v.networks).find((n) => n.id === id)?.name ?? 'a network'

type ListenerRow = { protocol: string; port: number | null; target_port: number | null }
const editing = ref<LoadBalancer | 'new' | null>(null)
const form = ref({
  name: '',
  vpc: '',
  network: null as string | null,
  health: true,
  listeners: [] as ListenerRow[],
  members: [] as string[],
})
const formError = ref('')
const busy = ref(false)

const vpc = computed(() => vpcs.value.find((v) => v.id === form.value.vpc))
/** VMs with a NIC in the chosen VPC: the possible members. */
const candidates = computed(() => {
  const nets = new Set(vpc.value?.networks.map((n) => n.id) ?? [])
  return vms.value.filter((vm) => !vm.template && vm.nics.some((n) => nets.has(n.network_id)))
})

function open(lb: LoadBalancer | 'new') {
  formError.value = ''
  if (lb === 'new') {
    const first = vpcs.value.find((v) => v.external_network_id) ?? vpcs.value[0]
    form.value = {
      name: '',
      vpc: first?.id ?? '',
      network: first?.external_network_id ? null : (first?.networks[0]?.id ?? null),
      health: true,
      listeners: [{ protocol: 'tcp', port: 80, target_port: null }],
      members: [],
    }
  } else {
    form.value = {
      name: lb.name,
      vpc: lb.vpc_id,
      network: lb.network_id ?? null,
      health: lb.health_check,
      listeners: lb.listeners.map((l) => ({
        protocol: l.protocol,
        port: l.port,
        target_port: l.target_port === l.port ? null : (l.target_port ?? null),
      })),
      members: lb.members.map((m) => m.vm_id),
    }
  }
  editing.value = lb
}

async function submit() {
  const e = editing.value
  if (!e) return
  formError.value = ''
  busy.value = true
  const f = form.value
  const listeners = f.listeners.map((l) => ({
    protocol: l.protocol,
    port: l.port ?? 0,
    target_port: l.target_port || null,
  }))
  try {
    if (e === 'new') {
      await ok(
        api.POST('/api/v1/load-balancers', {
          body: {
            tenant_id: scope.current,
            name: f.name,
            vpc_id: f.vpc,
            network_id: f.network,
            health_check: f.health,
            listeners,
            member_vm_ids: f.members,
          },
        }),
      )
    } else {
      await ok(
        api.PUT('/api/v1/load-balancers/{id}', {
          params: { path: { id: e.id } },
          body: { name: f.name, health_check: f.health, listeners, member_vm_ids: f.members },
        }),
      )
    }
    editing.value = null
    await load()
  } catch (err) {
    formError.value = errorText(err)
  } finally {
    busy.value = false
  }
}

const deleting = ref<LoadBalancer | null>(null)
async function remove() {
  const lb = deleting.value
  if (!lb) return
  error.value = ''
  try {
    await ok(api.DELETE('/api/v1/load-balancers/{id}', { params: { path: { id: lb.id } } }))
    deleting.value = null
    await load()
  } catch (e) {
    error.value = errorText(e)
    deleting.value = null
  }
}

const listenerText = (l: LoadBalancer['listeners'][number]) =>
  `${l.protocol.toUpperCase()} ${l.port}${l.target_port != null && l.target_port !== l.port ? ` → ${l.target_port}` : ''}`

/** A member's address in the VPC (its first NIC there). */
const memberIp = (vm: Vm) => {
  const nets = new Set(vpc.value?.networks.map((n) => n.id) ?? [])
  return vm.nics.find((n) => nets.has(n.network_id))?.ip
}
</script>

<template>
  <section>
    <header class="page-head">
      <h1>Load balancers</h1>
      <span v-if="lbs" class="mono muted">{{ lbs.length }}</span>
      <div v-if="canWrite" class="actions">
        <button
          class="btn primary"
          :disabled="!vpcs.length"
          :title="vpcs.length ? undefined : 'Create a VPC with a network first'"
          @click="open('new')"
        >
          Create load balancer
        </button>
      </div>
    </header>
    <p class="intro">
      Spreads connections to one address over several VMs, done by the network itself: on the VPC’s external network
      to be reached from outside, or in one of its networks for its own VMs. Members’ security groups decide who gets
      through, and connections keep the client’s address.
    </p>
    <p v-if="error" class="error-text" role="alert">{{ error }}</p>

    <div v-if="lbs && !lbs.length" class="empty-state">
      <p v-if="vpcs.length">No load balancers yet.</p>
      <template v-else>
        <p>Create a VPC with a network first.</p>
        <RouterLink to="/networks" class="btn">Go to VPCs</RouterLink>
      </template>
    </div>
    <div v-else-if="lbs" class="table-wrap">
      <table class="data">
        <thead>
          <tr>
            <th>Name</th>
            <th>Address</th>
            <th>Listeners</th>
            <th>Members</th>
            <th />
          </tr>
        </thead>
        <tbody>
          <tr v-for="lb in lbs" :key="lb.id">
            <td class="strong">{{ lb.name }}</td>
            <td>
              <div class="stack">
                <span class="mono">{{ lb.ip }}</span>
                <span class="muted small">
                  {{ lb.external ? 'External' : `In ${networkName(lb.network_id)}` }} · VPC {{ lb.vpc_name }}
                </span>
              </div>
            </td>
            <td>
              <div class="chips">
                <span v-for="(l, i) in lb.listeners" :key="i" class="chip mono">{{ listenerText(l) }}</span>
                <span v-if="!lb.listeners.length" class="muted">—</span>
              </div>
            </td>
            <td>
              <div class="members">
                <RouterLink v-for="m in lb.members" :key="m.nic_id" :to="{ name: 'vm', params: { id: m.vm_id } }">
                  {{ m.vm_name }}
                </RouterLink>
                <span v-if="!lb.members.length" class="muted">None</span>
                <span v-if="lb.health_check && lb.members.length" class="tag" title="Members that do not answer are left out">
                  Health-checked
                </span>
              </div>
            </td>
            <td class="end">
              <div v-if="canWrite" class="buttons">
                <button class="btn small" @click="open(lb)">Edit</button>
                <button class="btn small danger" @click="deleting = lb">Delete…</button>
              </div>
            </td>
          </tr>
        </tbody>
      </table>
    </div>

    <ModalDialog
      v-if="editing"
      :title="editing === 'new' ? 'Create a load balancer' : `Edit ${editing.name}`"
      @close="editing = null"
    >
      <form class="form" @submit.prevent="submit">
        <label class="field">
          <span class="label">Name</span>
          <input v-model="form.name" required maxlength="64" placeholder="web" />
        </label>
        <template v-if="editing === 'new'">
          <label class="field">
            <span class="label">VPC</span>
            <select v-model="form.vpc" required @change="((form.members = []), (form.network = vpc?.external_network_id ? null : (vpc?.networks[0]?.id ?? null)))">
              <option v-for="v in vpcs" :key="v.id" :value="v.id">{{ v.name }}</option>
            </select>
          </label>
          <label class="field">
            <span class="label">Address</span>
            <select v-model="form.network">
              <option :value="null" :disabled="!vpc?.external_network_id">
                On the external network{{ vpc?.external_network_id ? '' : ' (the VPC has none)' }}
              </option>
              <option v-for="n in vpc?.networks ?? []" :key="n.id" :value="n.id">In {{ n.name }} ({{ n.cidr }})</option>
            </select>
          </label>
        </template>
        <fieldset class="field">
          <legend class="label">Listeners</legend>
          <div v-for="(l, i) in form.listeners" :key="i" class="listener">
            <select v-model="l.protocol" aria-label="Protocol">
              <option value="tcp">TCP</option>
              <option value="udp">UDP</option>
            </select>
            <input v-model.number="l.port" type="number" min="1" max="65535" required aria-label="Port" placeholder="Port" />
            <span class="muted">to members’ port</span>
            <input
              v-model.number="l.target_port"
              type="number"
              min="1"
              max="65535"
              aria-label="Members' port"
              :placeholder="l.port ? String(l.port) : 'Same'"
            />
            <button type="button" class="btn small" @click="form.listeners.splice(i, 1)">Remove</button>
          </div>
          <button
            type="button"
            class="btn small"
            @click="form.listeners.push({ protocol: 'tcp', port: null, target_port: null })"
          >
            Add listener
          </button>
        </fieldset>
        <fieldset class="field">
          <legend class="label">Members</legend>
          <label v-for="vm in candidates" :key="vm.id" class="check">
            <input v-model="form.members" type="checkbox" :value="vm.id" />
            <span>{{ vm.name }}</span>
            <span v-if="memberIp(vm)" class="mono muted">{{ memberIp(vm) }}</span>
          </label>
          <span v-if="!candidates.length" class="hint">No VMs in this VPC yet.</span>
        </fieldset>
        <label class="check">
          <input v-model="form.health" type="checkbox" />
          Leave out members that fail health checks
        </label>
        <p v-if="formError" class="error-text" role="alert">{{ formError }}</p>
        <footer>
          <button type="button" class="btn" @click="editing = null">Cancel</button>
          <button class="btn primary" :disabled="busy">{{ editing === 'new' ? 'Create' : 'Save' }}</button>
        </footer>
      </form>
    </ModalDialog>

    <ModalDialog v-if="deleting" title="Delete load balancer" @close="deleting = null">
      <form class="form" @submit.prevent="remove">
        <p>
          Delete <strong>{{ deleting.name }}</strong>? Its address {{ deleting.ip }} is given back, and connections to it
          stop. The member VMs stay.
        </p>
        <footer>
          <button type="button" class="btn" @click="deleting = null">Cancel</button>
          <button class="btn danger">Delete load balancer</button>
        </footer>
      </form>
    </ModalDialog>
  </section>
</template>

<style scoped>
.page-head {
  margin-bottom: 4px;
}
.data td {
  padding-block: 12px;
}
.strong {
  font-weight: 600;
}
.stack {
  display: grid;
  gap: 2px;
}
.small {
  font-size: 12px;
}
.chips {
  display: flex;
  flex-wrap: wrap;
  gap: 4px;
}
.chip {
  padding: 1px 6px;
  border-radius: var(--radius);
  background: var(--sunken);
  font-size: 11px;
}
.members {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 4px 12px;
}
.buttons {
  display: inline-flex;
  gap: 6px;
}
fieldset.field {
  margin: 0;
  padding: 0;
  border: 0;
  align-content: start;
}
fieldset.field legend {
  margin-bottom: 4px;
}
fieldset.field > .btn {
  justify-self: start;
}
.listener {
  display: grid;
  grid-template-columns: 80px 1fr auto 1fr auto;
  align-items: center;
  gap: 6px;
  margin-bottom: 6px;
}
</style>
