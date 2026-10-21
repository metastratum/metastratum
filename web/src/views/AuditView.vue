<script setup lang="ts">
import { ref, watch } from 'vue'
import type { RouteLocationRaw } from 'vue-router'
import { api, errorText, ok, type AuditEntry } from '../api/client'
import { shortId } from '../state'
import { useScope } from '../stores/scope'
import { useSession } from '../stores/session'

const scope = useScope()
const session = useSession()
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
// Where a target is shown: its own page, or the list it is on (infrastructure
// pages only for those who may see them).
const PAGES: Record<string, string> = {
  image: '/images',
  disk: '/disks',
  backup: '/backups',
  vpc: '/networks',
  network: '/networks',
  security_group: '/security-groups',
  floating_ip: '/floating-ips',
  load_balancer: '/load-balancers',
  network_policy: '/guardrails',
}
const INFRA: Record<string, string> = {
  host: '/hosts',
  cluster: '/clusters',
  storage_pool: '/storage',
  external_network: '/external-networks',
  backup_repository: '/backup-repositories',
}
function targetLink(e: AuditEntry): RouteLocationRaw | null {
  const kind = e.target_kind ?? ''
  if (!e.target_id) return null
  if (kind === 'vm') return { name: 'vm', params: { id: e.target_id } }
  if (kind === 'tenant') return { name: 'tenants', params: { id: e.target_id } }
  if (PAGES[kind]) return PAGES[kind]
  if (INFRA[kind] && session.me?.provider_admin) return INFRA[kind]
  return null
}

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
      <template v-if="scope.solo">Every change and sign-in, newest first.</template>
      <template v-else>
        Every change and sign-in in {{ scope.tenant?.name ?? 'this tenant' }} and its sub-tenants, newest first.
      </template>
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
            <th v-if="!scope.solo">Tenant</th>
            <th>Target</th>
            <th />
          </tr>
        </thead>
        <tbody>
          <template v-for="e in entries" :key="e.id">
            <tr :class="{ clickable: hasDetails(e), opened: open === e.id }" @click="hasDetails(e) && (open = open === e.id ? null : e.id)">
              <td class="mono nowrap">{{ when(e.at) }}</td>
              <td>{{ e.username ?? 'system' }}</td>
              <td class="nowrap">
                <span class="mono">{{ e.action }}</span>
                <span v-if="e.outcome !== 'success'" class="tag fail">Failed</span>
              </td>
              <td v-if="!scope.solo" class="muted">{{ tenantName(e.tenant_id) }}</td>
              <td>
                <span v-if="e.target_kind" class="target">
                  <span class="muted">{{ e.target_kind }}</span>
                  <template v-if="e.target_id">
                    <RouterLink v-if="targetLink(e)" :to="targetLink(e)!" class="mono" @click.stop>{{ shortId(e.target_id) }}</RouterLink>
                    <span v-else class="mono">{{ shortId(e.target_id) }}</span>
                  </template>
                </span>
              </td>
              <td class="end">
                <button
                  v-if="hasDetails(e)"
                  type="button"
                  class="chevron"
                  :aria-expanded="open === e.id"
                  :aria-label="`Details of ${e.action}`"
                  @click.stop="open = open === e.id ? null : e.id"
                >
                  <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true">
                    <path d="M3 4.5 6 7.5 9 4.5" fill="none" stroke="currentColor" stroke-width="1.5" />
                  </svg>
                </button>
              </td>
            </tr>
            <tr v-if="open === e.id" class="details">
              <td :colspan="scope.solo ? 5 : 6"><pre>{{ JSON.stringify(e.details, null, 2) }}</pre></td>
            </tr>
          </template>
        </tbody>
      </table>
    </div>
    <button v-if="more && entries" class="btn older" title="Loads the next 100" @click="load(entries[entries.length - 1]!.id)">
      Show older
    </button>
  </section>
</template>

<style scoped>
.nowrap {
  white-space: nowrap;
}
tr.clickable {
  cursor: pointer;
}
tr.clickable:hover td {
  background: var(--sunken);
}
/* An entry and its details read as one. */
tr.opened td {
  border-bottom: 0;
}
.fail {
  margin-left: 8px;
  border-color: var(--led-fault);
  color: var(--led-fault);
}
.target {
  display: inline-flex;
  align-items: center;
  gap: 6px;
}
.chevron {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 26px;
  height: 26px;
  padding: 0;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
  background: var(--panel);
  color: var(--ink);
  cursor: pointer;
}
.chevron[aria-expanded='true'] svg {
  transform: rotate(180deg);
}
.details td {
  padding: 0 14px 12px;
}
.details pre {
  margin: 0;
  padding: 12px;
  border-radius: var(--radius);
  background: var(--sunken);
  font: 400 12px/1.5 var(--font-mono);
  white-space: pre-wrap;
  word-break: break-all;
}
.older {
  margin-top: 18px;
}
</style>
