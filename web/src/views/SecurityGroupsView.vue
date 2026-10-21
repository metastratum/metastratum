<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { api, errorText, ok, type Rule, type SecurityGroup } from '../api/client'
import ModalDialog from '../components/ModalDialog.vue'
import RuleEditor from '../components/RuleEditor.vue'
import { ruleText } from '../state'
import { useLiveRefresh } from '../stores/live'
import { useScope } from '../stores/scope'

const scope = useScope()
const groups = ref<SecurityGroup[] | null>(null)
const error = ref('')
const adding = ref<string | null>(null)
const busy = ref(false)

async function load() {
  if (!scope.current) return
  const tenant = scope.current
  try {
    const g = await ok(api.GET('/api/v1/security-groups', { params: { query: { tenant_id: tenant } } }))
    if (tenant === scope.current) groups.value = g
  } catch (e) {
    error.value = errorText(e)
  }
}
watch(() => scope.current, load, { immediate: true })
useLiveRefresh(['security_group'], load)

const canWrite = computed(() => scope.can('network.write'))
const sorted = (g: SecurityGroup) => [...g.rules].sort((a, b) => a.direction.localeCompare(b.direction))

// Long groups show their first rules until asked for all.
const LIMIT = 5
const expanded = ref(new Set<string>())
const shown = (g: SecurityGroup) => (expanded.value.has(g.id) ? sorted(g) : sorted(g).slice(0, LIMIT))
function toggleAll(g: SecurityGroup) {
  const next = new Set(expanded.value)
  if (!next.delete(g.id)) next.add(g.id)
  expanded.value = next
}

async function addRule(g: SecurityGroup, body: Record<string, unknown>) {
  error.value = ''
  busy.value = true
  try {
    await ok(
      api.POST('/api/v1/security-groups/{id}/rules', {
        params: { path: { id: g.id } },
        body: body as never,
      }),
    )
    adding.value = null
    await load()
  } catch (e) {
    error.value = errorText(e)
  } finally {
    busy.value = false
  }
}

async function removeRule(r: Rule) {
  error.value = ''
  try {
    await ok(api.DELETE('/api/v1/security-group-rules/{id}', { params: { path: { id: r.id } } }))
    await load()
  } catch (e) {
    error.value = errorText(e)
  }
}

// New group / delete group.
const dialog = ref<{ kind: 'new' } | { kind: 'delete'; group: SecurityGroup } | null>(null)
const form = ref({ name: '', description: '' })
const formError = ref('')
async function submit() {
  const d = dialog.value
  if (!d) return
  formError.value = ''
  try {
    if (d.kind === 'new') {
      await ok(api.POST('/api/v1/security-groups', { body: { tenant_id: scope.current, ...form.value } }))
    } else {
      await ok(api.DELETE('/api/v1/security-groups/{id}', { params: { path: { id: d.group.id } } }))
    }
    dialog.value = null
    await load()
  } catch (e) {
    formError.value = errorText(e)
  }
}
</script>

