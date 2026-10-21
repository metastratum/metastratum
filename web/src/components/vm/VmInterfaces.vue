<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { api, errorText, ok, type SecurityGroup, type Vm, type Vpc } from '../../api/client'
import { shortId } from '../../state'
import IdChip from '../IdChip.vue'

/** A VM's network interfaces: adding, moving and removing them, and their security groups. */
const props = defineProps<{ vm: Vm }>()
const emit = defineEmits<{ update: [vm: Vm] }>()

// Security groups of the VM's tenant, to name and change an interface's
// groups; its VPCs and their networks, to add interfaces and move them.
const groups = ref<SecurityGroup[]>([])
const vpcs = ref<Vpc[]>([])
watch(
  () => props.vm.tenant_id,
  async (tenant) => {
    groups.value = await ok(api.GET('/api/v1/security-groups', { params: { query: { tenant_id: tenant } } })).catch(
      () => [],
    )
    vpcs.value = await ok(api.GET('/api/v1/vpcs', { params: { query: { tenant_id: tenant } } })).catch(() => [])
  },
  { immediate: true },
)
const withNetworks = computed(() => vpcs.value.filter((v) => v.networks.length))
const groupName = (id: string) => groups.value.find((g) => g.id === id)?.name ?? shortId(id)
const ready = computed(() => props.vm.status === 'ready')

type Nic = Vm['nics'][number]
// Adding an interface, moving one to another network, removing one.
const action = ref<{ kind: 'add' | 'move' | 'remove'; nic?: Nic } | null>(null)
const network = ref('')
const busy = ref(false)
const error = ref('')
function start(kind: 'add' | 'move' | 'remove', n?: Nic) {
  error.value = ''
  network.value = n?.network_id ?? withNetworks.value[0]?.networks[0]?.id ?? ''
  action.value = { kind, nic: n }
}
async function run() {
  const a = action.value
  if (!a) return
  busy.value = true
  error.value = ''
  try {
    const path = { id: props.vm.id, nic: a.nic?.id ?? '' }
    emit(
      'update',
      await ok(
        a.kind === 'add'
          ? api.POST('/api/v1/vms/{id}/nics', { params: { path: { id: props.vm.id } }, body: { network_id: network.value } })
          : a.kind === 'move'
            ? api.PATCH('/api/v1/vms/{id}/nics/{nic}', { params: { path }, body: { network_id: network.value } })
            : api.DELETE('/api/v1/vms/{id}/nics/{nic}', { params: { path } }),
      ),
    )
    action.value = null
  } catch (e) {
    error.value = errorText(e)
  } finally {
    busy.value = false
  }
}
const live = computed(() => props.vm.power === 'running')
const title = computed(() => {
  const a = action.value
  if (!a) return ''
  if (a.kind === 'add') return 'Add an interface on'
  const name = a.nic?.network_name ?? a.nic?.ip
  return a.kind === 'move' ? `Move ${name} to` : `Remove the interface with ${a.nic?.ip}?`
})

// Changing an interface's security groups.
const editing = ref<string | null>(null)
const chosen = ref<string[]>([])
const groupError = ref('')
function edit(n: Nic) {
  groupError.value = ''
  chosen.value = [...n.security_group_ids]
  editing.value = n.id
}
async function save(nic: string) {
  groupError.value = ''
  try {
    emit(
      'update',
      await ok(
        api.PUT('/api/v1/vms/{id}/nics/{nic}/security-groups', {
          params: { path: { id: props.vm.id, nic } },
          body: { security_group_ids: chosen.value },
        }),
      ),
    )
    editing.value = null
  } catch (e) {
    groupError.value = errorText(e)
  }
}
</script>

