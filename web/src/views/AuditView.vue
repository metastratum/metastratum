<script setup lang="ts">
import { ref, watch } from 'vue'
import { api, errorText, ok, type AuditEntry } from '../api/client'
import { useScope } from '../stores/scope'

const scope = useScope()
const PAGE = 100

const entries = ref<AuditEntry[] | null>(null)
const more = ref(false)
const error = ref('')
const open = ref<number | null>(null)

async function load(before?: number) {
  error.value = ''
  try {
    const page = await ok(
      api.GET('/api/v1/audit', { params: { query: { tenant_id: scope.current, limit: PAGE, before } } }),
    )
    entries.value = before ? [...(entries.value ?? []), ...page] : page
    more.value = page.length === PAGE
  } catch (e) {
    error.value = errorText(e)
  }
}
watch(
  () => scope.current,
  (id) => id && load(),
  { immediate: true },
)

const when = (at: string) =>
  new Date(at).toLocaleString(undefined, { dateStyle: 'medium', timeStyle: 'medium' })
const tenantName = (id?: string | null) => (id ? (scope.byId.get(id)?.name ?? '—') : '—')
// Sign-ins only record how; the action already says enough.
const hasDetails = (e: AuditEntry) =>
  !!e.details && Object.keys(e.details as object).some((k) => k !== 'method' || e.outcome !== 'success')
</script>

<template>
  <section>
    <header class="page-head">
      <h1>Audit log</h1>
    </header>
    <p class="intro">
      Every change and sign-in in {{ scope.tenant?.name ?? 'this tenant' }} and its sub-tenants, newest first.
    </p>

    <p v-if="error" class="error-text">{{ error }}</p>
    <div v-if="entries && !entries.length" class="empty-state">
      <p>Nothing has happened here yet.</p>
    </div>
    <div v-else-if="entries" class="table-wrap">
      <table class="data">
        <thead>
          <tr>
            <th>When</th>
            <th>Who</th>
            <th>Action</th>
            <th>Tenant</th>
            <th>Target</th>
          </tr>
        </thead>
        <tbody>
          <template v-for="e in entries" :key="e.id">
            <tr :class="{ failed: e.outcome !== 'success', clickable: hasDetails(e) }" @click="open = open === e.id ? null : e.id">
              <td class="mono when">{{ when(e.at) }}</td>
              <td>{{ e.username ?? 'system' }}</td>
              <td>
                <span class="mono">{{ e.action }}</span>
                <span v-if="e.outcome !== 'success'" class="tag fail">Failed</span>
              </td>
              <td>{{ tenantName(e.tenant_id) }}</td>
              <td class="mono muted">
                <template v-if="e.target_kind">{{ e.target_kind }} {{ e.target_id?.slice(0, 8) }}</template>
                <button
                  v-if="hasDetails(e)"
                  type="button"
                  class="more"
                  :aria-expanded="open === e.id"
                  :aria-label="`Details of ${e.action}`"
                  @click.stop="open = open === e.id ? null : e.id"
                >
                  {{ open === e.id ? '−' : '+' }}
                </button>
              </td>
            </tr>
            <tr v-if="open === e.id" class="details">
              <td colspan="5"><pre class="secret">{{ JSON.stringify(e.details, null, 2) }}</pre></td>
            </tr>
          </template>
        </tbody>
      </table>
    </div>
    <button v-if="more && entries" class="btn older" @click="load(entries[entries.length - 1]!.id)">Show older</button>
  </section>
</template>

<style scoped>
.when {
  white-space: nowrap;
}
tr.clickable {
  cursor: pointer;
}
tr.clickable:hover {
  background: var(--sunken);
}
tr.failed td:first-child {
  box-shadow: inset 3px 0 0 var(--led-fault);
}
.fail {
  margin-left: 8px;
  border-color: var(--led-fault);
  color: var(--led-fault);
}
.more {
  margin-left: 8px;
  padding: 0 6px;
  border: 1px solid var(--rule);
  border-radius: 2px;
  background: none;
  color: var(--ink);
  font: 500 12px var(--font-mono);
  cursor: pointer;
}
.details td {
  background: var(--sunken);
}
.older {
  margin-top: 14px;
}
</style>
