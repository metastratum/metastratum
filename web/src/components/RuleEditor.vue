<script setup lang="ts">
import { computed, ref } from 'vue'
import type { SecurityGroup } from '../api/client'
import { ruleText } from '../state'

/**
 * A form for the traffic a rule matches. Security-group rules may name a
 * group as the other end; guardrails only address ranges.
 */
const props = defineProps<{ groups?: SecurityGroup[]; submitLabel: string; busy?: boolean }>()
const emit = defineEmits<{ submit: [body: Record<string, unknown>]; cancel: [] }>()

const direction = ref<'ingress' | 'egress'>('ingress')
const protocol = ref<'any' | 'tcp' | 'udp' | 'icmp'>('tcp')
const ports = ref('')
const remoteKind = ref<'cidr' | 'group'>('cidr')
const cidr = ref('0.0.0.0/0')
const group = ref('')
const description = ref('')

const PRESETS = [
  { label: 'SSH', protocol: 'tcp', ports: '22' },
  { label: 'HTTP', protocol: 'tcp', ports: '80' },
  { label: 'HTTPS', protocol: 'tcp', ports: '443' },
  { label: 'Ping', protocol: 'icmp', ports: '' },
] as const

const parsedPorts = computed<[number, number] | null | 'bad'>(() => {
  const v = ports.value.trim()
  if (!v) return null
  const m = /^(\d{1,5})(?:\s*[-–]\s*(\d{1,5}))?$/.exec(v)
  if (!m) return 'bad'
  const a = Number(m[1])
  const b = m[2] ? Number(m[2]) : a
  return a >= 1 && b <= 65535 && a <= b ? [a, b] : 'bad'
})
const hasPorts = computed(() => protocol.value === 'tcp' || protocol.value === 'udp')
const preview = computed(() =>
  ruleText({
    direction: direction.value,
    protocol: protocol.value,
    port_min: hasPorts.value && Array.isArray(parsedPorts.value) ? parsedPorts.value[0] : null,
    port_max: hasPorts.value && Array.isArray(parsedPorts.value) ? parsedPorts.value[1] : null,
    remote_cidr: remoteKind.value === 'cidr' ? cidr.value.trim() || '0.0.0.0/0' : null,
    remote_group_name: remoteKind.value === 'group' ? props.groups?.find((g) => g.id === group.value)?.name : null,
  }),
)
const valid = computed(
  () => !(hasPorts.value && parsedPorts.value === 'bad') && (remoteKind.value === 'cidr' || !!group.value),
)

function preset(p: (typeof PRESETS)[number]) {
  protocol.value = p.protocol
  ports.value = p.ports
}

function submit() {
  const p = hasPorts.value && Array.isArray(parsedPorts.value) ? parsedPorts.value : null
  emit('submit', {
    direction: direction.value,
    protocol: protocol.value,
    port_min: p?.[0] ?? null,
    port_max: p?.[1] ?? null,
    remote_cidr: remoteKind.value === 'cidr' ? cidr.value.trim() || '0.0.0.0/0' : null,
    remote_group_id: remoteKind.value === 'group' ? group.value : null,
    description: description.value.trim(),
  })
}
</script>

<template>
  <form class="rule" @submit.prevent="submit">
    <div class="presets" role="group" aria-label="Common rules">
      <button v-for="p in PRESETS" :key="p.label" type="button" class="btn small" @click="preset(p)">{{ p.label }}</button>
    </div>
    <div class="grid">
      <label class="field">
        <span class="label">Direction</span>
        <select v-model="direction">
          <option value="ingress">Incoming</option>
          <option value="egress">Outgoing</option>
        </select>
      </label>
      <label class="field">
        <span class="label">Protocol</span>
        <select v-model="protocol">
          <option value="tcp">TCP</option>
          <option value="udp">UDP</option>
          <option value="icmp">ICMP</option>
          <option value="any">All</option>
        </select>
      </label>
      <label class="field">
        <span class="label">Ports</span>
        <input v-model="ports" :disabled="!hasPorts" placeholder="All" class="mono" />
        <span class="hint" :class="{ 'error-text': hasPorts && parsedPorts === 'bad' }">e.g. 22 or 8000-8100</span>
      </label>
      <label class="field">
        <span class="label">{{ direction === 'ingress' ? 'From' : 'To' }}</span>
        <select v-if="groups" v-model="remoteKind">
          <option value="cidr">Addresses</option>
          <option value="group">Members of a group</option>
        </select>
        <input v-if="remoteKind === 'cidr'" v-model="cidr" class="mono" placeholder="0.0.0.0/0" />
        <select v-else v-model="group" required>
          <option value="" disabled>Choose a group</option>
          <option v-for="g in groups" :key="g.id" :value="g.id">{{ g.name }}</option>
        </select>
      </label>
    </div>
    <label class="field">
      <span class="label">Note</span>
      <input v-model="description" maxlength="256" placeholder="Optional" />
    </label>
    <footer>
      <span class="preview">{{ preview }}</span>
      <button type="button" class="btn" @click="emit('cancel')">Cancel</button>
      <button class="btn primary" :disabled="busy || !valid">{{ submitLabel }}</button>
    </footer>
  </form>
</template>

<style scoped>
.rule {
  display: grid;
  gap: 12px;
  padding: 14px;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
  background: var(--sunken);
}
.presets {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
}
.grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(150px, 1fr));
  gap: 12px;
  align-items: start;
}
.field select + input,
.field select + select {
  margin-top: 6px;
}
footer {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px;
}
.preview {
  margin-right: auto;
  font-weight: 600;
}
</style>
