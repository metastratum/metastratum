<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { useRouter } from 'vue-router'
import { api, errorText, ok, type Tenant } from '../api/client'
import IdChip from '../components/IdChip.vue'
import ModalDialog from '../components/ModalDialog.vue'
import GrantsPanel from '../components/tenant/GrantsPanel.vue'
import PeoplePanel from '../components/tenant/PeoplePanel.vue'
import QuotaPanel from '../components/tenant/QuotaPanel.vue'
import SignInPanel from '../components/tenant/SignInPanel.vue'
import { ago } from '../state'
import { useScope } from '../stores/scope'

const props = defineProps<{ id?: string }>()
const scope = useScope()
const router = useRouter()

const selected = computed<Tenant | undefined>(() => scope.byId.get(props.id || scope.current) ?? scope.tenants[0])
const parent = computed(() => (selected.value?.parent_id ? scope.byId.get(selected.value.parent_id) : undefined))
const top = computed(() => Math.min(...scope.tenants.map((t) => t.depth)))

const TABS = [
  { id: 'quotas', label: 'Quotas' },
  { id: 'people', label: 'People' },
  { id: 'resources', label: 'Resources' },
  { id: 'sign-in', label: 'Sign-in' },
] as const
type Tab = (typeof TABS)[number]['id']
const tab = ref<Tab>('quotas')

// Which tree rows have a later sibling, for drawing the connector lines.
const hasNextSibling = computed(() => {
  const out = new Set<string>()
  const seen = new Set<string | null | undefined>()
  for (const t of [...scope.tenants].reverse()) {
    if (seen.has(t.parent_id)) out.add(t.id)
    seen.add(t.parent_id)
  }
  return out
})
/** For each ancestor level of a row: whether its line continues past it. */
function rails(t: Tenant): boolean[] {
  const out: boolean[] = []
  for (let a = t.parent_id ? scope.byId.get(t.parent_id) : undefined; a && a.depth > top.value; ) {
    out.unshift(hasNextSibling.value.has(a.id))
    a = a.parent_id ? scope.byId.get(a.parent_id) : undefined
  }
  return out
}

function open(t: Tenant) {
  void router.replace({ name: 'tenants', params: { id: t.id } })
}

