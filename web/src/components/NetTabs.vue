<script setup lang="ts">
import { useScope } from '../stores/scope'

const scope = useScope()
</script>

<template>
  <nav class="tabs" aria-label="Networking">
    <RouterLink to="/networks">VPCs</RouterLink>
    <RouterLink to="/security-groups">Security groups</RouterLink>
    <RouterLink to="/floating-ips">Floating IPs</RouterLink>
    <RouterLink to="/load-balancers">Load balancers</RouterLink>
    <RouterLink v-if="!scope.solo" to="/guardrails">Guardrails</RouterLink>
  </nav>
</template>

<style scoped>
.tabs {
  display: flex;
  gap: 4px;
  margin-bottom: 20px;
  overflow-x: auto;
  border-bottom: 1px solid var(--rule);
}
a {
  padding: 8px 12px;
  margin-bottom: -1px;
  border-bottom: 2px solid transparent;
  color: var(--muted);
  font: 500 12px var(--font-label);
  letter-spacing: 0.12em;
  text-transform: uppercase;
  text-decoration: none;
  white-space: nowrap;
}
a:hover {
  color: var(--ink);
}
a.router-link-active {
  border-bottom-color: var(--tape);
  color: var(--ink);
}
</style>
