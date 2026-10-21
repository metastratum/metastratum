<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { useRouter } from 'vue-router'
import { api, errorText, ok, type Tenant } from '../api/client'
import IdChip from '../components/IdChip.vue'
import ModalDialog from '../components/ModalDialog.vue'
import GrantsPanel from '../components/tenant/GrantsPanel.vue'
import PeoplePanel from '../components/tenant/PeoplePanel.vue'
import QuotaPanel from '../components/tenant/QuotaPanel.vue'
import DomainsPanel from '../components/tenant/DomainsPanel.vue'
import SignInPanel from '../components/tenant/SignInPanel.vue'
import { ago } from '../state'
import { useScope } from '../stores/scope'

const props = defineProps<{ id?: string }>()
const scope = useScope()
const router = useRouter()

const selected = computed<Tenant | undefined>(() => scope.byId.get(props.id || scope.current) ?? scope.tenants[0])
const parent = computed(() => (selected.value?.parent_id ? scope.byId.get(selected.value.parent_id) : undefined))
const domainsKey = computed(() => scope.tenants.flatMap((t) => t.domains).join())
const top = computed(() => Math.min(...scope.tenants.map((t) => t.depth)))

const TABS = [
  { id: 'quotas', label: 'Quotas' },
  { id: 'people', label: 'People' },
  { id: 'resources', label: 'Resources' },
  { id: 'sign-in', label: 'Sign-in' },
] as const
type Tab = (typeof TABS)[number]['id']
const chosen = ref<Tab>('quotas')
// With the provider alone there are no quotas or grants to give: people and
// sign-in remain.
const tabs = computed(() => (scope.solo ? TABS.filter((t) => t.id === 'people' || t.id === 'sign-in') : TABS))
const tab = computed<Tab>({
  get: () => (tabs.value.some((t) => t.id === chosen.value) ? chosen.value : tabs.value[0]!.id),
  set: (t) => (chosen.value = t),
})

