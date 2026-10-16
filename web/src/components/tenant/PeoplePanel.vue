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
function open(d: Dialog) {
  formError.value = ''
  password.value = ''
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
      await ok(api.PATCH('/api/v1/users/{id}', { params: { path: { id: d.user.id } }, body: { password: password.value } }))
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
        <h3>Roles at {{ tenant.name }}</h3>
        <button v-if="canManage" class="btn small primary" @click="openGrant">Give access</button>
      </header>
      <p class="muted note">
        These apply here and in every sub-tenant. People with a role at a tenant above this one have access too.
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
                <span class="role" :class="b.role">{{ b.role }}</span>
                <span v-if="b.source === 'oidc'" class="tag" title="From the identity provider’s groups; set again at each sign-in">
                  From groups
                </span>
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
        <h3>Accounts in {{ tenant.name }}</h3>
        <button v-if="canManage" class="btn small" @click="open({ kind: 'add' })">Add user</button>
      </header>
      <p class="muted note">
        People sign in to this tenant with these accounts. Accounts from an identity provider are created at their
        first sign-in.
      </p>
      <div v-if="users && users.length" class="table-wrap">
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
            <tr v-for="u in users" :key="u.id" :class="{ disabled: u.disabled }">
              <td>
                <strong>{{ u.display_name || u.username }}</strong>
                <div class="muted small-text">{{ u.username }}<template v-if="u.email"> · {{ u.email }}</template></div>
              </td>
              <td>
                {{ u.local ? 'Password' : 'Identity provider' }}
                <span v-if="u.disabled" class="tag">Disabled</span>
              </td>
              <td>
                <span v-for="b in rolesOf(u)" :key="b.id" class="role" :class="b.role">{{ b.role }}</span>
                <span v-if="!rolesOf(u).length" class="muted">—</span>
              </td>
              <td class="end">
                <span v-if="rowError[u.id]" class="error-text">{{ rowError[u.id] }}</span>
                <template v-if="canManage && u.id !== session.me?.user_id">
                  <button v-if="u.local" class="btn small" @click="open({ kind: 'password', user: u })">Set password</button>
                  <button class="btn small" @click="setDisabled(u, !u.disabled)">{{ u.disabled ? 'Enable' : 'Disable' }}</button>
                  <button class="btn small danger" @click="open({ kind: 'delete', user: u })">Delete</button>
                </template>
              </td>
            </tr>
          </tbody>
        </table>
      </div>
      <p v-else-if="users" class="muted">No accounts belong to this tenant.</p>
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
        </template>
        <p v-else>
          Delete <strong>{{ dialog.user.username }}</strong>? Their roles and API tokens go with the account. Their VMs
          stay.
        </p>
        <p v-if="formError" class="error-text" role="alert">{{ formError }}</p>
        <footer>
          <button type="button" class="btn" @click="dialog = null">Cancel</button>
          <button class="btn" :class="dialog.kind === 'delete' ? 'danger' : 'primary'" :disabled="busy">
            {{
              dialog.kind === 'grant'
                ? 'Give access'
                : dialog.kind === 'add'
                  ? 'Add user'
                  : dialog.kind === 'password'
                    ? 'Set password'
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
  align-items: center;
  justify-content: space-between;
  gap: 12px;
}
h3 {
  font-size: 13px;
  letter-spacing: 0.08em;
  text-transform: uppercase;
}
.note {
  margin: -4px 0 4px;
  font-size: 13px;
}
section > p {
  margin: 0;
}
.small-text {
  font-size: 12px;
}
.role {
  display: inline-block;
  margin-right: 6px;
  padding: 1px 7px;
  border-radius: 2px;
  font: 600 11px var(--font-label);
  letter-spacing: 0.1em;
  text-transform: uppercase;
  background: var(--sunken);
}
/* Admin is the role that changes the most; it wears the tape. */
.role.admin {
  background: var(--tape);
  color: var(--tape-ink);
}
.role.viewer {
  background: none;
  box-shadow: inset 0 0 0 1px var(--rule);
  color: var(--muted);
}
tr.disabled td {
  color: var(--muted);
}
.end .btn + .btn {
  margin-left: 6px;
}
.end .error-text {
  margin-right: 8px;
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
