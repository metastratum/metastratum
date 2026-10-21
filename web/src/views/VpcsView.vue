<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { api, errorText, ok, type Schemas, type Vpc } from '../api/client'
import ModalDialog from '../components/ModalDialog.vue'
import StatusLed from '../components/StatusLed.vue'
import { useLiveRefresh } from '../stores/live'
import { useScope } from '../stores/scope'
import { useSession } from '../stores/session'

const scope = useScope()
const session = useSession()
const vpcs = ref<Vpc[] | null>(null)
const externals = ref<Schemas['UsableExternalNetwork'][]>([])
const error = ref('')

async function load() {
  if (!scope.current) return
  const tenant = scope.current
  try {
    const [v, e] = await Promise.all([
      ok(api.GET('/api/v1/vpcs', { params: { query: { tenant_id: tenant } } })),
      ok(api.GET('/api/v1/tenants/{id}/external-networks', { params: { path: { id: tenant } } })),
    ])
    if (tenant === scope.current) [vpcs.value, externals.value] = [v, e]
  } catch (e) {
    error.value = errorText(e)
  }
}
watch(() => scope.current, load, { immediate: true })
useLiveRefresh(['vpc', 'network', 'vm'], load)

const canWrite = computed(() => scope.can('network.write'))

// Dialogs.
type Dialog =
  | { kind: 'vpc' }
  | { kind: 'network'; vpc: Vpc }
  | { kind: 'uplink'; vpc: Vpc }
  | { kind: 'delete-vpc'; vpc: Vpc }
  | { kind: 'delete-network'; vpc: Vpc; network: Vpc['networks'][number] }
const dialog = ref<Dialog | null>(null)
const form = ref({ name: '', cidr: '', external: '' as string | null })
const formError = ref('')
const busy = ref(false)

function open(d: Dialog) {
  formError.value = ''
  form.value = {
    name: '',
    cidr: d.kind === 'network' ? suggestCidr(d.vpc) : '',
    external: d.kind === 'uplink' ? (d.vpc.external_network_id ?? null) : null,
  }
  dialog.value = d
}

/** The first 10.N.0.0/24 the VPC doesn't use yet. */
function suggestCidr(v: Vpc): string {
  const used = new Set(v.networks.map((n) => n.cidr))
  for (let i = 0; i < 255; i++) if (!used.has(`10.${i}.0.0/24`)) return `10.${i}.0.0/24`
  return ''
}

async function submit() {
  const d = dialog.value
  if (!d) return
  formError.value = ''
  busy.value = true
  try {
    if (d.kind === 'vpc') {
      await ok(
        api.POST('/api/v1/vpcs', {
          body: { tenant_id: scope.current, name: form.value.name, external_network_id: form.value.external || null },
        }),
      )
    } else if (d.kind === 'network') {
      await ok(api.POST('/api/v1/networks', { body: { vpc_id: d.vpc.id, name: form.value.name, cidr: form.value.cidr } }))
    } else if (d.kind === 'uplink') {
      await ok(
        api.PATCH('/api/v1/vpcs/{id}', {
          params: { path: { id: d.vpc.id } },
          body: { external_network_id: form.value.external || null },
        }),
      )
    } else if (d.kind === 'delete-vpc') {
      await ok(api.DELETE('/api/v1/vpcs/{id}', { params: { path: { id: d.vpc.id } } }))
    } else {
      await ok(api.DELETE('/api/v1/networks/{id}', { params: { path: { id: d.network.id } } }))
    }
    dialog.value = null
    await load()
  } catch (e) {
    formError.value = errorText(e)
  } finally {
    busy.value = false
  }
}

function title(d: Dialog): string {
  switch (d.kind) {
    case 'vpc':
      return 'New VPC'
    case 'network':
      return `Add network to ${d.vpc.name}`
    case 'uplink':
      return `Route ${d.vpc.name} to an external network`
    case 'delete-vpc':
      return 'Delete VPC'
    case 'delete-network':
      return 'Delete network'
  }
}
</script>

