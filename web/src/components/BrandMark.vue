<script setup lang="ts">
/** The metastratum mark and name: three cut-end strips stacked as layers,
 *  the label-tape one on top being the platform you operate. `bar` sets it
 *  on the top bar, which is dark in either theme. */
defineProps<{ bar?: boolean }>()
</script>

<template>
  <span class="brand" :class="{ bar }">
    <svg class="mark" viewBox="0 0 48 48" aria-hidden="true">
      <polygon class="top" points="10,9 42,9 38,17 6,17" />
      <polygon class="mid" points="10,20 42,20 38,28 6,28" />
      <polygon class="low" points="10,31 42,31 38,39 6,39" />
    </svg>
    <span class="name">metastratum</span>
  </span>
</template>

<style scoped>
.brand {
  --brand-mid: var(--ink);
  --brand-low: #8e99a2;
  display: inline-flex;
  align-items: center;
  gap: 10px;
  color: var(--ink);
}
@media (prefers-color-scheme: dark) {
  .brand {
    --brand-low: #5d6974;
  }
}
.brand.bar {
  --brand-mid: var(--bar-ink);
  --brand-low: #5d6974;
  color: var(--bar-ink);
}
.mark {
  width: 32px;
  height: 32px;
  flex: none;
}
.bar .mark {
  width: 26px;
  height: 26px;
}
.top {
  fill: var(--tape);
}
.mid {
  fill: var(--brand-mid);
}
.low {
  fill: var(--brand-low);
}
.name {
  font: 600 16px var(--font-label);
  letter-spacing: 0.14em;
  text-transform: uppercase;
}
.bar .name {
  font-size: 14px;
}
</style>