// Creating, renaming and deleting tenants.
const dialog = ref<'create' | 'rename' | 'delete' | null>(null)
const form = ref({ name: '', slug: '' })
const error = ref('')
const busy = ref(false)
const slugOk = computed(() => /^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$/.test(form.value.slug))
watch(
  () => form.value.name,
  (name, old) => {
    // Suggest a slug from the name until the user edits it.
    const fromOld = old.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '')
    if (dialog.value === 'create' && (form.value.slug === '' || form.value.slug === fromOld)) {
      form.value.slug = name.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '').slice(0, 63)
    }
  },
)
function start(kind: 'create' | 'rename' | 'delete') {
  error.value = ''
  form.value = { name: kind === 'rename' ? (selected.value?.name ?? '') : '', slug: '' }
  dialog.value = kind
}
async function submit() {
  const t = selected.value
  if (!t) return
  error.value = ''
  busy.value = true
  try {
    if (dialog.value === 'create') {
      const created = await ok(api.POST('/api/v1/tenants', { body: { parent_id: t.id, ...form.value } }))
      await scope.load()
      open(created)
    } else if (dialog.value === 'rename') {
      await ok(api.PATCH('/api/v1/tenants/{id}', { params: { path: { id: t.id } }, body: { name: form.value.name } }))
      await scope.load()
    } else if (dialog.value === 'delete') {
      await ok(api.DELETE('/api/v1/tenants/{id}', { params: { path: { id: t.id } } }))
      if (scope.current === t.id && t.parent_id) scope.select(t.parent_id)
      await scope.load()
      if (parent.value) open(parent.value)
    }
    dialog.value = null
  } catch (e) {
    error.value = errorText(e)
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <section>
    <header class="page-head">
      <h1>Tenants</h1>
    </header>
    <p class="intro">
      Each tenant’s quotas, people and resources apply to everything below it. A role at a tenant covers its
      sub-tenants too.
    </p>

    <div class="layout">
      <nav class="tree" aria-label="Tenants">
        <ul>
          <li v-for="t in scope.tenants" :key="t.id">
            <button
              type="button"
              class="node"
              :class="{ on: selected?.id === t.id }"
              :aria-current="selected?.id === t.id ? 'true' : undefined"
              @click="open(t)"
            >
              <span v-for="(more, i) in rails(t)" :key="i" class="rail" :class="{ more }" aria-hidden="true" />
              <span v-if="t.depth > top" class="elbow" :class="{ more: hasNextSibling.has(t.id) }" aria-hidden="true" />
              <span class="node-name">{{ t.name }}</span>
            </button>
          </li>
        </ul>
      </nav>

      <article v-if="selected" class="detail">
        <header class="detail-head">
          <div class="title">
            <h2>{{ selected.name }}</h2>
            <span class="path mono">/{{ selected.slug_path }}</span>
          </div>
          <div class="meta">
            <IdChip :value="selected.id" short />
            <span class="muted">Created {{ ago(selected.created_at) }}</span>
            <span v-if="scope.roleAt(selected.id)" class="tag">You: {{ scope.roleAt(selected.id) }}</span>
          </div>
          <div class="actions">
            <button v-if="scope.can('tenant.write', selected.id)" class="btn small" @click="start('rename')">Rename</button>
            <button v-if="scope.can('tenant.write', selected.id)" class="btn small primary" @click="start('create')">
              Add sub-tenant
            </button>
            <button v-if="parent && scope.can('tenant.write', parent.id)" class="btn small danger" @click="start('delete')">
              Delete
            </button>
          </div>
        </header>

        <div class="tabs" role="tablist">
          <button
            v-for="t in TABS"
            :key="t.id"
            type="button"
            role="tab"
            :aria-selected="tab === t.id"
            :class="{ on: tab === t.id }"
            @click="tab = t.id"
          >
            {{ t.label }}
          </button>
        </div>
        <div class="tab-body" role="tabpanel">
          <QuotaPanel v-if="tab === 'quotas'" :key="`q${selected.id}`" :tenant="selected" :parent="parent" />
          <PeoplePanel v-else-if="tab === 'people'" :key="`p${selected.id}`" :tenant="selected" />
          <GrantsPanel v-else-if="tab === 'resources'" :key="`g${selected.id}`" :tenant="selected" :parent="parent" />
          <SignInPanel v-else :key="`s${selected.id}`" :tenant="selected" />
        </div>
      </article>
    </div>

    <ModalDialog
      v-if="dialog && selected"
      :title="dialog === 'create' ? `New tenant in ${selected.name}` : dialog === 'rename' ? 'Rename tenant' : 'Delete tenant'"
      @close="dialog = null"
    >
      <form class="form" @submit.prevent="submit">
        <template v-if="dialog === 'delete'">
          <p>
            Delete <strong>{{ selected.name }}</strong>? Its users, images and settings go with it. A tenant that still
            has VMs or sub-tenants can’t be deleted.
          </p>
        </template>
        <template v-else>
          <label class="field">
            <span class="label">Name</span>
            <input v-model="form.name" required maxlength="128" autofocus />
          </label>
          <label v-if="dialog === 'create'" class="field">
            <span class="label">Slug</span>
            <input v-model="form.slug" required maxlength="63" class="mono" />
            <span class="hint" :class="{ 'error-text': form.slug && !slugOk }">
              Lowercase letters, digits and hyphens. Users sign in to
              <span class="mono">{{ [selected.slug_path, form.slug || '…'].filter(Boolean).join('/') }}</span>; it can’t be
              changed later.
            </span>
          </label>
        </template>
        <p v-if="error" class="error-text" role="alert">{{ error }}</p>
        <footer>
          <button type="button" class="btn" @click="dialog = null">Cancel</button>
          <button
            class="btn"
            :class="dialog === 'delete' ? 'danger' : 'primary'"
            :disabled="busy || (dialog === 'create' && !slugOk)"
          >
            {{ dialog === 'create' ? 'Create tenant' : dialog === 'rename' ? 'Rename' : 'Delete tenant' }}
          </button>
        </footer>
      </form>
    </ModalDialog>
  </section>
</template>

<style scoped>
.layout {
  display: grid;
  grid-template-columns: 260px 1fr;
  gap: 20px;
  align-items: start;
}
.tree {
  position: sticky;
  top: 72px;
  max-height: calc(100vh - 96px);
  overflow: auto;
  padding: 8px;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
  background: var(--panel);
}
.tree ul {
  margin: 0;
  padding: 0;
  list-style: none;
}
.node {
  display: flex;
  align-items: stretch;
  width: 100%;
  min-height: 32px;
  padding: 0 8px;
  border: 0;
  border-radius: 2px;
  background: none;
  color: var(--ink);
  font: 400 14px var(--font-body);
  text-align: left;
  cursor: pointer;
}
.node:hover {
  background: var(--sunken);
}
.node.on {
  background: var(--tape);
  color: var(--tape-ink);
  font-weight: 600;
}
/* Connector lines, like the output of `tree`. */
.rail,
.elbow {
  position: relative;
  flex: none;
  width: 18px;
}
.rail.more::before,
.elbow::before {
  content: '';
  position: absolute;
  top: 0;
  bottom: 0;
  left: 6px;
  border-left: 1px solid var(--muted);
  opacity: 0.55;
}
.elbow:not(.more)::before {
  bottom: 50%;
}
.elbow::after {
  content: '';
  position: absolute;
  top: 50%;
  left: 6px;
  width: 9px;
  border-top: 1px solid var(--muted);
  opacity: 0.55;
}
.node.on .rail::before,
.node.on .elbow::before,
.node.on .elbow::after {
  border-color: var(--tape-ink);
}
.node-name {
  align-self: center;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.detail {
  min-width: 0;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
  background: var(--panel);
}
.detail-head {
  display: grid;
  grid-template-columns: 1fr auto;
  gap: 8px 16px;
  padding: 18px 20px 14px;
}
.title {
  display: flex;
  flex-wrap: wrap;
  align-items: baseline;
  gap: 12px;
}
.title h2 {
  font-size: 20px;
}
.path {
  color: var(--muted);
}
.meta {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 10px;
  grid-column: 1;
}
.actions {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
  grid-row: 1 / span 2;
  grid-column: 2;
  align-self: start;
  justify-content: flex-end;
}
.tabs {
  display: flex;
  gap: 2px;
  padding: 0 14px;
  overflow-x: auto;
  border-bottom: 1px solid var(--rule);
}
.tabs button {
  padding: 10px 10px 8px;
  border: 0;
  border-bottom: 2px solid transparent;
  background: none;
  color: var(--muted);
  font: 500 12px var(--font-label);
  letter-spacing: 0.12em;
  text-transform: uppercase;
  cursor: pointer;
}
.tabs button:hover {
  color: var(--ink);
}
.tabs button.on {
  border-bottom-color: var(--tape);
  color: var(--ink);
}
.tab-body {
  padding: 20px;
}
@media (max-width: 860px) {
  .layout {
    grid-template-columns: 1fr;
  }
  .tree {
    position: static;
    max-height: 260px;
  }
  .detail-head {
    grid-template-columns: 1fr;
  }
  .actions {
    grid-row: auto;
    grid-column: 1;
    justify-content: flex-start;
  }
  .tab-body {
    padding: 16px;
  }
}
</style>
