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
// Where the tenant's people sign in: at the nearest domain bound to it or
// above it, else at this address.
const signIn = ref<Schemas['SignInView'] | null>(null)
const redirectUri = computed(() => signIn.value?.redirect_uri ?? `${location.origin}/api/v1/auth/oidc/callback`)

const params = () => ({ params: { path: { id: props.tenant.id } } })

// Whether password accounts here and below need a second factor.
const policy = ref<Schemas['TwoFactorPolicy'] | null>(null)
async function setRequired(required: boolean) {
  error.value = ''
  busy.value = true
  try {
    policy.value = await ok(api.PUT('/api/v1/tenants/{id}/two-factor-policy', { ...params(), body: { required } }))
  } catch (e) {
    error.value = errorText(e)
  } finally {
    busy.value = false
  }
}

onMounted(async () => {
  try {
    ;[existing.value, signIn.value, policy.value] = await Promise.all([
      ok(api.GET('/api/v1/tenants/{id}/identity-provider', params())).catch((e) => {
        if (e instanceof ApiError && e.status === 404) return null
        throw e
      }),
      ok(api.GET('/api/v1/tenants/{id}/sign-in', params())),
      ok(api.GET('/api/v1/tenants/{id}/two-factor-policy', params())),
    ])
  } catch (e) {
    error.value = errorText(e)
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
    <section class="idp">
      <header class="sub-head">
        <h3>Identity provider</h3>
        <div v-if="canManage && !editing" class="buttons">
          <button class="btn small" :class="{ primary: !existing }" @click="edit">
            {{ existing ? 'Change' : 'Set up an identity provider' }}
          </button>
          <button v-if="existing" class="btn small danger" :disabled="busy" @click="remove">Remove</button>
        </div>
      </header>
      <template v-if="!editing">
        <p class="muted note">
          Any OpenID Connect provider: Entra ID, Google, Keycloak, Okta… Accounts are created at first sign-in; groups
          decide the role.
        </p>
        <dl v-if="existing" class="summary">
          <dt>Provider</dt>
          <dd>{{ existing.name }}</dd>
          <dt>Issuer</dt>
          <dd class="mono">{{ existing.issuer }}</dd>
          <dt>Client ID</dt>
          <dd class="mono">{{ existing.client_id }}</dd>
          <dt>Roles</dt>
          <dd class="roles">
            <span v-for="m in existing.role_mappings ?? []" :key="m.group">
              Group <span class="mono">{{ m.group }}</span> → <strong>{{ m.role }}</strong>
            </span>
            <span v-if="existing.default_role">Anyone else → <strong>{{ existing.default_role }}</strong></span>
            <span v-else class="muted">Anyone else is turned away.</span>
          </dd>
        </dl>
        <p v-else class="plain">{{ scope.solo ? 'People sign in' : `${tenant.name} signs in` }} with passwords only.</p>
        <dl v-if="signIn" class="summary">
          <dt>Sign-in page</dt>
          <dd><a :href="signIn.page" class="mono">{{ signIn.page }}</a></dd>
        </dl>
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
          <span class="muted small">Everyone else</span>
          <select v-model="form.default_role">
            <option :value="null">Turn away</option>
            <option value="viewer">viewer</option>
            <option value="operator">operator</option>
            <option value="admin">admin</option>
          </select>
          <span class="hint">For people in none of these groups. Roles given by hand also let people in.</span>
        </label>
      </fieldset>

      <p v-if="error" class="error-text" role="alert">{{ error }}</p>
      <footer>
        <button type="button" class="btn" @click="editing = false">Cancel</button>
        <button class="btn primary" :disabled="busy">{{ busy ? 'Checking…' : 'Save' }}</button>
      </footer>
    </form>
    </section>
    <p v-if="error && !editing" class="error-text" role="alert">{{ error }}</p>

    <section v-if="policy" class="divided">
      <h3>Two-factor sign-in</h3>
      <div class="setting">
        <div class="setting-text">
          <span id="tfa-label" class="setting-name">Require a second factor</span>
          <span class="muted small">
            <template v-if="policy.required_by">Required by {{ policy.required_by.name }}.</template>
            <template v-else>
              For password accounts{{ scope.solo ? '' : ` in ${tenant.name} and below` }}: an authenticator app, a security
              key or a YubiKey. People without one can only add one when they next sign in. Identity-provider accounts use
              its own.
            </template>
          </span>
        </div>
        <button
          type="button"
          role="switch"
          class="switch"
          aria-labelledby="tfa-label"
          :aria-checked="policy.required || !!policy.required_by"
          :disabled="!canManage || !!policy.required_by || busy"
          :title="policy.required_by ? `Required by ${policy.required_by.name}` : undefined"
          @click="setRequired(!policy.required)"
        >
          <span class="knob" />
        </button>
      </div>
    </section>
  </div>
</template>

<style scoped>
.panel {
  display: grid;
  gap: 18px;
}
section {
  display: grid;
  gap: 12px;
}
/* Each part of signing in after the first, ruled off. */
.divided {
  gap: 10px;
  padding-top: 16px;
  border-top: 1px solid var(--rule);
}
h3 {
  font-size: 15px;
  letter-spacing: 0.08em;
  text-transform: uppercase;
}
.sub-head {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px 12px;
}
.buttons {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
  margin-left: auto;
}
.note {
  margin: 0;
  font-size: 13px;
}
.plain {
  margin: 0;
}
.small {
  font-size: 13px;
}
.summary {
  display: grid;
  grid-template-columns: 140px minmax(0, 1fr);
  gap: 8px 16px;
  margin: 0;
}
.summary dt {
  color: var(--muted);
}
.summary dd {
  margin: 0;
  overflow-wrap: anywhere;
}
.summary .roles {
  display: grid;
  gap: 2px;
}
/* A setting and its switch, side by side. */
.setting {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 12px 24px;
}
.setting-text {
  display: grid;
  flex: 1 1 320px;
  gap: 2px;
  max-width: 72ch;
}
.setting-name {
  font-weight: 500;
}
.switch {
  position: relative;
  flex: none;
  width: 40px;
  height: 22px;
  padding: 0;
  border: 0;
  border-radius: 11px;
  background: var(--led-off);
  cursor: pointer;
}
.switch[aria-checked='true'] {
  background: var(--ink);
}
.switch:disabled {
  opacity: 0.45;
  cursor: default;
}
.knob {
  position: absolute;
  top: 3px;
  left: 3px;
  width: 16px;
  height: 16px;
  border-radius: 50%;
  background: var(--panel);
  transition: left 0.12s;
}
.switch[aria-checked='true'] .knob {
  left: 21px;
  background: var(--tape);
}
@media (prefers-reduced-motion: reduce) {
  .knob {
    transition: none;
  }
}
.edit {
  display: grid;
  gap: 14px;
  padding: 14px;
  border-radius: var(--radius);
  background: var(--aisle);
}
.row {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(min(240px, 100%), 1fr));
  gap: 12px;
}
.callout {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px;
}
.callout :deep(.chip) {
  max-width: 100%;
  overflow-wrap: anywhere;
  text-align: left;
}
details summary {
  cursor: pointer;
  margin-bottom: 10px;
}
.mappings {
  display: grid;
  gap: 8px;
  margin: 0;
  padding: 12px 14px 14px;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
}
.mappings legend {
  padding: 0 6px;
}
.mappings > .btn {
  justify-self: start;
}
.mapping {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px;
}
.mapping input {
  flex: 1;
  min-width: 160px;
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
  margin-top: 6px;
}
.default select {
  justify-self: start;
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
    grid-template-columns: minmax(0, 1fr);
  }
}
</style>
