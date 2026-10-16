<script setup lang="ts">
import { onMounted, ref } from 'vue'

defineProps<{ title: string }>()
const emit = defineEmits<{ close: [] }>()
const el = ref<HTMLDialogElement>()

// Native <dialog>: focus trapping, Escape and inertness come for free.
onMounted(() => el.value?.showModal())
</script>

<template>
  <dialog ref="el" class="modal" @close="emit('close')" @cancel.prevent="emit('close')">
    <header>
      <h2>{{ title }}</h2>
      <button type="button" class="close" aria-label="Close" @click="emit('close')">×</button>
    </header>
    <slot />
  </dialog>
</template>

<style scoped>
.modal {
  width: min(560px, calc(100vw - 32px));
  padding: 0;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
  background: var(--panel);
  color: var(--ink);
  box-shadow: var(--shadow);
}
.modal::backdrop {
  background: rgb(18 22 25 / 45%);
}
header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 14px 18px;
  border-bottom: 1px solid var(--rule);
}
h2 {
  font-size: 15px;
  text-transform: uppercase;
  letter-spacing: 0.08em;
}
.close {
  border: 0;
  background: none;
  color: var(--muted);
  font-size: 22px;
  line-height: 1;
  cursor: pointer;
}
</style>
