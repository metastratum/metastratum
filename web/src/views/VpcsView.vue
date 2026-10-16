<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { api, errorText, ok, type Schemas, type Vpc } from '../api/client'
import ModalDialog from '../components/ModalDialog.vue'
import NetTabs from '../components/NetTabs.vue'
import { useLiveRefresh } from '../stores/live'
import { useScope } from '../stores/scope'

const scope = useScope()
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

const TITLES: Record<Dialog['kind'], string> = {
  vpc: 'New VPC',
  network: 'Add network',
  uplink: 'Route to an external network',
  'delete-vpc': 'Delete VPC',
  'delete-network': 'Delete network',
}
</script>

<template>
  <section>
    <NetTabs />
    <header class="page-head">
      <h1>VPCs</h1>
      <div v-if="canWrite" class="actions"><button class="btn primary" @click="open({ kind: 'vpc' })">New VPC</button></div>
    </header>
    <p class="intro">
      A VPC is a private routing domain: its networks reach each other, and nothing else does unless the VPC routes
      to an external network. Networks in different VPCs may use the same addresses.
    </p>
    <p v-if="error" class="error-text">{{ error }}</p>

    <div v-if="vpcs && !vpcs.length" class="empty-state">
      <p>{{ scope.tenant?.name }} has no VPCs yet. One is created for the first VM, or create your own.</p>
      <button v-if="canWrite" class="btn primary" @click="open({ kind: 'vpc' })">New VPC</button>
    </div>

    <ul v-else-if="vpcs" class="vpcs">
      <li v-for="v in vpcs" :key="v.id" class="vpc">
        <!-- Uplink: where the VPC's traffic leaves it. -->
        <div class="uplink" :class="{ none: !v.external_network_id }">
          <template v-if="v.external_network_id">
            <span class="ext">{{ v.external_network_name }}</span>
            <span class="mono muted">via {{ v.gateway_ip }}</span>
          </template>
          <span v-else class="muted">Internal only — no route out</span>
          <button v-if="canWrite" class="btn small" @click="open({ kind: 'uplink', vpc: v })">
            {{ v.external_network_id ? 'Change' : 'Connect' }}
          </button>
        </div>
        <div class="router">
          <span class="tape">{{ v.name }}</span>
          <span class="label">VPC router</span>
          <div class="router-actions">
            <button v-if="canWrite" class="btn small" @click="open({ kind: 'network', vpc: v })">Add network</button>
            <button v-if="canWrite" class="btn small danger" @click="open({ kind: 'delete-vpc', vpc: v })">Delete</button>
          </div>
        </div>
        <ul class="bus">
          <li v-for="n in v.networks" :key="n.id" class="net">
            <span class="drop" aria-hidden="true" />
            <strong>{{ n.name }}</strong>
            <span class="mono">{{ n.cidr }}</span>
            <span class="mono muted">gateway {{ n.gateway }}</span>
            <span class="muted">{{ n.ports }} {{ n.ports === 1 ? 'VM' : 'VMs' }}</span>
            <button
              v-if="canWrite"
              class="btn small danger"
              :disabled="n.ports > 0"
              :title="n.ports > 0 ? 'VMs are connected' : undefined"
              @click="open({ kind: 'delete-network', vpc: v, network: n })"
            >
              Delete
            </button>
          </li>
          <li v-if="!v.networks.length" class="net empty">
            <span class="drop" aria-hidden="true" />
            <span class="muted">No networks yet.</span>
          </li>
        </ul>
      </li>
    </ul>

    <ModalDialog v-if="dialog" :title="TITLES[dialog.kind]" @close="dialog = null">
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
              <template v-if="!externals.length"> None is available to {{ scope.tenant?.name }}.</template>
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
.vpcs {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(min(100%, 520px), 1fr));
  gap: 16px;
  margin: 0;
  padding: 0;
  list-style: none;
}
.vpc {
  padding: 0 0 12px;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
  background: var(--panel);
}
/* The uplink sits above the router like a cable to the patch panel. */
.uplink {
  position: relative;
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 10px;
  padding: 10px 16px 18px;
  border-bottom: 1px dashed var(--rule);
}
.uplink::after {
  content: '';
  position: absolute;
  bottom: -1px;
  left: 31px;
  height: 14px;
  border-left: 2px solid var(--led-on);
}
.uplink.none::after {
  border-left-style: dotted;
  border-left-color: var(--led-off);
}
.uplink .btn {
  margin-left: auto;
}
.ext {
  font-weight: 600;
}
.router {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 10px;
  padding: 12px 16px 4px;
}
.tape {
  padding: 3px 9px 2px;
  background: var(--tape);
  color: var(--tape-ink);
  font: 600 13px var(--font-label);
  letter-spacing: 0.1em;
  text-transform: uppercase;
  clip-path: polygon(4px 0, 100% 0, calc(100% - 4px) 100%, 0 100%);
}
.router-actions {
  display: flex;
  gap: 6px;
  margin-left: auto;
}
/* Networks hang off a bus, drawn like the tenant tree's connectors. */
.bus {
  margin: 4px 0 0;
  padding: 0 16px 0 31px;
  list-style: none;
}
.net {
  position: relative;
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 4px 12px;
  min-height: 38px;
  padding-left: 22px;
}
.drop {
  position: absolute;
  top: 0;
  bottom: 0;
  left: 0;
  border-left: 1px solid var(--muted);
  opacity: 0.6;
}
.net:last-child .drop {
  bottom: 50%;
}
.drop::after {
  content: '';
  position: absolute;
  top: 50%;
  left: 0;
  width: 14px;
  border-top: 1px solid var(--muted);
}
.net .btn {
  margin-left: auto;
}
.net.empty {
  color: var(--muted);
}
</style>
