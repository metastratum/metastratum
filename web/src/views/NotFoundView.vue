<template>
  <section class="nf">
    <h1>Nothing at this address</h1>
    <p>The page may have moved, or the resource was deleted.</p>
    <RouterLink to="/vms" class="btn">Go to machines</RouterLink>
  </section>
</template>

<style scoped>
.nf {
  display: grid;
  gap: 12px;
  justify-items: start;
  padding-top: 48px;
}
p {
  margin: 0;
  color: var(--muted);
}
</style>
