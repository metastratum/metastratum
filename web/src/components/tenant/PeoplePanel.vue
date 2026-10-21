<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { api, errorText, ok, type Binding, type Tenant, type User } from '../../api/client'
import { ago } from '../../state'
import { useScope } from '../../stores/scope'
import { useSession } from '../../stores/session'
import ModalDialog from '../ModalDialog.vue'

const props = defineProps<{ tenant: Tenant }>()
const scope = useScope()
const session = useSession()

const bindings = ref<Binding[] | null>(null)
const users = ref<User[] | null>(null)
const error = ref('')
const rowError = ref<Record<string, string>>({})

const canManage = computed(() => scope.can('user.write', props.tenant.id))
const ROLES = [
  { id: 'admin', text: 'Everything here and below: tenants, people, quotas, VMs.' },
  { id: 'operator', text: 'Creates and runs VMs and images; sees the rest.' },
  { id: 'viewer', text: 'Sees everything, changes nothing.' },
] as const

async function load() {
  error.value = ''
  try {
    const id = props.tenant.id
    ;[bindings.value, users.value] = await Promise.all([
      ok(api.GET('/api/v1/tenants/{id}/role-bindings', { params: { path: { id } } })),
      ok(api.GET('/api/v1/users', { params: { query: { tenant_id: id } } })),
    ])
  } catch (e) {
    error.value = errorText(e)
  }
}
onMounted(load)

const rolesOf = (u: User) => (bindings.value ?? []).filter((b) => b.user_id === u.id)

// Accounts: filtered by name, the first few shown until asked for all.
const SHOWN = 5
const accountQuery = ref('')
const allAccounts = ref(false)
const matching = computed(() => {
  const q = accountQuery.value.trim().toLowerCase()
  return (users.value ?? []).filter(
    (u) => !q || u.username.toLowerCase().includes(q) || u.display_name.toLowerCase().includes(q),
  )
})
const shownAccounts = computed(() => (allAccounts.value ? matching.value : matching.value.slice(0, SHOWN)))

async function revoke(b: Binding) {
  rowError.value = {}
  try {
    await ok(api.DELETE('/api/v1/role-bindings/{id}', { params: { path: { id: b.id } } }))
    await load()
  } catch (e) {
    rowError.value[b.id] = errorText(e)
  }
}

async function setDisabled(u: User, disabled: boolean) {
  rowError.value = {}
  try {
    await ok(api.PATCH('/api/v1/users/{id}', { params: { path: { id: u.id } }, body: { disabled } }))
    await load()
  } catch (e) {
    rowError.value[u.id] = errorText(e)
  }
}

// Dialogs: give someone a role here, add a user, reset a password, delete.
type Dialog =
  | { kind: 'grant' }
  | { kind: 'add' }
  | { kind: 'password'; user: User }
  | { kind: 'reset'; user: User }
  | { kind: 'delete'; user: User }
const dialog = ref<Dialog | null>(null)
const busy = ref(false)
const formError = ref('')
const grant = ref({ user_id: '', role: 'operator' })
const add = ref({ username: '', display_name: '', email: '', password: '', role: 'operator' })
const password = ref('')

// People who can be given a role here: users of this tenant and the
// tenants above it, as far as the current user can see.
const candidates = ref<User[]>([])
async function openGrant() {
  open({ kind: 'grant' })
  grant.value = { user_id: '', role: 'operator' }
  const lineage = scope.lineage(props.tenant.id).filter((t) => scope.can('user.read', t.id))
  const lists = await Promise.all(
    lineage.map((t) => ok(api.GET('/api/v1/users', { params: { query: { tenant_id: t.id } } })).catch(() => [])),
  )
  candidates.value = lists.flat().filter((u) => !u.disabled)
}
function tenantName(id: string) {
  return scope.byId.get(id)?.name ?? ''
}
// After a password or second-factor reset: also revoke the person's API
// tokens. Only when someone else may have got into the account; otherwise
// it would break their automation for nothing.
const revokeTokens = ref(false)
function open(d: Dialog) {
  formError.value = ''
  password.value = ''
  revokeTokens.value = false
  dialog.value = d
}