<template>
  <section class="vm-card">
    <div class="vm-card-head">
      <h2>Network interfaces</h2>
      <button
        v-if="withNetworks.length && action?.kind !== 'add'"
        type="button"
        class="btn small"
        :disabled="!ready"
        @click="start('add')"
      >
        Add interface
      </button>
    </div>

    <form v-if="action" class="nic-form" @submit.prevent="run">
      <span class="what">{{ title }}</span>
      <template v-if="action.kind !== 'remove'">
        <select v-model="network" aria-label="Network" required>
          <optgroup v-for="v in withNetworks" :key="v.id" :label="`VPC ${v.name}`">
            <option v-for="net in v.networks" :key="net.id" :value="net.id">{{ net.name }} ({{ net.cidr }})</option>
          </optgroup>
        </select>
        <button type="button" class="btn small" @click="action = null">Cancel</button>
        <button
          class="btn small primary"
          :disabled="busy || (action.kind === 'move' && network === action.nic?.network_id)"
        >
          {{ action.kind === 'add' ? (busy ? 'Adding…' : 'Add') : busy ? 'Moving…' : 'Move' }}
        </button>
        <span class="hint">
          <template v-if="action.kind === 'add'">
            {{ live ? 'Plugged in at once' : 'There from its next start' }}, in the tenant’s default security group.
            Guests set it up with DHCP.
          </template>
          <template v-else>
            It gets an address there.
            <template v-if="live">
              It is unplugged and plugged in again, and the guest asks DHCP for the new address.
            </template>
          </template>
        </span>
      </template>
      <template v-else>
        <button type="button" class="btn small" @click="action = null">Keep it</button>
        <button class="btn small danger" :disabled="busy">{{ busy ? 'Removing…' : 'Remove' }}</button>
        <span class="hint">
          <template v-if="action.nic?.floating_ip">
            Its floating IP {{ action.nic.floating_ip }} is kept, attached to nothing.
          </template>
          It leaves any load balancers.
        </span>
      </template>
      <span v-if="error" class="error-text hint" role="alert">{{ error }}</span>
    </form>

    <div v-for="n in vm.nics" :key="n.id" class="nic">
      <div class="nic-head">
        <RouterLink to="/networks" class="net">{{ n.network_name ?? shortId(n.network_id) }}</RouterLink>
        <span class="mono">{{ n.ip }}</span>
        <span v-if="ready" class="buttons">
          <button v-if="withNetworks.length" type="button" class="btn small" @click="start('move', n)">Move…</button>
          <button v-if="vm.nics.length > 1" type="button" class="btn small danger" @click="start('remove', n)">
            Remove…
          </button>
        </span>
      </div>
      <dl class="kv">
        <dt>Floating IP</dt>
        <dd class="mono">
          <RouterLink v-if="n.floating_ip" to="/floating-ips">{{ n.floating_ip }}</RouterLink>
          <span v-else class="muted">None</span>
        </dd>
        <dt>Security groups</dt>
        <dd>
          <div v-if="editing !== n.id" class="groups">
            <span v-for="g in n.security_group_ids" :key="g" class="tag">{{ groupName(g) }}</span>
            <span v-if="!n.security_group_ids.length" class="error-text">None: no traffic passes</span>
            <button v-if="groups.length" type="button" class="btn small" @click="edit(n)">Change</button>
          </div>
          <form v-else class="groups" @submit.prevent="save(n.id)">
            <label v-for="g in groups" :key="g.id" class="check">
              <input v-model="chosen" type="checkbox" :value="g.id" />
              {{ g.name }}
            </label>
            <button type="button" class="btn small" @click="editing = null">Cancel</button>
            <button class="btn small primary">Save</button>
            <span v-if="!chosen.length" class="hint error-text">With none, no traffic passes.</span>
            <span v-if="groupError" class="hint error-text" role="alert">{{ groupError }}</span>
          </form>
        </dd>
        <dt>MAC</dt>
        <dd class="mono muted">{{ n.mac }}</dd>
        <dt>Interface</dt>
        <dd><IdChip :value="n.id" short /></dd>
      </dl>
    </div>
  </section>
</template>

<style scoped>
.nic-form {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px 10px;
  padding: 12px;
  border-radius: var(--radius);
  background: var(--aisle);
}
.nic-form .what {
  font-weight: 500;
}
.nic-form select {
  height: 32px;
  padding: 0 8px;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
  background: var(--panel);
  color: var(--ink);
  font: 400 14px var(--font-body);
}
.hint {
  flex-basis: 100%;
  font-size: 12px;
  color: var(--muted);
}
.hint.error-text {
  color: var(--led-fault);
}
.nic {
  display: grid;
  gap: 10px;
  padding: 12px;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
}
.nic-head {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px 12px;
}
.net {
  font-weight: 500;
}
.buttons {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
  margin-left: auto;
}
.kv {
  display: grid;
  grid-template-columns: 120px 1fr;
  gap: 8px 12px;
  margin: 0;
}
.kv dt {
  color: var(--muted);
}
.kv dd {
  margin: 0;
}
.groups {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 6px 14px;
}
div.groups {
  gap: 6px;
}
.check {
  display: inline-flex;
}
</style>