<template>
  <section>
    <header class="page-head">
      <h1>VPCs</h1>
      <span v-if="vpcs" class="mono muted">{{ vpcs.length }}</span>
      <div v-if="canWrite" class="actions"><button class="btn primary" @click="open({ kind: 'vpc' })">New VPC</button></div>
    </header>
    <p class="intro">
      Networks in a VPC reach each other and nothing else, unless the VPC routes to an external network. Networks in
      different VPCs may use the same addresses.
    </p>
    <p v-if="error" class="error-text">{{ error }}</p>

    <div v-if="vpcs && !vpcs.length" class="empty-state">
      <p>
        {{ scope.solo ? 'No VPCs' : `${scope.tenant?.name} has no VPCs` }} yet. One is created for the first VM, or
        create your own.
      </p>
      <button v-if="canWrite" class="btn primary" @click="open({ kind: 'vpc' })">New VPC</button>
    </div>

    <div v-else-if="vpcs" class="vpcs">
      <section v-for="v in vpcs" :key="v.id" class="vpc">
        <div class="vpc-head">
          <h2>{{ v.name }}</h2>
          <!-- Uplink: where the VPC's traffic leaves it. -->
          <span class="uplink">
            <StatusLed :tone="v.external_network_id ? 'on' : 'off'" />
            <span v-if="v.external_network_id">
              Routes to
              <RouterLink v-if="session.me?.provider_admin" to="/external-networks" class="ext">{{
                v.external_network_name
              }}</RouterLink>
              <span v-else class="ext">{{ v.external_network_name }}</span>
              <span class="mono muted"> via {{ v.gateway_ip }}</span>
            </span>
            <span v-else class="muted">Internal only</span>
          </span>
          <button v-if="canWrite" class="btn small" @click="open({ kind: 'uplink', vpc: v })">
            {{ v.external_network_id ? 'Change' : 'Connect' }}
          </button>
          <div v-if="canWrite" class="vpc-actions">
            <button class="btn small" @click="open({ kind: 'network', vpc: v })">Add network</button>
            <button class="btn small danger" @click="open({ kind: 'delete-vpc', vpc: v })">Delete</button>
          </div>
        </div>
        <div v-if="v.networks.length" class="nets">
          <table class="data">
            <thead>
              <tr>
                <th>Network</th>
                <th>Range</th>
                <th>Gateway</th>
                <th class="num">VMs</th>
                <th v-if="canWrite"><span class="sr-only">Actions</span></th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="n in v.networks" :key="n.id">
                <td class="name">{{ n.name }}</td>
                <td class="mono">{{ n.cidr }}</td>
                <td class="mono muted">{{ n.gateway }}</td>
                <td class="num mono">{{ n.ports }}</td>
                <td v-if="canWrite" class="end">
                  <button
                    class="btn small danger"
                    :disabled="n.ports > 0"
                    :title="n.ports > 0 ? 'VMs are connected' : undefined"
                    @click="open({ kind: 'delete-network', vpc: v, network: n })"
                  >
                    Delete
                  </button>
                </td>
              </tr>
            </tbody>
          </table>
        </div>
        <p v-else class="none muted">No networks yet.</p>
      </section>
    </div>

    <ModalDialog v-if="dialog" :title="title(dialog)" @close="dialog = null">
      <form class="form" @submit.prevent="submit">
        <template v-if="dialog.kind === 'vpc'">
          <label class="field">
            <span class="label">Name</span>
            <input v-model="form.name" required maxlength="64" autofocus />
          </label>
        </template>
        <template v-if="dialog.kind === 'vpc' || dialog.kind === 'uplink'">
          <label class="field">
            <span class="label">External network</span>
            <select v-model="form.external">
              <option :value="null">None (internal only)</option>
              <option v-for="e in externals" :key="e.id" :value="e.id">{{ e.name }} ({{ e.cidr }})</option>
            </select>
            <span class="hint">
              VMs reach it (and beyond) through the VPC's address there. Floating IPs need it too.
              <template v-if="!externals.length">
                {{
                  scope.solo
                    ? ' There is none yet (Infrastructure → External networks).'
                    : ` None is available to ${scope.tenant?.name}.`
                }}
              </template>
            </span>
          </label>
        </template>
        <template v-if="dialog.kind === 'network'">
          <div class="row">
            <label class="field">
              <span class="label">Name</span>
              <input v-model="form.name" required maxlength="64" autofocus />
            </label>
            <label class="field">
              <span class="label">Address range</span>
              <input v-model="form.cidr" required class="mono" placeholder="10.1.0.0/24" />
              <span class="hint">/16 to /29. The first address is the gateway.</span>
            </label>
          </div>
        </template>
        <p v-if="dialog.kind === 'delete-vpc'">
          Delete <strong>{{ dialog.vpc.name }}</strong>? Delete its networks first. Its address on the external network
          is released.
        </p>
        <p v-if="dialog.kind === 'delete-network'">
          Delete network <strong>{{ dialog.network.name }}</strong> ({{ dialog.network.cidr }})?
        </p>
        <p v-if="formError" class="error-text" role="alert">{{ formError }}</p>
        <footer>
          <button type="button" class="btn" @click="dialog = null">Cancel</button>
          <button class="btn" :class="dialog.kind.startsWith('delete') ? 'danger' : 'primary'" :disabled="busy">
            {{ dialog.kind.startsWith('delete') ? 'Delete' : dialog.kind === 'uplink' ? 'Save' : 'Create' }}
          </button>
        </footer>
      </form>
    </ModalDialog>
  </section>
</template>

<style scoped>
.page-head {
  margin-bottom: 4px;
}
.vpcs {
  display: grid;
  gap: 18px;
}
.vpc {
  border: 1px solid var(--rule);
  border-radius: var(--radius);
  background: var(--panel);
}
.vpc-head {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 10px 14px;
  padding: 14px 16px;
}
.vpc-head h2 {
  font-size: 16px;
}
.uplink {
  display: inline-flex;
  align-items: center;
  gap: 8px;
}
.ext {
  font-weight: 500;
}
.vpc-actions {
  display: flex;
  gap: 6px;
  margin-left: auto;
}
.nets {
  overflow-x: auto;
  border-top: 1px solid var(--rule);
}
.data th:first-child,
.data td:first-child {
  padding-left: 16px;
}
.data th:last-child,
.data td:last-child {
  padding-right: 16px;
}
.name {
  font-weight: 600;
}
.none {
  margin: 0;
  padding: 14px 16px;
  border-top: 1px solid var(--rule);
}
.sr-only {
  position: absolute;
  width: 1px;
  height: 1px;
  overflow: hidden;
  clip-path: inset(50%);
  white-space: nowrap;
}
</style>