async function submit() {
  const d = dialog.value
  if (!d) return
  formError.value = ''
  busy.value = true
  try {
    if (d.kind === 'grant') {
      await ok(api.POST('/api/v1/role-bindings', { body: { tenant_id: props.tenant.id, ...grant.value } }))
    } else if (d.kind === 'add') {
      const { email, ...rest } = add.value
      await ok(api.POST('/api/v1/users', { body: { tenant_id: props.tenant.id, ...rest, email: email || null } }))
      add.value = { username: '', display_name: '', email: '', password: '', role: 'operator' }
    } else if (d.kind === 'password') {
      await ok(
        api.PATCH('/api/v1/users/{id}', {
          params: { path: { id: d.user.id } },
          body: { password: password.value, revoke_tokens: revokeTokens.value },
        }),
      )
    } else if (d.kind === 'reset') {
      await ok(
        api.DELETE('/api/v1/users/{id}/two-factor', {
          params: { path: { id: d.user.id }, query: { revoke_tokens: revokeTokens.value } },
        }),
      )
    } else {
      await ok(api.DELETE('/api/v1/users/{id}', { params: { path: { id: d.user.id } } }))
    }
    dialog.value = null
    await load()
  } catch (e) {
    formError.value = errorText(e)
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <div class="panel">
    <p v-if="error" class="error-text">{{ error }}</p>

    <section>
      <header class="sub-head">
        <h3>{{ scope.solo ? 'Roles' : `Roles at ${tenant.name}` }}</h3>
        <button v-if="canManage" class="btn small primary push" @click="openGrant">Give access</button>
      </header>
      <p class="muted note">
        <template v-if="scope.solo">What each person may do.</template>
        <template v-else>Also apply in every sub-tenant. Roles at a tenant above give access too.</template>
      </p>
      <div v-if="bindings && bindings.length" class="table-wrap">
        <table class="data">
          <thead>
            <tr>
              <th>Person</th>
              <th>Role</th>
              <th>Given</th>
              <th />
            </tr>
          </thead>
          <tbody>
            <tr v-for="b in bindings" :key="b.id">
              <td>{{ b.username }}</td>
              <td>
                <div class="tags">
                  <span class="role" :class="b.role">{{ b.role }}</span>
                  <span v-if="b.source === 'oidc'" class="tag" title="From the identity provider’s groups; set again at each sign-in">
                    From groups
                  </span>
                </div>
              </td>
              <td class="muted">{{ ago(b.created_at) }}</td>
              <td class="end">
                <span v-if="rowError[b.id]" class="error-text">{{ rowError[b.id] }}</span>
                <button v-if="canManage" class="btn small danger" @click="revoke(b)">Remove</button>
              </td>
            </tr>
          </tbody>
        </table>
      </div>
      <p v-else-if="bindings" class="muted">Nobody has a role at this tenant itself.</p>
    </section>

    <section>
      <header class="sub-head">
        <h3>{{ scope.solo ? 'Accounts' : `Accounts in ${tenant.name}` }}</h3>
        <span v-if="users" class="mono muted">{{ users.length }}</span>
        <div class="push tools">
          <label v-if="users && users.length">
            <span class="visually-hidden">Filter accounts</span>
            <input v-model="accountQuery" type="search" placeholder="Filter accounts" />
          </label>
          <button v-if="canManage" class="btn small" @click="open({ kind: 'add' })">Add user</button>
        </div>
      </header>
      <p class="muted note">
        <template v-if="scope.solo">People sign in with these accounts. </template>Accounts from an identity provider
        appear at their first sign-in.
      </p>
      <div v-if="matching.length" class="table-wrap accounts">
        <div :class="{ scroll: allAccounts }">
          <table class="data">
            <thead>
              <tr>
                <th>User</th>
                <th>Signs in with</th>
                <th>Roles here</th>
                <th />
              </tr>
            </thead>
            <tbody>
              <tr v-for="u in shownAccounts" :key="u.id" :class="{ disabled: u.disabled }">
                <td>
                  <strong>{{ u.display_name || u.username }}</strong>
                  <div class="muted small-text">{{ u.username }}<template v-if="u.email"> · {{ u.email }}</template></div>
                </td>
                <td>
                  <div class="tags">
                    <span>{{ u.local ? (u.two_factor ? 'Password and second factor' : 'Password') : 'Identity provider' }}</span>
                    <span v-if="u.disabled" class="tag">Disabled</span>
                  </div>
                </td>
                <td>
                  <div class="tags">
                    <span v-for="b in rolesOf(u)" :key="b.id" class="role" :class="b.role">{{ b.role }}</span>
                    <span v-if="!rolesOf(u).length" class="muted">—</span>
                  </div>
                </td>
                <td class="end">
                  <span v-if="rowError[u.id]" class="error-text">{{ rowError[u.id] }}</span>
                  <RouterLink v-if="u.id === session.me?.user_id" to="/account" class="small-link">Your account</RouterLink>
                  <div v-else-if="canManage" class="row-actions">
                    <button v-if="u.local" class="btn small" @click="open({ kind: 'password', user: u })">Set password</button>
                    <button v-if="u.two_factor" class="btn small" @click="open({ kind: 'reset', user: u })">
                      Reset second factor
                    </button>
                    <button class="btn small" @click="setDisabled(u, !u.disabled)">{{ u.disabled ? 'Enable' : 'Disable' }}</button>
                    <button class="btn small danger" @click="open({ kind: 'delete', user: u })">Delete…</button>
                  </div>
                </td>
              </tr>
            </tbody>
          </table>
        </div>
        <button v-if="matching.length > SHOWN" type="button" class="more" @click="allAccounts = !allAccounts">
          {{ allAccounts ? 'Show fewer' : `Show ${matching.length - SHOWN} more accounts` }}
        </button>
      </div>
      <p v-else-if="users" class="muted">
        {{ users.length ? 'No accounts match.' : 'No accounts belong to this tenant.' }}
      </p>
    </section>

    <ModalDialog
      v-if="dialog"
      :title="
        dialog.kind === 'grant'
          ? `Give access to ${tenant.name}`
          : dialog.kind === 'add'
            ? `Add user to ${tenant.name}`
            : dialog.kind === 'password'
              ? `Set password for ${dialog.user.username}`
              : dialog.kind === 'reset'
                ? `Reset ${dialog.user.username}’s second factor`
                : 'Delete user'
      "
      @close="dialog = null"
    >
      <form class="form" @submit.prevent="submit">
        <template v-if="dialog.kind === 'grant'">
          <label class="field">
            <span class="label">Person</span>
            <select v-model="grant.user_id" required>
              <option value="" disabled>Choose someone</option>
              <option v-for="u in candidates" :key="u.id" :value="u.id">
                {{ u.display_name || u.username }} ({{ u.username }}, {{ tenantName(u.tenant_id) }})
              </option>
            </select>
          </label>
          <fieldset class="roles">
            <legend class="label">Role</legend>
            <label v-for="r in ROLES" :key="r.id" class="role-choice">
              <input v-model="grant.role" type="radio" :value="r.id" />
              <span><strong>{{ r.id }}</strong> <span class="muted">{{ r.text }}</span></span>
            </label>
          </fieldset>
        </template>
        <template v-else-if="dialog.kind === 'add'">
          <div class="row">
            <label class="field">
              <span class="label">Username</span>
              <input v-model="add.username" required autocomplete="off" autofocus />
            </label>
            <label class="field">
              <span class="label">Full name</span>
              <input v-model="add.display_name" autocomplete="off" />
            </label>
          </div>
          <label class="field">
            <span class="label">Email</span>
            <input v-model="add.email" type="email" autocomplete="off" />
          </label>
          <label class="field">
            <span class="label">Password</span>
            <input v-model="add.password" type="password" required minlength="10" autocomplete="new-password" />
            <span class="hint">At least 10 characters. Share it with them privately.</span>
          </label>
          <fieldset class="roles">
            <legend class="label">Role at {{ tenant.name }}</legend>
            <label v-for="r in ROLES" :key="r.id" class="role-choice">
              <input v-model="add.role" type="radio" :value="r.id" />
              <span><strong>{{ r.id }}</strong> <span class="muted">{{ r.text }}</span></span>
            </label>
          </fieldset>
        </template>
        <template v-else-if="dialog.kind === 'password'">
          <label class="field">
            <span class="label">New password</span>
            <input v-model="password" type="password" required minlength="10" autocomplete="new-password" autofocus />
            <span class="hint">At least 10 characters. This signs them out everywhere.</span>
          </label>
          <label class="check">
            <input v-model="revokeTokens" type="checkbox" />
            <span>Also revoke their API tokens</span>
          </label>
          <p class="muted hint">If someone else may have got into the account. Their scripts stop working until they make new tokens.</p>
        </template>
        <template v-else-if="dialog.kind === 'reset'">
          <p>
            For someone who lost their phone or key: this removes all of
            <strong>{{ dialog.user.username }}</strong>’s second factors and backup codes and signs them out. They then
            sign in with their password alone, and can add new ones.
          </p>
          <label class="check">
            <input v-model="revokeTokens" type="checkbox" />
            <span>Also revoke their API tokens</span>
          </label>
          <p class="muted hint">If someone else may have got into the account. Their scripts stop working until they make new tokens.</p>
        </template>
        <p v-else>
          Delete <strong>{{ dialog.user.username }}</strong>? Their roles and API tokens go with the account. Their VMs
          stay.
        </p>
        <p v-if="formError" class="error-text" role="alert">{{ formError }}</p>
        <footer>
          <button type="button" class="btn" @click="dialog = null">Cancel</button>
          <button class="btn" :class="dialog.kind === 'delete' || dialog.kind === 'reset' ? 'danger' : 'primary'" :disabled="busy">
            {{
              dialog.kind === 'grant'
                ? 'Give access'
                : dialog.kind === 'add'
                  ? 'Add user'
                  : dialog.kind === 'password'
                    ? 'Set password'
                    : dialog.kind === 'reset'
                      ? 'Reset'
                      : 'Delete user'
            }}
          </button>
        </footer>
      </form>
    </ModalDialog>
  </div>
</template>

<style scoped>
.panel {
  display: grid;
  gap: 28px;
}
section {
  display: grid;
  gap: 10px;
}
.sub-head {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px 12px;
}
.push {
  margin-left: auto;
}
.tools {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
}
.tools input {
  width: 180px;
  height: 26px;
  padding: 0 8px;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
  background: var(--panel);
  color: var(--ink);
  font: 400 13px var(--font-body);
}
h3 {
  font-size: 15px;
  letter-spacing: 0.08em;
  text-transform: uppercase;
}
.note {
  margin: 0;
  font-size: 13px;
}
section > p {
  margin: 0;
}
.small-text {
  font-size: 12px;
}
.tags {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 4px 6px;
}
/* Roles as outlined tags, heavier the more they allow. */
.role {
  display: inline-block;
  padding: 1px 6px;
  border: 1px solid var(--rule);
  border-radius: 2px;
  font: 500 10.5px var(--font-label);
  letter-spacing: 0.1em;
  text-transform: uppercase;
  color: var(--muted);
  white-space: nowrap;
}
.role.admin {
  border-color: var(--ink);
  color: var(--ink);
}
.role.operator {
  border-color: var(--muted);
  color: var(--ink);
}
tr.disabled td {
  opacity: 0.6;
}
.row-actions {
  display: flex;
  flex-wrap: wrap;
  justify-content: flex-end;
  gap: 6px;
}
.row-actions .btn {
  white-space: nowrap;
}
.end .error-text {
  margin-right: 8px;
}
.small-link {
  font-size: 13px;
}
.accounts .scroll {
  max-height: 420px;
  overflow-y: auto;
}
.more {
  width: 100%;
  height: 34px;
  border: 0;
  border-top: 1px solid var(--rule);
  background: none;
  color: var(--ink);
  font: 500 12px var(--font-body);
  cursor: pointer;
}
.more:hover {
  background: var(--sunken);
}
.visually-hidden {
  position: absolute;
  width: 1px;
  height: 1px;
  overflow: hidden;
  clip: rect(0 0 0 0);
  white-space: nowrap;
}
.roles {
  display: grid;
  gap: 8px;
  margin: 0;
  padding: 0;
  border: 0;
}
.roles legend {
  margin-bottom: 6px;
}
.role-choice {
  display: flex;
  align-items: baseline;
  gap: 8px;
}
</style>
