<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { api, errorText, ok, type Token } from '../api/client'
import IdChip from '../components/IdChip.vue'
import ModalDialog from '../components/ModalDialog.vue'
import SegmentedFilter from '../components/SegmentedFilter.vue'
import TwoFactorPanel from '../components/TwoFactorPanel.vue'
import { ago } from '../state'
import { useScope, type Role } from '../stores/scope'
import { useSession } from '../stores/session'

const session = useSession()
const scope = useScope()
const tokens = ref<Token[] | null>(null)
const error = ref('')

async function load() {
  tokens.value = await ok(api.GET('/api/v1/tokens'))
}
onMounted(load)

// Creating a token; its secret is shown once.
const creating = ref(false)
const form = ref({ name: '', days: 90 as number | null, limited: false, tenant: '', role: 'viewer' as Role })
const secret = ref('')
const busy = ref(false)
function startCreate() {
  form.value = { name: '', days: 90, limited: false, tenant: scope.current, role: 'viewer' }
  secret.value = ''
  error.value = ''
  creating.value = true
}
async function create() {
  error.value = ''
  busy.value = true
  try {
    const f = form.value
    const scopes = f.limited ? [{ tenant_id: f.tenant, role: f.role }] : null
    const t = await ok(api.POST('/api/v1/tokens', { body: { name: f.name, expires_in_days: f.days, scopes } }))
    secret.value = t.token
    await load()
  } catch (e) {
    error.value = errorText(e)
  } finally {
    busy.value = false
  }
}
async function revoke(t: Token) {
  error.value = ''
  try {
    await ok(api.DELETE('/api/v1/tokens/{id}', { params: { path: { id: t.id } } }))
    await load()
  } catch (e) {
    error.value = errorText(e)
  }
}

// Changing one's own password.
const pw = ref({ current: '', next: '', again: '' })
const pwMessage = ref('')
const pwError = ref('')
async function changePassword() {
  pwMessage.value = ''
  pwError.value = ''
  if (pw.value.next !== pw.value.again) {
    pwError.value = 'The two passwords don’t match.'
    return
  }
  try {
    await ok(
      api.PATCH('/api/v1/users/{id}', {
        params: { path: { id: session.me!.user_id } },
        body: { password: pw.value.next, current_password: pw.value.current },
      }),
    )
    pw.value = { current: '', next: '', again: '' }
    pwMessage.value = 'Password changed. Your other sessions were signed out.'
  } catch (e) {
    pwError.value = errorText(e)
  }
}

// Everything the user can do, or one role at one tenant.
const access = computed({
  get: () => (form.value.limited ? 'limited' : 'all'),
  set: (v: string) => (form.value.limited = v === 'limited'),
})
const ACCESS = [
  { id: 'all', label: 'Everything you can do' },
  { id: 'limited', label: 'One role, at one tenant' },
]

const tenantName = (id: string) => scope.byId.get(id)?.name ?? 'a tenant you can no longer see'
const accessOf = (t: Token) =>
  t.scopes ? t.scopes.map((s) => `${s.role} at ${tenantName(s.tenant_id)}`).join(', ') : 'Everything you can do'
const expiry = (t: Token) => (t.expires_at ? new Date(t.expires_at).toLocaleDateString() : 'Never')
</script>

