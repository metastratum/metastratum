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
    const span = Math.max(used, own ?? 0, ceiling?.max ?? 0) * 1.08 || 1
    const pct = (v: number) => Math.min(100, (v / span) * 100)
    return {
      ...r,
      used,
      own,
      ceiling,
      full: Number.isFinite(effective) && used >= effective,
      near: Number.isFinite(effective) && used >= effective * 0.85,
      unlimited: !Number.isFinite(effective),
      usedPct: Number.isFinite(effective) ? pct(used) : used ? 6 : 0,
      ownPct: own !== undefined ? pct(own) : undefined,
      ceilingPct: ceiling ? pct(ceiling.max) : undefined,
      // Ticks at round fractions of the span, like a meter face.
      ticks: [0.25, 0.5, 0.75].map((f) => f * 100),
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
      <p class="muted lead">
        Usage counts {{ tenant.name }} and all its sub-tenants.
        <template v-if="!tenant.parent_id">The provider has no limits of its own.</template>
        <template v-else-if="!editable">
          Limits are set by the administrators of {{ parent?.name ?? 'the tenant above' }}.
        </template>
      </p>

      <ul v-if="!editing" class="gauges">
        <li v-for="g in gauges" :key="g.key" class="gauge" :class="{ near: g.near, full: g.full }">
          <div class="gauge-head">
            <span class="label">{{ g.label }}</span>
            <span class="reading mono">
              {{ fmt(g.used, g.scale) }}<template v-if="g.unlimited && g.unit"> {{ g.unit }}</template>
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
            role="meter"
            :aria-label="`${g.label} used`"
            :aria-valuenow="g.used"
            aria-valuemin="0"
            :aria-valuemax="g.unlimited ? undefined : Math.min(g.own ?? Infinity, g.ceiling?.max ?? Infinity)"
          >
            <span v-for="t in g.ticks" :key="t" class="tick" :style="{ left: `${t}%` }" aria-hidden="true" />
            <span class="fill" :class="{ open: g.unlimited }" :style="{ width: `${g.usedPct}%` }" />
            <span v-if="g.ownPct !== undefined" class="stop" :style="{ left: `${g.ownPct}%` }" title="This tenant’s limit" />
            <span
              v-if="g.ceilingPct !== undefined"
              class="notch"
              :style="{ left: `${g.ceilingPct}%` }"
              :title="`Limit set at ${g.ceiling!.tenant_name}`"
            />
          </div>
          <div v-if="g.ceiling" class="ceiling-note">
            <span class="notch-key" aria-hidden="true" />
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

      <button v-if="editable && !editing" class="btn small" @click="edit">Change limits</button>
    </template>
  </div>
</template>

<style scoped>
.panel {
  display: grid;
  gap: 18px;
  justify-items: start;
}
.lead {
  margin: 0;
}
.gauges {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(240px, 1fr));
  gap: 22px 28px;
  width: 100%;
  margin: 0;
  padding: 0;
  list-style: none;
}
.gauge {
  display: grid;
  gap: 8px;
}
.gauge-head {
  display: flex;
  align-items: baseline;
  justify-content: space-between;
  gap: 8px;
}
.reading {
  font-size: 13px;
}
/* A load meter: sunken track, graduated, with the limit as an end stop. */
.track {
  position: relative;
  height: 14px;
  border-radius: 2px;
  background: var(--sunken);
  box-shadow: inset 0 1px 2px rgb(0 0 0 / 12%);
}
.tick {
  position: absolute;
  bottom: 0;
  width: 1px;
  height: 5px;
  background: var(--muted);
  opacity: 0.4;
}
.fill {
  position: absolute;
  inset: 2px auto 2px 2px;
  max-width: calc(100% - 4px);
  border-radius: 1px;
  background: var(--led-on);
  transition: width 0.4s ease;
}
/* No limit anywhere: a stub, faded out, rather than a misleading fraction. */
.fill.open {
  background: linear-gradient(90deg, var(--led-on), transparent);
}
.near .fill {
  background: var(--led-busy);
}
.full .fill {
  background: var(--led-fault);
}
.stop {
  position: absolute;
  top: -3px;
  bottom: -3px;
  width: 3px;
  margin-left: -1.5px;
  border-radius: 1px;
  background: var(--ink);
}
/* An ancestor's tighter limit: a red notch cut into the top of the track. */
.notch,
.notch-key {
  width: 0;
  height: 0;
  border-right: 5px solid transparent;
  border-left: 5px solid transparent;
  border-top: 7px solid var(--led-fault);
}
.notch {
  position: absolute;
  top: -7px;
  margin-left: -5px;
}
.notch-key {
  display: inline-block;
  margin-right: 4px;
  vertical-align: 1px;
}
.ceiling-note {
  font-size: 12px;
  color: var(--muted);
}
.edit {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(150px, 1fr));
  gap: 14px;
  width: 100%;
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
