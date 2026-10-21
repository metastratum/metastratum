<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue'
import { api, ok, type Task } from '../../api/client'
import { ago } from '../../state'
import { useLiveRefresh } from '../../stores/live'
import { useScope } from '../../stores/scope'
import { taskLabel } from '../../stores/tasks'
import StatusLed from '../StatusLed.vue'

/** What was done to a VM lately: its tasks, newest first. */
const props = defineProps<{ vmId: string; tenantId: string }>()
const scope = useScope()

const history = ref<Task[] | null>(null)
async function load() {
  history.value = await ok(api.GET('/api/v1/tasks', { params: { query: { target_id: props.vmId, limit: 10 } } }))
}
onMounted(load)
watch(() => props.vmId, load)
// Task notices don't say which VM they are about; this list is cheap.
useLiveRefresh(['task'], load)

const FEW = 5
const all = ref(false)
const shown = computed(() => (all.value ? history.value : history.value?.slice(0, FEW)) ?? [])
const tone = (t: Task) => (t.state === 'failed' ? 'fault' : t.state === 'succeeded' ? 'on' : 'busy')
</script>

<template>
  <section class="vm-card">
    <h2>Recent activity</h2>
    <template v-if="history?.length">
      <ol :class="{ all }">
        <li v-for="t in shown" :key="t.id">
          <StatusLed :tone="tone(t)" />
          <span>{{ taskLabel(t) }}</span>
          <span class="when muted" :title="new Date(t.created_at).toLocaleString()">{{ ago(t.created_at) }}</span>
          <span v-if="t.error" class="error-text err">{{ t.error }}</span>
        </li>
      </ol>
      <div class="foot">
        <button v-if="history.length > FEW && !all" type="button" class="linkish" @click="all = true">
          Show {{ history.length - FEW }} more
        </button>
        <RouterLink v-if="scope.can('audit.read', tenantId)" to="/audit" class="audit">
          Full history in the audit log →
        </RouterLink>
      </div>
    </template>
    <p v-else-if="history" class="muted none">Nothing yet.</p>
  </section>
</template>

<style scoped>
ol {
  display: grid;
  gap: 8px;
  margin: 0;
  padding: 0;
  list-style: none;
}
ol.all {
  max-height: 280px;
  overflow-y: auto;
}
li {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 4px 10px;
}
.when {
  margin-left: auto;
}
.err {
  flex-basis: 100%;
  padding-left: 18px;
  font-size: 12px;
}
.foot {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px 16px;
  padding-top: 10px;
  border-top: 1px solid var(--rule);
}
.foot:empty {
  display: none;
}
.linkish {
  padding: 0;
  border: 0;
  background: none;
  color: var(--ink);
  font: 500 13px var(--font-body);
  cursor: pointer;
}
.audit {
  margin-left: auto;
  font-size: 13px;
}
.none {
  margin: 0;
}
</style>
