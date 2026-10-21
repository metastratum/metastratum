<script setup lang="ts">
import { computed } from 'vue'
import { useScope } from '../stores/scope'

const scope = useScope()
const top = computed(() => Math.min(...scope.tenants.map((t) => t.depth)))
// Native options can't be styled, so the tree is drawn with indentation.
const indent = (depth: number) => (depth > top.value ? '  '.repeat(depth - top.value - 1) + '└ ' : '')
</script>

<template>
  <label v-if="scope.tenants.length > 1" class="picker">
    <span class="label">Tenant scope</span>
    <select :value="scope.current" @change="scope.select(($event.target as HTMLSelectElement).value)">
      <option v-for="t in scope.tenants" :key="t.id" :value="t.id">{{ indent(t.depth) }}{{ t.name }}</option>
    </select>
  </label>
</template>

<style scoped>
.picker {
  display: grid;
  gap: 6px;
  padding: 0 8px;
}
.label {
  color: rgb(227 232 236 / 55%);
}
select {
  width: 100%;
  height: 34px;
  padding: 0 8px;
  border: 1px solid rgb(227 232 236 / 22%);
  border-radius: var(--radius);
  background: rgb(255 255 255 / 5%);
  color: var(--bar-ink);
  font: 500 13px var(--font-body);
}
select:hover {
  border-color: rgb(227 232 236 / 50%);
}
option {
  background: var(--bar);
  color: var(--bar-ink);
}
</style>
