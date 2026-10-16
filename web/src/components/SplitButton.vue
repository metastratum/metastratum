<script setup lang="ts">
import { nextTick, onBeforeUnmount, ref } from 'vue'

/**
 * A button for the usual action, joined to a menu of alternatives (for
 * example: Unlink, or Delete).
 */
export type SplitItem = { key: string; label: string; danger?: boolean }

defineProps<{
  label: string
  items: SplitItem[]
  /** Names the menu for screen readers, e.g. "More ways to remove nfs". */
  menuLabel: string
  small?: boolean
  danger?: boolean
  disabled?: boolean
  title?: string
}>()
const emit = defineEmits<{ main: []; select: [key: string] }>()

const open = ref(false)
const root = ref<HTMLElement | null>(null)
const menuEl = ref<HTMLElement | null>(null)
// The menu is placed on the page itself (scrolling tables and dialogs would
// clip it), under the toggle, right-aligned with it.
const place = ref({ top: 0, right: 0 })
// A modal <dialog> sits in the browser's top layer, above the page: the
// menu goes into the dialog the button is in, if any.
const target = ref<string | HTMLElement>('body')
const itemEls = ref<HTMLButtonElement[]>([])
const toggleEl = ref<HTMLButtonElement | null>(null)

function onDocument(e: MouseEvent) {
  const t = e.target as Node
  if (!root.value?.contains(t) && !menuEl.value?.contains(t)) close()
}
function close() {
  open.value = false
  document.removeEventListener('mousedown', onDocument)
  window.removeEventListener('scroll', close, true)
  window.removeEventListener('resize', close)
}
async function show(focusFirst: boolean) {
  target.value = root.value?.closest('dialog') ?? 'body'
  const r = toggleEl.value?.getBoundingClientRect()
  if (r) place.value = { top: r.bottom + 4, right: window.innerWidth - r.right }
  open.value = true
  document.addEventListener('mousedown', onDocument)
  window.addEventListener('scroll', close, true)
  window.addEventListener('resize', close)
  if (focusFirst) {
    await nextTick()
    itemEls.value[0]?.focus()
  }
}
function toggle() {
  if (open.value) close()
  else void show(false)
}
function choose(key: string) {
  close()
  emit('select', key)
}
function escape(e: KeyboardEvent) {
  // With the menu open, Escape closes only the menu (not a dialog around it).
  if (!open.value) return
  e.preventDefault()
  e.stopPropagation()
  close()
  toggleEl.value?.focus()
}
function move(by: number) {
  const els = itemEls.value
  const at = els.indexOf(document.activeElement as HTMLButtonElement)
  els[(at + by + els.length) % els.length]?.focus()
}
onBeforeUnmount(close)
defineExpose({ close })
</script>

<template>
  <div ref="root" class="split" @keydown.esc="escape">
    <button
      type="button"
      class="btn main"
      :class="{ small, danger }"
      :disabled="disabled"
      :title="title"
      @click="emit('main')"
    >
      {{ label }}
    </button>
    <button
      ref="toggleEl"
      type="button"
      class="btn toggle"
      :class="{ small, danger }"
      :disabled="disabled"
      :title="title"
      aria-haspopup="menu"
      :aria-expanded="open"
      :aria-label="menuLabel"
      @click="toggle"
      @keydown.down.prevent="show(true)"
    >
      <span aria-hidden="true">▾</span>
    </button>
    <Teleport :to="target">
      <ul
        v-if="open"
        ref="menuEl"
        role="menu"
        class="menu"
        :aria-label="menuLabel"
        :style="{ top: `${place.top}px`, right: `${place.right}px` }"
        @keydown.down.prevent="move(1)"
        @keydown.up.prevent="move(-1)"
        @keydown.esc="escape"
      >
        <li v-for="item in items" :key="item.key" role="none">
          <button
            ref="itemEls"
            type="button"
            role="menuitem"
            :class="{ danger: item.danger }"
            @click="choose(item.key)"
          >
            {{ item.label }}
          </button>
        </li>
      </ul>
    </Teleport>
  </div>
</template>

<style scoped>
.split {
  position: relative;
  display: inline-flex;
}
.split .main {
  border-top-right-radius: 0;
  border-bottom-right-radius: 0;
}
.split .toggle {
  border-top-left-radius: 0;
  border-bottom-left-radius: 0;
  margin-left: -1px;
  padding-left: 6px;
  padding-right: 6px;
}
.menu {
  position: fixed;
  z-index: 1000;
  min-width: 120px;
  margin: 0;
  padding: 4px 0;
  list-style: none;
  background: var(--panel);
  border: 1px solid var(--rule);
  border-radius: var(--radius);
  box-shadow: var(--shadow);
}
.menu button {
  display: block;
  width: 100%;
  padding: 6px 14px;
  border: 0;
  background: none;
  color: inherit;
  font: inherit;
  text-align: left;
  white-space: nowrap;
  cursor: pointer;
}
.menu button:hover,
.menu button:focus-visible {
  background: var(--sunken);
  outline: none;
}
.menu button.danger {
  color: var(--led-fault);
}
</style>
