<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { api, errorText, ok, type Token } from '../api/client'
import IdChip from '../components/IdChip.vue'
import ModalDialog from '../components/ModalDialog.vue'
import { ago } from '../state'
import { useSession } from '../stores/session'

const session = useSession()
const tokens = ref<Token[] | null>(null)
const error = ref('')

async function load() {
  tokens.value = await ok(api.GET('/api/v1/tokens'))
}
onMounted(load)

// Creating a token; its secret is shown once.
const creating = ref(false)
const form = ref({ name: '', days: 90 as number | null })
const secret = ref('')
const busy = ref(false)
function startCreate() {
  form.value = { name: '', days: 90 }
  secret.value = ''
  error.value = ''
  creating.value = true
}
async function create() {
  error.value = ''
  busy.value = true
  try {
    const t = await ok(api.POST('/api/v1/tokens', { body: { name: form.value.name, expires_in_days: form.value.days } }))
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
const pw = ref({ next: '', again: '' })
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
      api.PATCH('/api/v1/users/{id}', { params: { path: { id: session.me!.user_id } }, body: { password: pw.value.next } }),
    )
    pw.value = { next: '', again: '' }
    pwMessage.value = 'Password changed. Your other sessions were signed out.'
  } catch (e) {
    pwError.value = errorText(e)
  }
}

const expiry = (t: Token) => (t.expires_at ? new Date(t.expires_at).toLocaleDateString() : 'Never')
</script>

<template>
  <section v-if="session.me" class="account">
    <header class="page-head">
      <h1>{{ session.me.display_name || session.me.username }}</h1>
      <span class="muted">{{ session.me.username }} · {{ session.me.tenant_name }}</span>
    </header>

    <div class="grid">
      <article class="card">
        <h2>Your roles</h2>
        <ul class="roles">
          <li v-for="r in session.me.roles" :key="`${r.tenant_id}${r.role}`">
            <strong>{{ r.role }}</strong> at {{ r.tenant_name }}
            <span class="mono muted">/{{ r.tenant }}</span>
          </li>
        </ul>
        <p class="muted small">A role covers the tenant and everything below it.</p>
      </article>

      <article v-if="session.me.local" class="card">
        <h2>Password</h2>
        <form class="pw" @submit.prevent="changePassword">
          <label class="field">
            <span class="label">New password</span>
            <input v-model="pw.next" type="password" minlength="10" required autocomplete="new-password" />
          </label>
          <label class="field">
            <span class="label">Again</span>
            <input v-model="pw.again" type="password" minlength="10" required autocomplete="new-password" />
          </label>
          <p v-if="pwError" class="error-text" role="alert">{{ pwError }}</p>
          <p v-if="pwMessage" class="muted" role="status">{{ pwMessage }}</p>
          <button class="btn">Change password</button>
        </form>
      </article>
    </div>

    <header class="page-head tokens-head">
      <h2>API tokens</h2>
      <div class="actions"><button class="btn primary" @click="startCreate">Create token</button></div>
    </header>
    <p class="intro">
      For scripts and automation. A token acts as you, with your roles; send it as
      <span class="mono">Authorization: Bearer …</span>.
    </p>
    <p v-if="error && !creating" class="error-text">{{ error }}</p>
    <div v-if="tokens && tokens.length" class="table-wrap">
      <table class="data">
        <thead>
          <tr>
            <th>Name</th>
            <th>Created</th>
            <th>Last used</th>
            <th>Expires</th>
            <th />
          </tr>
        </thead>
        <tbody>
          <tr v-for="t in tokens" :key="t.id">
            <td>{{ t.name }}</td>
            <td class="muted">{{ ago(t.created_at) }}</td>
            <td class="muted">{{ t.last_used_at ? ago(t.last_used_at) : 'Never' }}</td>
            <td class="muted">{{ expiry(t) }}</td>
            <td class="end"><button class="btn small danger" @click="revoke(t)">Revoke</button></td>
          </tr>
        </tbody>
      </table>
    </div>
    <div v-else-if="tokens" class="empty-state"><p>You have no API tokens.</p></div>

    <ModalDialog v-if="creating" title="Create API token" @close="creating = false">
      <div v-if="secret" class="form">
        <p>Copy the token now. It won’t be shown again.</p>
        <IdChip :value="secret" />
        <footer><button class="btn primary" @click="creating = false">Done</button></footer>
      </div>
      <form v-else class="form" @submit.prevent="create">
        <label class="field">
          <span class="label">Name</span>
          <input v-model="form.name" required maxlength="128" placeholder="terraform" autofocus />
          <span class="hint">So you can tell your tokens apart.</span>
        </label>
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
.grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(300px, 1fr));
  gap: 16px;
  margin: 16px 0 32px;
}
.card {
  display: grid;
  gap: 12px;
  align-content: start;
  padding: 18px;
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
  gap: 12px;
  justify-items: start;
}
.pw .field {
  width: 100%;
}
.pw p {
  margin: 0;
}
.form :deep(.chip) {
  overflow-wrap: anywhere;
  text-align: left;
}
</style>
