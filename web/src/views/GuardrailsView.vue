<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { api, errorText, ok, type Policy } from '../api/client'
import NetTabs from '../components/NetTabs.vue'
import RuleEditor from '../components/RuleEditor.vue'
import { ruleText } from '../state'
import { useLiveRefresh } from '../stores/live'
import { useScope } from '../stores/scope'

const scope = useScope()
const policies = ref<Policy[] | null>(null)
const error = ref('')
const adding = ref(false)
const busy = ref(false)

async function load() {
  if (!scope.current) return
  const tenant = scope.current
  try {
    const p = await ok(api.GET('/api/v1/tenants/{id}/network-policies', { params: { path: { id: tenant } } }))
    if (tenant === scope.current) policies.value = p
  } catch (e) {
    error.value = errorText(e)
  }
}
watch(() => scope.current, load, { immediate: true })
useLiveRefresh(['network_policy'], load)

const canWrite = computed(() => scope.can('policy.write'))
const own = computed(() => policies.value?.filter((p) => !p.inherited) ?? [])
const inherited = computed(() => policies.value?.filter((p) => p.inherited) ?? [])

async function add(body: Record<string, unknown>) {
  error.value = ''
  busy.value = true
  try {
    await ok(
      api.POST('/api/v1/tenants/{id}/network-policies', {
        params: { path: { id: scope.current } },
        body: body as never,
      }),
    )
    adding.value = false
    await load()
  } catch (e) {
    error.value = errorText(e)
  } finally {
    busy.value = false
  }
}

async function remove(p: Policy) {
  error.value = ''
  try {
    await ok(api.DELETE('/api/v1/network-policies/{id}', { params: { path: { id: p.id } } }))
    await load()
  } catch (e) {
    error.value = errorText(e)
  }
}
</script>

<template>
  <section>
    <NetTabs />
    <header class="page-head">
      <h1>Guardrails</h1>
      <div v-if="canWrite" class="actions">
        <button class="btn primary" @click="adding = !adding">Add guardrail</button>
      </div>
    </header>
    <p class="intro">
      Traffic blocked for every VM in {{ scope.tenant?.name ?? 'this tenant' }} and all tenants below it, whatever their
      security groups allow. Only {{ scope.tenant?.name ?? 'this tenant' }}’s administrators can change these.
    </p>
    <p v-if="error" class="error-text" role="alert">{{ error }}</p>

    <RuleEditor v-if="adding" submit-label="Block this traffic" :busy="busy" @submit="add" @cancel="adding = false" />

    <template v-if="policies">
      <section class="block">
        <h2 class="label">Set here</h2>
        <ul class="rails">
          <li v-for="p in own" :key="p.id" class="rail">
            <span class="stop" aria-hidden="true">✕</span>
            <span class="text">{{ ruleText(p) }}</span>
            <span v-if="p.description" class="muted">{{ p.description }}</span>
            <button v-if="canWrite" class="btn small" @click="remove(p)">Remove</button>
          </li>
          <li v-if="!own.length" class="muted rail">None.</li>
        </ul>
      </section>
      <section v-if="inherited.length" class="block">
        <h2 class="label">From tenants above</h2>
        <ul class="rails">
          <li v-for="p in inherited" :key="p.id" class="rail locked">
            <span class="stop" aria-hidden="true">✕</span>
            <span class="text">{{ ruleText(p) }}</span>
            <span class="tag">{{ p.tenant_name }}</span>
            <span v-if="p.description" class="muted">{{ p.description }}</span>
          </li>
        </ul>
      </section>
    </template>
  </section>
</template>

<style scoped>
.block {
  margin-top: 20px;
}
.block h2 {
  margin-bottom: 8px;
}
.rails {
  margin: 0;
  padding: 0;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
  background: var(--panel);
  list-style: none;
}
.rail {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 4px 12px;
  padding: 10px 14px;
  border-top: 1px solid var(--rule);
}
.rail:first-child {
  border-top: 0;
}
.rail .btn {
  margin-left: auto;
}
.text {
  font-weight: 600;
}
.stop {
  display: grid;
  place-items: center;
  width: 20px;
  height: 20px;
  border-radius: 2px;
  background: var(--led-fault);
  color: #fff;
  font-size: 11px;
}
/* Set above: cannot be changed from here. */
.locked {
  background: repeating-linear-gradient(-45deg, transparent 0 8px, color-mix(in srgb, var(--rule) 45%, transparent) 8px 9px);
}
</style>
