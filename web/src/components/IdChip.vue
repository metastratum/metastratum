<script setup lang="ts">
import { ref } from 'vue'

const props = defineProps<{ value: string; short?: boolean }>()
const copied = ref(false)

async function copy() {
  await navigator.clipboard?.writeText(props.value)
  copied.value = true
  setTimeout(() => (copied.value = false), 1200)
}
</script>

<template>
  <button type="button" class="chip mono" :title="copied ? 'Copied' : `Copy ${value}`" @click.stop="copy">
    {{ copied ? 'copied' : short ? value.slice(0, 8) : value }}
  </button>
</template>

<style scoped>
.chip {
  padding: 1px 6px;
  border: 1px solid var(--rule);
  border-radius: 2px;
  background: var(--sunken);
  color: var(--ink);
  cursor: copy;
}
.chip:hover {
  border-color: var(--muted);
}
</style>
