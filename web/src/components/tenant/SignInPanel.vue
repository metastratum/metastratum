<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { api, ApiError, errorText, ok, type Schemas, type Tenant } from '../../api/client'
import { useScope } from '../../stores/scope'
import IdChip from '../IdChip.vue'

const props = defineProps<{ tenant: Tenant }>()
const scope = useScope()

type Provider = Schemas['ProviderView']
const existing = ref<Provider | null>(null)
const loaded = ref(false)
const editing = ref(false)
const error = ref('')
const busy = ref(false)

type Mapping = Schemas['MappingView']
type Form = Provider & { client_secret: string; role_mappings: Mapping[] }
const blank = (): Form => ({
  name: '',
  issuer: '',
  client_id: '',
  client_secret: '',
  scopes: 'openid profile email',
  username_claim: 'preferred_username',
  groups_claim: 'groups',
  default_role: null,
  role_mappings: [],
  redirect_uri: '',
})
const form = ref(blank())

const canManage = computed(() => scope.can('user.write', props.tenant.id))
// The controller reports its redirect URI once a provider exists.
const redirectUri = computed(() => existing.value?.redirect_uri || `${location.origin}/api/v1/auth/oidc/callback`)
const signInPath = computed(() => `/login?tenant=${encodeURIComponent(props.tenant.slug_path)}`)

const params = () => ({ params: { path: { id: props.tenant.id } } })

onMounted(async () => {
  try {
    existing.value = await ok(api.GET('/api/v1/tenants/{id}/identity-provider', params()))
  } catch (e) {
    if (!(e instanceof ApiError && e.status === 404)) error.value = errorText(e)
  }
  loaded.value = true
})

function edit() {
  form.value = existing.value ? { ...blank(), ...existing.value, client_secret: '', role_mappings: (existing.value.role_mappings ?? []).map((m) => ({ ...m })) } : blank()
  error.value = ''
  editing.value = true
}

async function save() {
  error.value = ''
  busy.value = true
  try {
    const body = { ...form.value, role_mappings: form.value.role_mappings.filter((m) => m.group.trim()) }
    existing.value = await ok(api.PUT('/api/v1/tenants/{id}/identity-provider', { ...params(), body }))
    editing.value = false
  } catch (e) {
    error.value = errorText(e)
  } finally {
    busy.value = false
  }
}

