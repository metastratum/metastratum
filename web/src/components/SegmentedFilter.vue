<script setup lang="ts">
/** A few mutually exclusive filters, each with how many it would show. */
defineProps<{ options: { id: string; label: string; count?: number }[]; label: string }>()
const model = defineModel<string>({ required: true })
</script>

<template>
  <div class="segmented" role="group" :aria-label="label">
    <button
      v-for="o in options"
      :key="o.id"
      type="button"
      :aria-pressed="model === o.id"
      :class="{ on: model === o.id }"
      @click="model = o.id"
    >
      {{ o.label }} <span v-if="o.count != null" class="mono">{{ o.count }}</span>
    </button>
  </div>
</template>

<style scoped>
.segmented {
  display: flex;
  flex-wrap: wrap;
  gap: 2px;
  padding: 3px;
  border-radius: var(--radius);
  background: var(--sunken);
}
button {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  height: 26px;
  padding: 0 10px;
  border: 0;
  border-radius: 2px;
  background: transparent;
  color: var(--muted);
  font: 500 13px var(--font-body);
  cursor: pointer;
}
button:hover {
  color: var(--ink);
}
button.on {
  background: var(--panel);
  color: var(--ink);
  box-shadow: 0 1px 0 rgb(28 35 41 / 6%);
}
.mono {
  font-size: 11px;
  color: var(--muted);
}
</style>
