<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { api, errorText, ok, type Schemas, type Tenant } from '../../api/client'
import { useScope } from '../../stores/scope'

const props = defineProps<{ tenant: Tenant; parent?: Tenant }>()
const scope = useScope()

type Quotas = Schemas['Quotas']
const quotas = ref<Quotas | null>(null)
const loadError = ref('')

/** Resources, and how a value is shown and entered. Memory is entered in GiB. */
const RESOURCES = [
  { key: 'vms', label: 'VMs', unit: '', scale: 1 },
  { key: 'vcpus', label: 'vCPUs', unit: '', scale: 1 },
  { key: 'memory_mib', label: 'Memory', unit: 'GiB', scale: 1024 },
  { key: 'disk_gib', label: 'Disk', unit: 'GiB', scale: 1 },
  { key: 'floating_ips', label: 'Floating IPs', unit: '', scale: 1 },
  { key: 'snapshot_gib', label: 'Snapshots', unit: 'GiB', scale: 1 },
  { key: 'backup_gib', label: 'Backups', unit: 'GiB', scale: 1 },
  { key: 'image_gib', label: 'Images', unit: 'GiB', scale: 1 },
  { key: 'vpcs', label: 'VPCs', unit: '', scale: 1 },
  { key: 'load_balancers', label: 'Load balancers', unit: '', scale: 1 },
] as const

async function load() {
  try {
    quotas.value = await ok(api.GET('/api/v1/tenants/{id}/quotas', { params: { path: { id: props.tenant.id } } }))
  } catch (e) {
    loadError.value = errorText(e)
  }
}
onMounted(load)

// Limits are set from above: by whoever administers the parent.
const editable = computed(() => !!props.parent && scope.can('quota.write', props.parent.id))

const fmt = (v: number, scale: number) => {
  const n = v / scale
  return Number.isInteger(n) ? n.toLocaleString() : n.toFixed(1)
}

const gauges = computed(() => {
  const q = quotas.value
  if (!q) return []
  return RESOURCES.map((r) => {
    const used = q.usage[r.key] ?? 0
    const own = q.limits[r.key]
    const inherited = q.inherited[r.key]
    // The ancestor's limit matters only where it is tighter than our own.
    const ceiling = inherited && (own === undefined || inherited.max < own) ? inherited : undefined
    const effective = Math.min(own ?? Infinity, ceiling?.max ?? Infinity)
    const bounded = Number.isFinite(effective)
    return {
      ...r,
      used,
      own,
      ceiling,
      bounded,
      full: bounded && used >= effective,
      near: bounded && used >= effective * 0.8,
      usedPct: bounded ? Math.min(100, (used / (effective || 1)) * 100) : 0,
    }
  })
})