async function remove() {
  error.value = ''
  busy.value = true
  try {
    await ok(api.DELETE('/api/v1/tenants/{id}/identity-provider', params()))
    existing.value = null
  } catch (e) {
    error.value = errorText(e)
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <div v-if="loaded" class="panel">
    <template v-if="!editing">
      <p class="muted lead">
        People in {{ tenant.name }} can sign in with your organization’s identity provider (any OpenID Connect
        provider: Entra ID, Google, Keycloak, Okta…). Their account is created at their first sign-in, and their
        groups decide their role.
      </p>

      <dl v-if="existing" class="summary">
        <div><dt class="label">Provider</dt><dd>{{ existing.name }}</dd></div>
        <div><dt class="label">Issuer</dt><dd class="mono">{{ existing.issuer }}</dd></div>
        <div><dt class="label">Client ID</dt><dd class="mono">{{ existing.client_id }}</dd></div>
        <div>
          <dt class="label">Roles</dt>
          <dd>
            <div v-for="m in existing.role_mappings ?? []" :key="m.group">
              Group <span class="mono">{{ m.group }}</span> → <strong>{{ m.role }}</strong>
            </div>
            <div>
              <template v-if="existing.default_role">Anyone else → <strong>{{ existing.default_role }}</strong></template>
              <span v-else class="muted">Anyone else is turned away.</span>
            </div>
          </dd>
        </div>
        <div>
          <dt class="label">Sign-in page</dt>
          <dd><a :href="signInPath" class="mono">{{ signInPath }}</a></dd>
        </div>
      </dl>
      <p v-else class="muted">{{ tenant.name }} signs in with passwords only.</p>

      <div v-if="canManage" class="buttons">
        <button class="btn" :class="{ primary: !existing }" @click="edit">
          {{ existing ? 'Change' : 'Set up an identity provider' }}
        </button>
        <button v-if="existing" class="btn danger" :disabled="busy" @click="remove">Remove</button>
      </div>
    </template>

    <form v-else class="edit" @submit.prevent="save">
      <div class="callout">
        <span class="label">Register this redirect URI with the provider</span>
        <IdChip :value="redirectUri" />
      </div>
      <div class="row">
        <label class="field">
          <span class="label">Button label</span>
          <input v-model="form.name" required maxlength="64" placeholder="Acme SSO" />
          <span class="hint">Shown as “Continue with …” on the sign-in page.</span>
        </label>
        <label class="field">
          <span class="label">Issuer URL</span>
          <input v-model="form.issuer" required type="url" placeholder="https://login.example.com/realms/acme" />
          <span class="hint">Checked when you save.</span>
        </label>
      </div>
      <div class="row">
        <label class="field">
          <span class="label">Client ID</span>
          <input v-model="form.client_id" required autocomplete="off" />
        </label>
        <label class="field">
          <span class="label">Client secret</span>
          <input
            v-model="form.client_secret"
            type="password"
            autocomplete="new-password"
            :required="!existing"
            :placeholder="existing ? 'Unchanged' : ''"
          />
        </label>
      </div>
      <details>
        <summary class="label">Scopes and claims</summary>
        <div class="row">
          <label class="field">
            <span class="label">Scopes</span>
            <input v-model="form.scopes" class="mono" />
            <span class="hint">Add <span class="mono">groups</span> if the provider needs it to send groups.</span>
          </label>
          <label class="field">
            <span class="label">Username claim</span>
            <input v-model="form.username_claim" class="mono" />
          </label>
          <label class="field">
            <span class="label">Groups claim</span>
            <input v-model="form.groups_claim" class="mono" />
          </label>
        </div>
      </details>

      <fieldset class="mappings">
        <legend class="label">Roles from groups</legend>
        <div v-for="(m, i) in form.role_mappings" :key="i" class="mapping">
          <input v-model="m.group" placeholder="Group name" aria-label="Group" />
          <select v-model="m.role" aria-label="Role">
            <option value="admin">admin</option>
            <option value="operator">operator</option>
            <option value="viewer">viewer</option>
          </select>
          <button type="button" class="btn small" @click="form.role_mappings.splice(i, 1)">Remove</button>
        </div>
        <button type="button" class="btn small" @click="form.role_mappings.push({ group: '', role: 'viewer' })">
          Add group
        </button>
        <label class="field default">
          <span class="label">Everyone else</span>
          <select v-model="form.default_role">
            <option :value="null">Turn away</option>
            <option value="viewer">viewer</option>
            <option value="operator">operator</option>
            <option value="admin">admin</option>
          </select>
          <span class="hint">For people in none of these groups. Roles you give by hand also let people in.</span>
        </label>
      </fieldset>

      <p v-if="error" class="error-text" role="alert">{{ error }}</p>
      <footer>
        <button type="button" class="btn" @click="editing = false">Cancel</button>
        <button class="btn primary" :disabled="busy">{{ busy ? 'Checking…' : 'Save' }}</button>
      </footer>
    </form>
    <p v-if="error && !editing" class="error-text" role="alert">{{ error }}</p>
  </div>
</template>

<style scoped>
.panel {
  display: grid;
  gap: 18px;
}
.lead {
  max-width: 72ch;
  margin: 0;
}
.summary {
  display: grid;
  grid-template-columns: max-content 1fr;
  gap: 10px 20px;
  margin: 0;
}
.summary > div {
  display: contents;
}
.summary dd {
  margin: 0;
  overflow-wrap: anywhere;
}
.buttons {
  display: flex;
  gap: 8px;
}
.edit {
  display: grid;
  gap: 16px;
}
.row {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(220px, 1fr));
  gap: 12px;
}
.callout {
  display: grid;
  gap: 6px;
  justify-items: start;
  padding: 12px 14px;
  border-left: 3px solid var(--tape);
  background: var(--sunken);
}
.callout :deep(.chip) {
  max-width: 100%;
  overflow-wrap: anywhere;
  text-align: left;
}
details summary {
  cursor: pointer;
  margin-bottom: 12px;
}
.mappings {
  display: grid;
  gap: 8px;
  justify-items: start;
  margin: 0;
  padding: 14px;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
}
.mapping {
  display: grid;
  grid-template-columns: minmax(0, 1fr) 120px auto;
  gap: 8px;
  width: 100%;
}
.mapping input,
.mapping select {
  height: 32px;
  padding: 0 8px;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
  background: var(--panel);
  color: var(--ink);
  font: 400 14px var(--font-body);
}
.default {
  width: 100%;
  margin-top: 8px;
}
footer {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
}
.error-text {
  margin: 0;
}
@media (max-width: 520px) {
  .summary {
    grid-template-columns: 1fr;
  }
  .mapping {
    grid-template-columns: 1fr auto;
  }
  .mapping input {
    grid-column: 1 / -1;
  }
}
</style>