// The tree, filtered by name; rows are indented by depth.
const treeQuery = ref('')
const shownTree = computed(() => {
  const q = treeQuery.value.trim().toLowerCase()
  return scope.tenants.filter((t) => !q || t.name.toLowerCase().includes(q))
})

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
      <h1>{{ scope.solo ? 'People' : 'Tenants' }}</h1>
      <span v-if="!scope.solo" class="mono muted">{{ scope.tenants.length }}</span>
      <div v-if="scope.solo && selected && scope.can('tenant.write', selected.id)" class="actions">
        <button class="btn" @click="start('create')">Add tenant</button>
      </div>
    </header>
    <p v-if="scope.solo" class="intro">
      Who can sign in and what they may do. To give teams or customers a share of their own, with quotas and their own
      people, add a tenant.
    </p>
    <p v-else class="intro">
      Each tenant’s quotas, people and resources apply to everything below it. A role at a tenant covers its
      sub-tenants too.
    </p>

    <div class="layout">
      <nav v-if="!scope.solo" class="tree" aria-label="Tenants">
        <label>
          <span class="visually-hidden">Filter tenants</span>
          <input v-model="treeQuery" type="search" placeholder="Filter tenants" />
        </label>
        <ul>
          <li v-for="t in shownTree" :key="t.id">
            <button
              type="button"
              class="node"
              :class="{ on: selected?.id === t.id }"
              :style="{ paddingLeft: `${10 + (t.depth - top) * 16}px` }"
              :aria-current="selected?.id === t.id ? 'true' : undefined"
              @click="open(t)"
            >
              <span v-if="t.depth > top" class="elbow" aria-hidden="true" />
              <span class="node-name">{{ t.name }}</span>
            </button>
          </li>
        </ul>
        <p v-if="!shownTree.length" class="muted none">No tenants match.</p>
      </nav>

      <article v-if="selected" class="detail">
        <header v-if="!scope.solo" class="detail-head">
          <div class="who">
            <div class="title">
              <h2>{{ selected.name }}</h2>
              <span class="path mono">/{{ selected.slug_path }}</span>
            </div>
            <div class="meta">
              <IdChip :value="selected.id" short />
              <span class="muted">Created {{ ago(selected.created_at) }}</span>
              <span v-if="scope.roleAt(selected.id)" class="tag">You: {{ scope.roleAt(selected.id) }}</span>
            </div>
          </div>
          <div class="actions">
            <button v-if="scope.can('tenant.write', selected.id)" class="btn" @click="start('rename')">Rename</button>
            <button v-if="parent && scope.can('tenant.write', parent.id)" class="btn danger" @click="start('delete')">
              Delete…
            </button>
            <button v-if="scope.can('tenant.write', selected.id)" class="btn primary" @click="start('create')">
              Add sub-tenant
            </button>
          </div>
        </header>

        <div class="tabs" role="tablist">
          <button
            v-for="t in tabs"
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
          <div v-else class="sign-in">
            <!-- A domain bound here or above changes where people sign in. -->
            <SignInPanel :key="`s${selected.id}:${domainsKey}`" :tenant="selected" />
            <DomainsPanel v-if="!scope.solo" :key="`d${selected.id}`" :tenant="selected" />
          </div>
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
.page-head {
  margin-bottom: 4px;
}
/* The tree beside the tenant; the tenant drops below it when narrow. */
.layout {
  display: flex;
  flex-wrap: wrap;
  align-items: flex-start;
  gap: 20px;
}
.tree {
  display: grid;
  flex: 1 1 240px;
  gap: 8px;
  max-width: 100%;
  padding: 10px;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
  background: var(--panel);
}
.tree input {
  width: 100%;
  height: 32px;
  padding: 0 10px;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
  background: var(--panel);
  color: var(--ink);
  font: 400 13px var(--font-body);
}
.tree ul {
  display: grid;
  gap: 2px;
  max-height: 520px;
  margin: 0;
  padding: 0;
  overflow-y: auto;
  list-style: none;
}
.node {
  display: flex;
  align-items: center;
  gap: 6px;
  width: 100%;
  min-height: 32px;
  padding: 4px 10px;
  border: 0;
  border-radius: var(--radius);
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
/* Where a sub-tenant hangs off its parent. */
.elbow {
  flex: none;
  width: 9px;
  height: 10px;
  margin-top: -6px;
  border-bottom: 1px solid var(--led-off);
  border-left: 1px solid var(--led-off);
}
.node.on .elbow {
  border-color: var(--tape-ink);
}
.node-name {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.none {
  margin: 0;
  padding: 4px 10px;
}
.detail {
  flex: 999 1 560px;
  min-width: 0;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
  background: var(--panel);
}
.detail-head {
  display: flex;
  flex-wrap: wrap;
  align-items: flex-start;
  gap: 10px 16px;
  padding: 18px 20px 14px;
}
.who {
  display: grid;
  gap: 6px;
}
.title {
  display: flex;
  flex-wrap: wrap;
  align-items: baseline;
  gap: 10px;
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
  gap: 8px 12px;
}
.actions {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
  margin-left: auto;
}
.actions .btn {
  height: 30px;
  padding: 0 10px;
}
.tabs {
  display: flex;
  gap: 2px;
  padding: 0 20px;
  overflow-x: auto;
  border-bottom: 1px solid var(--rule);
}
.tabs button {
  margin-bottom: -1px;
  padding: 10px 12px;
  border: 0;
  border-bottom: 2px solid transparent;
  background: none;
  color: var(--muted);
  font: 500 12px var(--font-label);
  letter-spacing: 0.12em;
  text-transform: uppercase;
  white-space: nowrap;
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
  padding: 18px 20px 22px;
}
.sign-in {
  display: grid;
  gap: 18px;
}
.visually-hidden {
  position: absolute;
  width: 1px;
  height: 1px;
  overflow: hidden;
  clip: rect(0 0 0 0);
  white-space: nowrap;
}
@media (max-width: 760px) {
  .tree ul {
    max-height: 260px;
  }
  .tab-body {
    padding: 16px;
  }
}
</style>
