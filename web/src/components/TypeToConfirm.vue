<script setup lang="ts">
import { computed, ref, watch } from 'vue'

/** Asks for `name` to be typed before a destructive action can go ahead. */
const props = defineProps<{ name: string }>()
const confirmed = defineModel<boolean>({ default: false })
const typed = ref('')
const matches = computed(() => typed.value.trim() === props.name)
watch(matches, (m) => (confirmed.value = m), { immediate: true })
</script>

<template>
  <label class="field confirm">
    <span class="label">Type <strong class="mono">{{ name }}</strong> to confirm</span>
    <input v-model="typed" autocomplete="off" spellcheck="false" :aria-invalid="typed !== '' && !matches" />
  </label>
</template>

<style scoped>
.confirm input[aria-invalid='true'] {
  border-color: var(--led-fault);
}
</style>
