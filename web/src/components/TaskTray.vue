<script setup lang="ts">
import { computed, ref } from 'vue'
import { taskLabel, useTasks } from '../stores/tasks'
import { ago } from '../state'
import StatusLed from './StatusLed.vue'

const tasks = useTasks()
const open = ref(false)
const recent = computed(() => tasks.tasks.slice(0, 12))

function tone(state: string) {
  return state === 'failed' ? 'fault' : state === 'succeeded' ? 'on' : state === 'cancelled' ? 'off' : 'busy'
}
</script>

<template>
  <div class="tray">
    <button type="button" class="toggle" :aria-expanded="open" @click="open = !open">
      <StatusLed :tone="tasks.active.length ? 'busy' : 'off'" />
      <span>{{ tasks.active.length ? `${tasks.active.length} running` : 'Tasks' }}</span>
    </button>
    <div v-if="open" class="panel" role="region" aria-label="Recent tasks">
      <p v-if="!recent.length" class="empty">Nothing has run yet. Changes you make show up here.</p>
      <ol v-else>
        <li v-for="t in recent" :key="t.id">
          <StatusLed :tone="tone(t.state)" />
          <div class="what">
            <span>{{ taskLabel(t) }}</span>
            <span class="when">{{ t.state === 'running' && t.step ? t.step : ago(t.created_at) }}</span>
            <span v-if="t.error" class="error-text err">{{ t.error }}</span>
          </div>
          <progress v-if="t.state === 'running'" :value="t.progress ?? 0" max="1" />
        </li>
      </ol>
    </div>
  </div>
</template>

<style scoped>
.tray {
  position: relative;
}
.toggle {
  display: inline-flex;
  align-items: center;
  gap: 8px;
  height: 30px;
  padding: 0 10px;
  border: 1px solid rgb(255 255 255 / 18%);
  border-radius: var(--radius);
  background: none;
  color: inherit;
  font: 500 13px var(--font-body);
  cursor: pointer;
}
.panel {
  position: absolute;
  right: 0;
  top: calc(100% + 8px);
  z-index: 20;
  width: min(380px, calc(100vw - 24px));
  max-height: 70vh;
  overflow: auto;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
  background: var(--panel);
  color: var(--ink);
  box-shadow: var(--shadow);
}
ol {
  margin: 0;
  padding: 0;
  list-style: none;
}
li {
  display: grid;
  grid-template-columns: auto 1fr auto;
  align-items: center;
  gap: 10px;
  padding: 10px 14px;
  border-bottom: 1px solid var(--rule);
}
li:last-child {
  border-bottom: 0;
}
.what {
  display: grid;
}
.when {
  font-size: 12px;
  color: var(--muted);
}
.err {
  font-size: 12px;
}
progress {
  width: 64px;
  accent-color: var(--led-busy);
}
.empty {
  margin: 0;
  padding: 16px;
  color: var(--muted);
}
</style>
