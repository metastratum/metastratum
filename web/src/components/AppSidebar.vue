<script setup lang="ts">
import { computed } from 'vue'
import { useRoute } from 'vue-router'
import BrandMark from './BrandMark.vue'
import TenantPicker from './TenantPicker.vue'
import { useScope } from '../stores/scope'
import { useSession } from '../stores/session'
import { useShell } from '../stores/shell'

defineEmits<{ signOut: [] }>()

const route = useRoute()
const scope = useScope()
const session = useSession()
const shell = useShell()

type Link = { to: string; label: string; also?: string[]; count?: number | null }
const groups = computed(() => {
  const out: { title: string; links: Link[]; infra?: boolean }[] = [
    {
      title: 'Compute',
      links: [
        { to: '/vms', label: 'Machines', also: ['vm'], count: shell.vmCount },
        { to: '/disks', label: 'Disks' },
        { to: '/backups', label: 'Backups' },
        { to: '/images', label: 'Images' },
      ],
    },
    {
      title: 'Networking',
      links: [
        { to: '/networks', label: 'VPCs' },
        { to: '/security-groups', label: 'Security groups' },
        { to: '/floating-ips', label: 'Floating IPs' },
        { to: '/load-balancers', label: 'Load balancers' },
        // Guardrails bind sub-tenants: with none, there are none to set.
        ...(scope.solo ? [] : [{ to: '/guardrails', label: 'Guardrails' }]),
      ],
    },
    {
      title: 'Organization',
      links: [
        { to: '/tenants', label: scope.solo ? 'People' : 'Tenants' },
        ...(scope.can('audit.read') ? [{ to: '/audit', label: 'Audit log' }] : []),
      ],
    },
  ]
  if (session.me?.provider_admin) {
    out.push({
      title: 'Infrastructure',
      infra: true,
      links: [
        { to: '/hosts', label: 'Hosts & clusters', also: ['clusters'] },
        { to: '/storage', label: 'Storage' },
        { to: '/external-networks', label: 'External networks' },
        { to: '/backup-repositories', label: 'Backup repositories' },
      ],
    })
  }
  return out
})

/** The link for this page, or the list its record belongs to. */
function current(l: Link): boolean {
  return route.path === l.to || route.path.startsWith(`${l.to}/`) || (l.also ?? []).includes(String(route.name))
}
</script>

<template>
  <aside class="sidebar">
    <RouterLink to="/vms" class="mark" aria-label="metastratum home"><BrandMark bar /></RouterLink>
    <TenantPicker />
    <nav aria-label="Main">
      <div v-for="g in groups" :key="g.title" class="group" :class="{ infra: g.infra }">
        <span class="label">{{ g.title }}</span>
        <RouterLink
          v-for="l in g.links"
          :key="l.to"
          :to="l.to"
          :aria-current="current(l) ? 'page' : undefined"
          :class="{ current: current(l) }"
        >
          <span>{{ l.label }}</span>
          <span v-if="current(l) && l.count != null" class="count">{{ l.count }}</span>
        </RouterLink>
      </div>
    </nav>
    <div v-if="session.me" class="me">
      <RouterLink to="/account" :title="`Signed in as ${session.me.username}`">
        {{ session.me.display_name || session.me.username }}
      </RouterLink>
      <button type="button" @click="$emit('signOut')">Sign out</button>
    </div>
  </aside>
</template>

<style scoped>
.sidebar {
  display: flex;
  flex-direction: column;
  gap: 22px;
  padding: 16px 12px 20px;
  background: var(--bar);
  color: var(--bar-ink);
}
.mark {
  padding: 2px 8px;
  text-decoration: none;
}
nav {
  display: flex;
  flex-direction: column;
  gap: 18px;
}
.group {
  display: flex;
  flex-direction: column;
  gap: 2px;
}
.group.infra {
  padding-top: 14px;
  border-top: 1px solid rgb(227 232 236 / 12%);
}
.group .label {
  padding: 0 10px 4px;
  color: rgb(227 232 236 / 55%);
}
.group a {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  height: 32px;
  padding: 0 10px;
  border-radius: var(--radius);
  color: rgb(227 232 236 / 68%);
  font-weight: 500;
  font-size: 13px;
  text-decoration: none;
}
.group a:hover {
  background: rgb(227 232 236 / 6%);
  color: var(--bar-ink);
}
.group a.current {
  background: rgb(227 232 236 / 10%);
  color: #fff;
}
.count {
  font: 400 11px var(--font-mono);
  color: var(--tape);
}
.me {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  margin-top: auto;
  padding: 12px 10px 0;
  border-top: 1px solid rgb(227 232 236 / 12%);
}
.me a {
  overflow: hidden;
  color: var(--bar-ink);
  font-weight: 500;
  font-size: 13px;
  text-decoration: none;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.me a:hover,
.me a.router-link-active {
  text-decoration: underline;
  text-underline-offset: 4px;
}
.me button {
  flex: none;
  padding: 0;
  border: 0;
  background: none;
  color: rgb(227 232 236 / 68%);
  font: 400 13px var(--font-body);
  cursor: pointer;
}
.me button:hover {
  color: var(--bar-ink);
}
</style>
