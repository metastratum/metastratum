<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { api, errorText, ok, type Host } from '../api/client'

/** How the controller powers a failed host off, so its VMs can restart elsewhere. */
const props = defineProps<{ host: Host }>()

const kind = ref<'' | 'redfish' | 'libvirt'>('')
const url = ref('')
const username = ref('')
const password = ref('')
const hasPassword = ref(false)
const insecure = ref(false)
const domain = ref('')
const message = ref('')
const error = ref('')
const busy = ref(false)

const path = () => ({ params: { path: { id: props.host.id } } })

onMounted(async () => {
  try {
    const f = await ok(api.GET('/api/v1/hosts/{id}/fencing', path()))
    const c = (f.config ?? {}) as Record<string, unknown>
    kind.value = (f.kind ?? '') as typeof kind.value
    url.value = (c.url as string) ?? ''
    username.value = (c.username as string) ?? ''
    hasPassword.value = c.password === '(set)'
    insecure.value = !!c.insecure
    domain.value = (c.domain as string) ?? ''
  } catch (e) {
    error.value = errorText(e)
  }
})

async function save() {
  error.value = ''
  message.value = ''
  busy.value = true
  try {
    const config =
      kind.value === 'redfish'
        ? { url: url.value, username: username.value, insecure: insecure.value, ...(password.value && { password: password.value }) }
        : kind.value === 'libvirt'
          ? { domain: domain.value }
          : {}
    await ok(api.PUT('/api/v1/hosts/{id}/fencing', { ...path(), body: { kind: kind.value || null, config } }))
    password.value = ''
    hasPassword.value = kind.value === 'redfish'
    message.value = kind.value ? 'Saved.' : 'Fencing removed.'
  } catch (e) {
    error.value = errorText(e)
  } finally {
    busy.value = false
  }
}

async function test() {
  error.value = ''
  message.value = ''
  busy.value = true
  try {
    const r = await ok(api.POST('/api/v1/hosts/{id}/fencing/test', path()))
    message.value = `Reached it: the host is powered ${r.power}.`
  } catch (e) {
    error.value = errorText(e)
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <form class="fencing" @submit.prevent="save">
    <div>
      <strong>Fencing</strong>
      <p class="muted hint">
        How the controller powers {{ host.name }} off if it fails, before restarting its VMs elsewhere (in clusters
        with high availability). Without it, a failed host’s VMs are left alone.
      </p>
    </div>
    <label class="field">
      <span class="label">Method</span>
      <select v-model="kind">
        <option value="">None</option>
        <option value="redfish">Redfish (BMC)</option>
        <option value="libvirt">libvirt (host is a VM)</option>
      </select>
    </label>
    <template v-if="kind === 'redfish'">
      <label class="field">
        <span class="label">BMC address</span>
        <input v-model="url" required type="url" class="mono" placeholder="https://10.0.0.100" />
      </label>
      <div class="row">
        <label class="field">
          <span class="label">User</span>
          <input v-model="username" required autocomplete="off" />
        </label>
        <label class="field">
          <span class="label">Password</span>
          <input
            v-model="password"
            type="password"
            autocomplete="new-password"
            :required="!hasPassword"
            :placeholder="hasPassword ? 'Unchanged' : ''"
          />
        </label>
      </div>
      <label class="check">
        <input v-model="insecure" type="checkbox" />
        Accept the BMC’s self-signed certificate
      </label>
    </template>
    <label v-else-if="kind === 'libvirt'" class="field">
      <span class="label">Domain</span>
      <input v-model="domain" required class="mono" :placeholder="host.name" />
      <span class="hint">The host’s VM in libvirt on the controller’s machine.</span>
    </label>
    <p v-if="error" class="error-text" role="alert">{{ error }}</p>
    <p v-if="message" class="muted" role="status">{{ message }}</p>
    <footer>
      <button type="button" class="btn" :disabled="busy || !host.fence_kind" @click="test">Test</button>
      <button class="btn">Save fencing</button>
    </footer>
  </form>
</template>

<style scoped>
.fencing {
  display: grid;
  gap: 12px;
  padding: 12px 14px;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
}
.fencing p {
  margin: 0;
}
.hint {
  font-size: 12px;
}
.row {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 12px;
}
footer {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
}
</style>