// Editing.
const editing = ref(false)
// Number inputs give numbers, or '' when empty.
const draft = ref<Record<string, string | number>>({})
const saveError = ref('')
const busy = ref(false)
function edit() {
  draft.value = Object.fromEntries(
    RESOURCES.map((r) => [r.key, quotas.value?.limits[r.key] !== undefined ? fmt(quotas.value.limits[r.key]!, r.scale).replace(/,/g, '') : '']),
  )
  saveError.value = ''
  editing.value = true
}
async function save() {
  saveError.value = ''
  busy.value = true
  try {
    const body: Record<string, number | null> = {}
    for (const r of RESOURCES) {
      const v = String(draft.value[r.key] ?? '').trim()
      body[r.key] = v ? Math.round(Number(v) * r.scale) : null
    }
    quotas.value = await ok(
      api.PUT('/api/v1/tenants/{id}/quotas', { params: { path: { id: props.tenant.id } }, body }),
    )
    editing.value = false
  } catch (e) {
    saveError.value = errorText(e)
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <div class="panel">
    <p v-if="loadError" class="error-text">{{ loadError }}</p>
    <template v-else-if="quotas">
      <div class="lead-row">
        <p class="muted lead">
          Usage counts {{ tenant.name }} and all its sub-tenants.
          <template v-if="!tenant.parent_id">The provider has no limits of its own.</template>
          <template v-else-if="!editable">
            Limits are set by the administrators of {{ parent?.name ?? 'the tenant above' }}.
          </template>
        </p>
        <button v-if="editable && !editing" class="btn small change" @click="edit">Change limits</button>
      </div>

      <ul v-if="!editing" class="gauges">
        <li v-for="g in gauges" :key="g.key" class="gauge" :class="{ near: g.near, full: g.full }">
          <div class="gauge-head">
            <span class="label">{{ g.label }}</span>
            <span class="reading mono">
              {{ fmt(g.used, g.scale) }}{{ !g.bounded && g.unit ? ` ${g.unit}` : '' }}
              <span class="muted">
                /
                <template v-if="g.own !== undefined">{{ fmt(g.own, g.scale) }} {{ g.unit }}</template>
                <template v-else-if="g.ceiling">{{ fmt(g.ceiling.max, g.scale) }} {{ g.unit }}</template>
                <template v-else>no limit</template>
              </span>
            </span>
          </div>
          <div
            class="track"
            :class="{ open: !g.bounded }"
            role="meter"
            :aria-label="`${g.label} used`"
            :aria-valuenow="g.used"
            aria-valuemin="0"
            :aria-valuemax="g.bounded ? Math.min(g.own ?? Infinity, g.ceiling?.max ?? Infinity) : undefined"
          >
            <span class="fill" :style="{ width: `${g.usedPct}%` }" />
          </div>
          <div v-if="g.ceiling" class="ceiling-note">
            {{ g.ceiling.tenant_name }} allows {{ fmt(g.ceiling.max, g.scale) }} {{ g.unit }} in total
          </div>
        </li>
      </ul>

      <form v-else class="edit" @submit.prevent="save">
        <label v-for="r in RESOURCES" :key="r.key" class="field">
          <span class="label">{{ r.label }}{{ r.unit ? ` (${r.unit})` : '' }}</span>
          <input v-model="draft[r.key]" type="number" min="0" :step="r.scale > 1 ? 0.5 : 1" placeholder="No limit" />
          <span v-if="quotas.inherited[r.key]" class="hint">
            {{ quotas.inherited[r.key]!.tenant_name }}: {{ fmt(quotas.inherited[r.key]!.max, r.scale) }}
          </span>
        </label>
        <p v-if="saveError" class="error-text wide" role="alert">{{ saveError }}</p>
        <footer class="wide">
          <button type="button" class="btn" @click="editing = false">Cancel</button>
          <button class="btn primary" :disabled="busy">Save limits</button>
        </footer>
      </form>

    </template>
  </div>
</template>

<style scoped>
.panel {
  display: grid;
  gap: 18px;
}
.lead-row {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px 16px;
}
.lead {
  margin: 0;
}
.change {
  margin-left: auto;
}
.gauges {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(min(240px, 100%), 1fr));
  gap: 18px 28px;
  margin: 0;
  padding: 0;
  list-style: none;
}
.gauge {
  display: grid;
  gap: 6px;
}
.gauge-head {
  display: flex;
  align-items: baseline;
  justify-content: space-between;
  gap: 8px;
}
.track {
  position: relative;
  height: 8px;
  overflow: hidden;
  border-radius: 2px;
  background: var(--sunken);
}
/* No limit anywhere: hatched, with nothing to fill. */
.track.open {
  background: repeating-linear-gradient(135deg, var(--sunken) 0 4px, var(--aisle) 4px 8px);
}
.fill {
  display: block;
  height: 100%;
  background: var(--ink);
  transition: width 0.4s ease;
}
.near .fill {
  background: var(--led-busy);
}
.full .fill {
  background: var(--led-fault);
}
@media (prefers-reduced-motion: reduce) {
  .fill {
    transition: none;
  }
}
.ceiling-note {
  font-size: 12px;
  color: var(--muted);
}
.edit {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(min(180px, 100%), 1fr));
  gap: 14px 16px;
}
.edit .field {
  align-content: start;
}
.edit .wide {
  grid-column: 1 / -1;
  margin: 0;
}
.edit footer {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
}
</style>
