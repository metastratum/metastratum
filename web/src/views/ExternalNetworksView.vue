<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { api, errorText, ok, type ExternalNetwork } from '../api/client'
import InfraTabs from '../components/InfraTabs.vue'
import ModalDialog from '../components/ModalDialog.vue'
import { useLiveRefresh } from '../stores/live'

const nets = ref<ExternalNetwork[] | null>(null)
const error = ref('')

async function load() {
  nets.value = await ok(api.GET('/api/v1/external-networks'))
}
onMounted(load)
useLiveRefresh(['external_network', 'external_ip', 'host'], load)

type Dialog = { kind: 'edit'; net?: ExternalNetwork } | { kind: 'delete'; net: ExternalNetwork }
const dialog = ref<Dialog | null>(null)
const form = ref({ name: '', physical_network: '', cidr: '', gateway: '', pool_start: '', pool_end: '' })
const busy = ref(false)

function edit(n?: ExternalNetwork) {
  form.value = n
    ? { name: n.name, physical_network: n.physical_network, cidr: n.cidr, gateway: n.gateway, pool_start: n.pool_start, pool_end: n.pool_end }
    : { name: '', physical_network: 'external', cidr: '', gateway: '', pool_start: '', pool_end: '' }
  error.value = ''
  dialog.value = { kind: 'edit', net: n }
}

async function submit() {
  const d = dialog.value
  if (!d) return
  error.value = ''
  busy.value = true
  try {
    const f = form.value
    if (d.kind === 'delete') {
      await ok(api.DELETE('/api/v1/external-networks/{id}', { params: { path: { id: d.net.id } } }))
    } else if (d.net) {
      await ok(
        api.PUT('/api/v1/external-networks/{id}', {
          params: { path: { id: d.net.id } },
          body: { name: f.name, pool_start: f.pool_start, pool_end: f.pool_end },
        }),
      )
    } else {
      await ok(api.POST('/api/v1/external-networks', { body: { ...f, gateway: f.gateway || null } }))
    }
    dialog.value = null
    await load()
  } catch (e) {
    error.value = errorText(e)
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <section>
    <InfraTabs />
    <header class="page-head">
      <h1>External networks</h1>
      <div class="actions"><button class="btn primary" @click="edit()">Add external network</button></div>
    </header>
    <p class="intro">
      Networks outside metastratum that VPCs route to: the internet uplink, a datacenter VLAN. Each is a physical network
      on the hosts’ OVS bridges (<span class="mono">ovn-bridge-mappings</span>); VPC gateways run on hosts that reach it.
      Tenants use the ones granted to them.
    </p>

    <div v-if="nets && !nets.length" class="empty-state">
      <p>No external networks. Without one, VMs reach only each other.</p>
      <button class="btn primary" @click="edit()">Add external network</button>
    </div>
    <div v-else-if="nets" class="table-wrap">
      <table class="data">
        <thead>
          <tr>
            <th>Name</th>
            <th>Range</th>
            <th>Handed out</th>
            <th>Gateway hosts</th>
            <th class="num">In use</th>
            <th />
          </tr>
        </thead>
        <tbody>
          <tr v-for="n in nets" :key="n.id">
            <td>
              <strong>{{ n.name }}</strong>
              <div class="mono muted">{{ n.physical_network }}</div>
            </td>
            <td class="mono">{{ n.cidr }}<div class="muted">gw {{ n.gateway }}</div></td>
            <td class="mono">{{ n.pool_start }} – {{ n.pool_end }}</td>
            <td>
              <template v-if="n.hosts.length">{{ n.hosts.join(', ') }}</template>
              <span v-else class="error-text">No host reaches it</span>
            </td>
            <td class="num mono">{{ n.used }}</td>
            <td class="end">
              <button class="btn small" @click="edit(n)">Edit</button>
              <button class="btn small danger" :disabled="n.used > 0" @click="((error = ''), (dialog = { kind: 'delete', net: n }))">
                Delete
              </button>
            </td>
          </tr>
        </tbody>
      </table>
    </div>

    <ModalDialog
      v-if="dialog"
      :title="dialog.kind === 'delete' ? 'Delete external network' : dialog.net ? `Edit ${dialog.net.name}` : 'Add external network'"
      @close="dialog = null"
    >
      <form class="form" @submit.prevent="submit">
        <p v-if="dialog.kind === 'delete'">Delete <strong>{{ dialog.net.name }}</strong>? Tenants lose access to it.</p>
        <template v-else>
          <div class="row">
            <label class="field">
              <span class="label">Name</span>
              <input v-model="form.name" required maxlength="64" autofocus />
            </label>
            <label class="field">
              <span class="label">Physical network</span>
              <input v-model="form.physical_network" required class="mono" :disabled="!!dialog.net" />
              <span class="hint">As named in the hosts’ bridge mappings.</span>
            </label>
          </div>
          <div class="row">
            <label class="field">
              <span class="label">Range</span>
              <input v-model="form.cidr" required class="mono" placeholder="203.0.113.0/24" :disabled="!!dialog.net" />
            </label>
            <label class="field">
              <span class="label">Upstream gateway</span>
              <input v-model="form.gateway" class="mono" placeholder="First address" :disabled="!!dialog.net" />
            </label>
          </div>
          <div class="row">
            <label class="field">
              <span class="label">Hand out from</span>
              <input v-model="form.pool_start" required class="mono" />
            </label>
            <label class="field">
              <span class="label">to</span>
              <input v-model="form.pool_end" required class="mono" />
            </label>
          </div>
          <p class="muted">VPC gateways and floating IPs get addresses from this pool; keep it clear of other users of the network.</p>
        </template>
        <p v-if="error" class="error-text" role="alert">{{ error }}</p>
        <footer>
          <button type="button" class="btn" @click="dialog = null">Cancel</button>
          <button class="btn" :class="dialog.kind === 'delete' ? 'danger' : 'primary'" :disabled="busy">
            {{ dialog.kind === 'delete' ? 'Delete' : 'Save' }}
          </button>
        </footer>
      </form>
    </ModalDialog>
  </section>
</template>

<style scoped>
.end .btn + .btn {
  margin-left: 6px;
}
.data td div {
  font-size: 12px;
}
</style>