<template>
  <section>
    <header class="page-head">
      <h1>Security groups</h1>
      <span v-if="groups" class="mono muted">{{ groups.length }}</span>
      <div v-if="canWrite" class="actions">
        <button class="btn primary" @click="((form = { name: '', description: '' }), (formError = ''), (dialog = { kind: 'new' }))">
          New group
        </button>
      </div>
    </header>
    <p class="intro">
      An interface passes only what its groups allow; replies always come back.
      <template v-if="!scope.solo">
        <RouterLink to="/guardrails">Guardrails</RouterLink> above {{ scope.tenant?.name ?? 'this tenant' }} can still
        block it.
      </template>
    </p>
    <p v-if="error" class="error-text" role="alert">{{ error }}</p>

    <div v-if="groups && !groups.length" class="empty-state">
      <p>No security groups yet. {{ scope.solo ? 'A default group comes with the first VM.' : 'Each tenant gets a default group with its first VM.' }}</p>
    </div>
    <div v-else-if="groups" class="groups">
      <section v-for="g in groups" :key="g.id" class="group">
        <header>
          <h2>{{ g.name }}</h2>
          <span class="muted">{{ g.members }} {{ g.members === 1 ? 'interface' : 'interfaces' }}</span>
          <div v-if="canWrite" class="buttons">
            <button class="btn small" :aria-expanded="adding === g.id" @click="adding = adding === g.id ? null : g.id">
              Add rule
            </button>
            <button
              class="btn small danger"
              :disabled="g.members > 0"
              :title="g.members > 0 ? 'VMs use this group' : undefined"
              @click="((formError = ''), (dialog = { kind: 'delete', group: g }))"
            >
              Delete
            </button>
          </div>
          <p v-if="g.description" class="muted desc">{{ g.description }}</p>
        </header>
        <ul class="rules">
          <li v-for="r in shown(g)" :key="r.id" class="rule-row">
            <span class="dir" :class="r.direction">{{ r.direction === 'ingress' ? 'IN' : 'OUT' }}</span>
            <span class="text">{{ ruleText(r) }}</span>
            <span v-if="r.description" class="muted">{{ r.description }}</span>
            <button v-if="canWrite" class="btn small" :aria-label="`Remove rule: ${ruleText(r)}`" @click="removeRule(r)">
              Remove
            </button>
          </li>
          <li v-if="!g.rules.length" class="muted rule-row">No rules: members pass no traffic but DHCP.</li>
        </ul>
        <button v-if="g.rules.length > LIMIT" type="button" class="more" @click="toggleAll(g)">
          {{ expanded.has(g.id) ? 'Show fewer' : `Show ${g.rules.length - LIMIT} more rules` }}
        </button>
        <RuleEditor
          v-if="adding === g.id"
          class="editor"
          :groups="groups"
          submit-label="Add rule"
          :busy="busy"
          @submit="addRule(g, $event)"
          @cancel="adding = null"
        />
      </section>
    </div>

    <ModalDialog v-if="dialog" :title="dialog.kind === 'new' ? 'New security group' : 'Delete security group'" @close="dialog = null">
      <form class="form" @submit.prevent="submit">
        <template v-if="dialog.kind === 'new'">
          <label class="field">
            <span class="label">Name</span>
            <input v-model="form.name" required maxlength="64" autofocus placeholder="web" />
          </label>
          <label class="field">
            <span class="label">Description</span>
            <input v-model="form.description" maxlength="256" />
          </label>
          <p class="muted">It starts empty: add rules for what its members should accept and send.</p>
        </template>
        <p v-else>Delete <strong>{{ dialog.group.name }}</strong> and its rules?</p>
        <p v-if="formError" class="error-text" role="alert">{{ formError }}</p>
        <footer>
          <button type="button" class="btn" @click="dialog = null">Cancel</button>
          <button class="btn" :class="dialog.kind === 'new' ? 'primary' : 'danger'">
            {{ dialog.kind === 'new' ? 'Create group' : 'Delete group' }}
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
.groups {
  display: grid;
  gap: 18px;
}
.group {
  border: 1px solid var(--rule);
  border-radius: var(--radius);
  background: var(--panel);
}
.group header {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 6px 12px;
  padding: 14px 16px;
}
.group h2 {
  font-size: 16px;
}
.buttons {
  display: flex;
  gap: 6px;
  margin-left: auto;
}
.desc {
  flex-basis: 100%;
  margin: 0;
}
.rules {
  margin: 0;
  padding: 0;
  list-style: none;
}
.rule-row {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 4px 12px;
  padding: 9px 16px;
  border-top: 1px solid var(--rule);
}
.rule-row .btn {
  margin-left: auto;
}
.text {
  font-weight: 500;
}
/* Direction as a small arrow-like plate: in is green, out is ink. */
.dir {
  min-width: 38px;
  padding: 1px 0;
  border-radius: 2px;
  font: 600 10.5px var(--font-label);
  letter-spacing: 0.12em;
  text-align: center;
}
.dir.ingress {
  background: color-mix(in srgb, var(--led-on) 18%, transparent);
  color: var(--led-on);
}
.dir.egress {
  background: var(--sunken);
  color: var(--muted);
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
.editor {
  margin: 0 16px 16px;
  background: var(--aisle);
}
</style>
