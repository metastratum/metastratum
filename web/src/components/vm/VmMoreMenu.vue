<script setup lang="ts">
import { nextTick, onBeforeUnmount, ref, watch } from 'vue'

/** One entry: what it does, a line on why (or why not), and the action. */
export type MenuItem = {
  label: string
  hint?: string
  disabled?: boolean
  danger?: boolean
  /** A rule above it. */
  sep?: boolean
  run: () => void
}

defineProps<{ items: MenuItem[] }>()

const open = ref(false)
const root = ref<HTMLDivElement>()
const toggle = ref<HTMLButtonElement>()
const list = ref<HTMLDivElement>()

const entries = () => [...(list.value?.querySelectorAll<HTMLButtonElement>('[role=menuitem]:not(:disabled)') ?? [])]

function close(refocus = false) {
  open.value = false
  if (refocus) toggle.value?.focus()
}
function pick(item: MenuItem) {
  if (item.disabled) return
  close()
  item.run()
}
// Opened from the keyboard, the first entry takes focus.
async function show(focusFirst: boolean) {
  open.value = true
  await nextTick()
  if (focusFirst) entries()[0]?.focus()
}
function move(by: number) {
  const all = entries()
  if (!all.length) return
  const at = all.indexOf(document.activeElement as HTMLButtonElement)
  all[(at + by + all.length) % all.length]!.focus()
}
function onKey(e: KeyboardEvent) {
  if (e.key === 'Escape') {
    e.preventDefault()
    close(true)
  } else if (e.key === 'ArrowDown') {
    e.preventDefault()
    move(1)
  } else if (e.key === 'ArrowUp') {
    e.preventDefault()
    move(-1)
  } else if (e.key === 'Home' || e.key === 'End') {
    e.preventDefault()
    const all = entries()
    all[e.key === 'Home' ? 0 : all.length - 1]?.focus()
  } else if (e.key === 'Tab') {
    close()
  }
}
// A click anywhere else closes it.
function outside(e: PointerEvent) {
  if (!root.value?.contains(e.target as Node)) close()
}
watch(open, (on) =>
  on ? document.addEventListener('pointerdown', outside) : document.removeEventListener('pointerdown', outside),
)
onBeforeUnmount(() => document.removeEventListener('pointerdown', outside))
</script>

<template>
  <div ref="root" class="more">
    <button
      ref="toggle"
      type="button"
      class="btn"
      aria-haspopup="menu"
      :aria-expanded="open"
      @click="open ? close() : show(false)"
      @keydown.down.prevent="show(true)"
    >
      More
      <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true">
        <path d="M3 4.5 6 7.5 9 4.5" fill="none" stroke="currentColor" stroke-width="1.5" />
      </svg>
    </button>
    <div v-if="open" ref="list" class="menu" role="menu" @keydown="onKey">
      <template v-for="item in items" :key="item.label">
        <div v-if="item.sep" class="sep" role="separator" />
        <button
          type="button"
          role="menuitem"
          :disabled="item.disabled"
          :class="{ danger: item.danger }"
          @click="pick(item)"
        >
          <span class="what">{{ item.label }}</span>
          <span v-if="item.hint" class="hint">{{ item.hint }}</span>
        </button>
      </template>
    </div>
  </div>
</template>

<style scoped>
.more {
  position: relative;
}
.more > .btn {
  gap: 6px;
  padding-right: 10px;
}
.menu {
  position: absolute;
  top: calc(100% + 6px);
  right: 0;
  z-index: 5;
  display: flex;
  flex-direction: column;
  width: 280px;
  padding: 4px;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
  background: var(--panel);
  box-shadow: var(--shadow);
}
.menu button {
  display: grid;
  gap: 1px;
  padding: 7px 10px;
  border: 0;
  border-radius: 2px;
  background: none;
  color: var(--ink);
  font: 400 13px var(--font-body);
  text-align: left;
  cursor: pointer;
}
.menu button:hover:not(:disabled),
.menu button:focus-visible {
  background: var(--sunken);
  outline: none;
}
.menu button:disabled {
  opacity: 0.45;
  cursor: default;
}
.menu button.danger .what {
  color: var(--led-fault);
  font-weight: 500;
}
.hint {
  font-size: 12px;
  color: var(--muted);
}
.sep {
  height: 1px;
  margin: 4px 0;
  background: var(--rule);
}
</style>