<template>
  <section v-if="session.me" class="account">
    <header class="page-head">
      <h1>{{ session.me.display_name || session.me.username }}</h1>
      <span class="muted">{{ session.me.username }}<template v-if="!scope.solo"> · {{ session.me.tenant_name }}</template></span>
    </header>

    <div class="grid">
      <article class="card">
        <h2>Your roles</h2>
        <ul class="roles">
          <li v-for="r in session.me.roles" :key="`${r.tenant_id}${r.role}`">
            <strong>{{ r.role }}</strong><template v-if="!scope.solo"> at {{ r.tenant_name }}</template>
            <span v-if="!scope.solo && r.tenant" class="mono muted">/{{ r.tenant }}</span>
          </li>
        </ul>
        <p v-if="!scope.solo" class="muted small">A role covers the tenant and everything below it.</p>
      </article>

      <article v-if="session.me.local" class="card">
        <h2>Password</h2>
        <form class="pw" @submit.prevent="changePassword">
          <label class="field">
            <span class="label">Current password</span>
            <input v-model="pw.current" type="password" required autocomplete="current-password" />
          </label>
          <label class="field">
            <span class="label">New password</span>
            <input v-model="pw.next" type="password" minlength="10" required autocomplete="new-password" />
          </label>
          <label class="field">
            <span class="label">Again</span>
            <input v-model="pw.again" type="password" minlength="10" required autocomplete="new-password" />
          </label>
          <p v-if="pwError" class="error-text" role="alert">{{ pwError }}</p>
          <div class="pw-row">
            <button class="btn">Change password</button>
            <span v-if="pwMessage" class="muted status" role="status">{{ pwMessage }}</span>
          </div>
        </form>
      </article>

      <TwoFactorPanel v-if="session.me.local" @changed="session.load()" />
    </div>

    <section class="tokens">
    <header class="tokens-head">
      <h2>API tokens</h2>
      <span class="muted">
        For scripts and automation. They act as you, with your roles or fewer. Send as
        <span class="mono">Authorization: Bearer …</span>
      </span>
      <button class="btn primary" @click="startCreate">Create token</button>
    </header>
    <p v-if="error && !creating" class="error-text">{{ error }}</p>
    <div v-if="tokens && tokens.length" class="table-wrap">
      <table class="data">
        <thead>
          <tr>
            <th>Name</th>
            <th>Access</th>
            <th>Created</th>
            <th>Last used</th>
            <th>Expires</th>
            <th />
          </tr>
        </thead>
        <tbody>
          <tr v-for="t in tokens" :key="t.id">
            <td class="token-name">{{ t.name }}</td>
            <td class="muted">{{ accessOf(t) }}</td>
            <td class="muted">{{ ago(t.created_at) }}</td>
            <td class="muted">{{ t.last_used_at ? ago(t.last_used_at) : 'Never' }}</td>
            <td class="muted">{{ expiry(t) }}</td>
            <td class="end"><button class="btn small danger" @click="revoke(t)">Revoke</button></td>
          </tr>
        </tbody>
      </table>
    </div>
    <div v-else-if="tokens" class="empty-state"><p>You have no API tokens.</p></div>
    </section>

    <ModalDialog v-if="creating" title="Create API token" @close="creating = false">
      <div v-if="secret" class="form">
        <p>Copy it now. It won’t be shown again.</p>
        <IdChip :value="secret" />
        <footer><button class="btn primary" @click="creating = false">Done</button></footer>
      </div>
      <form v-else class="form" @submit.prevent="create">
        <label class="field">
          <span class="label">Name</span>
          <input v-model="form.name" required maxlength="128" placeholder="terraform" autofocus />
          <span class="hint">So you can tell your tokens apart.</span>
        </label>
        <fieldset class="access">
          <legend class="label">Access</legend>
          <SegmentedFilter v-model="access" :options="ACCESS" label="Access" class="choice" />
          <template v-if="form.limited">
            <div class="pair">
              <label class="field">
                <span class="hint">Role</span>
                <select v-model="form.role">
                  <option value="viewer">Viewer</option>
                  <option value="operator">Operator</option>
                  <option value="admin">Admin</option>
                </select>
              </label>
              <label class="field">
                <span class="hint">At tenant</span>
                <select v-model="form.tenant" required>
                  <option v-for="t in scope.tenants" :key="t.id" :value="t.id">{{ t.name }}{{ t.slug_path ? ` (/${t.slug_path})` : '' }}</option>
                </select>
              </label>
            </div>
            <span class="hint">A limited token can’t change your account or make other tokens.</span>
          </template>
        </fieldset>
        <label class="field">
          <span class="label">Expires</span>
          <select v-model="form.days">
            <option :value="7">In 7 days</option>
            <option :value="30">In 30 days</option>
            <option :value="90">In 90 days</option>
            <option :value="365">In a year</option>
            <option :value="null">Never</option>
          </select>
        </label>
        <p v-if="error" class="error-text" role="alert">{{ error }}</p>
        <footer>
          <button type="button" class="btn" @click="creating = false">Cancel</button>
          <button class="btn primary" :disabled="busy">Create token</button>
        </footer>
      </form>
    </ModalDialog>
  </section>
</template>

<style scoped>
.account {
  display: grid;
  gap: 20px;
}
.page-head {
  margin: 0;
}
.grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(min(320px, 100%), 1fr));
  gap: 16px;
  align-items: start;
}
.card {
  display: grid;
  gap: 12px;
  align-content: start;
  padding: 16px 18px;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
  background: var(--panel);
}
.card h2,
.tokens-head h2 {
  font-size: 15px;
  letter-spacing: 0.08em;
  text-transform: uppercase;
}
.roles {
  display: grid;
  gap: 6px;
  margin: 0;
  padding: 0;
  list-style: none;
}
.small {
  margin: 0;
  font-size: 12px;
}
.pw {
  display: grid;
  gap: 10px;
}
.pw p {
  margin: 0;
}
.pw-row {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 10px;
}
.status {
  font-size: 13px;
}
.tokens {
  display: grid;
  gap: 12px;
}
.tokens-head {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 12px;
}
.tokens-head .btn {
  margin-left: auto;
}
.tokens .error-text {
  margin: 0;
}
.token-name {
  font-weight: 500;
}
.access {
  display: grid;
  gap: 8px;
  margin: 0;
  padding: 0;
  border: 0;
}
.access legend {
  padding: 0 0 4px;
}
.choice {
  justify-self: start;
}
.access > .hint {
  font-size: 12px;
  color: var(--muted);
}
.pair {
  display: grid;
  grid-template-columns: 1fr 2fr;
  gap: 12px;
}
.form :deep(.chip) {
  overflow-wrap: anywhere;
  text-align: left;
}
</style>
