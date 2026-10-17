<template>
  <nav class="infra" aria-label="Infrastructure">
    <RouterLink to="/hosts">Hosts</RouterLink>
    <RouterLink to="/clusters">Clusters</RouterLink>
    <RouterLink to="/storage">Storage</RouterLink>
    <RouterLink to="/external-networks">External networks</RouterLink>
    <RouterLink to="/backup-repositories">Backups</RouterLink>
  </nav>
</template>

<style scoped>
.infra {
  display: flex;
  gap: 4px;
  margin-bottom: 20px;
  border-bottom: 1px solid var(--rule);
}
a {
  padding: 8px 12px;
  border-bottom: 2px solid transparent;
  margin-bottom: -1px;
  color: var(--muted);
  font: 500 12px var(--font-label);
  letter-spacing: 0.12em;
  text-transform: uppercase;
  text-decoration: none;
}
a:hover {
  color: var(--ink);
}
a.router-link-active {
  border-bottom-color: var(--tape);
  color: var(--ink);
}
</style>
