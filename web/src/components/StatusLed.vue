<script setup lang="ts">
import type { Tone } from '../state'

defineProps<{ tone: Tone; label?: string }>()
</script>

<template>
  <span class="led-wrap">
    <span class="led" :class="tone" aria-hidden="true" />
    <span v-if="label" class="led-label">{{ label }}</span>
  </span>
</template>

<style scoped>
.led-wrap {
  display: inline-flex;
  align-items: center;
  gap: 8px;
  white-space: nowrap;
}
.led {
  width: 8px;
  height: 8px;
  flex: none;
  border-radius: 50%;
  background: var(--led-off);
  box-shadow: inset 0 0 0 1px rgb(0 0 0 / 12%);
}
.led.on {
  background: var(--led-on);
  box-shadow: 0 0 0 3px color-mix(in srgb, var(--led-on) 22%, transparent);
}
.led.fault {
  background: var(--led-fault);
  box-shadow: 0 0 0 3px color-mix(in srgb, var(--led-fault) 22%, transparent);
}
/* Activity blinks, like a drive light. */
.led.busy {
  background: var(--led-busy);
  animation: blink 1.1s steps(2, jump-none) infinite;
}
@keyframes blink {
  50% {
    opacity: 0.25;
  }
}
</style>
