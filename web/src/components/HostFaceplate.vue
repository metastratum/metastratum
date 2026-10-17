<script setup lang="ts">
import { computed } from 'vue'
import type { Host, Vm } from '../api/client'
import { mib, vmState } from '../state'
import StatusLed from './StatusLed.vue'

const props = defineProps<{ host: Host; vms: Vm[] }>()
const emit = defineEmits<{ manage: [] }>()
const retired = computed(() => props.host.state === 'decommissioned')

// Bays are the host's memory: each VM occupies a share proportional to what
// it was allocated, and the rest is blanking panel.
const bays = computed(() =>
  props.vms
    .slice()
    .sort((a, b) => b.memory_mib - a.memory_mib)
    .map((vm) => ({ vm, share: (vm.memory_mib / Math.max(props.host.memory_mib, 1)) * 100 })),
)
const usedPct = computed(() => Math.min(100, (props.host.allocated_memory_mib / Math.max(props.host.memory_mib, 1)) * 100))
</script>

<template>
  <article class="plate" :class="{ offline: !host.connected }">
    <div class="ears" aria-hidden="true"><span /><span /></div>
    <header>
      <span class="tape">{{ host.name }}</span>
      <span v-if="retired" class="tag retired" title="Taken out of service for good; kept for the record">
        Decommissioned
      </span>
      <StatusLed v-else :tone="host.connected ? 'on' : 'fault'" :label="host.connected ? 'Connected' : 'Not connected'" />
      <span v-if="!host.kvm" class="error-text">No KVM</span>
      <span v-if="host.state === 'maintenance'" class="tag maint">Maintenance</span>
      <span v-if="host.state === 'fenced'" class="tag fenced" title="It failed and was powered off; its VMs were moved">Fenced</span>
      <span v-if="host.connected && !host.migration_ready" class="tag stray" title="QEMU has no TLS certificates for migration">
        No migration
      </span>
      <span
        v-if="!retired && host.pending_facts"
        class="tag stray"
        title="It reports changes about itself that wait for you to accept them (Manage)"
      >
        Changes waiting
      </span>
      <span
        v-if="!retired && host.cannot_run.length"
        class="tag stray"
        :title="`Its CPU lacks features these VMs of its cluster use, so they do not move here: ${host.cannot_run.join(', ')}. Fit their CPU to the cluster (Details) to change that.`"
      >
        {{ host.cannot_run.length }} VM{{ host.cannot_run.length === 1 ? '' : 's' }} can’t run here
      </span>
      <span v-if="!retired" class="tag" :class="{ stray: !host.cluster_name }">{{ host.cluster_name ?? 'No cluster' }}</span>
      <button v-if="!retired" type="button" class="btn small manage" @click="emit('manage')">Manage</button>
      <dl class="specs mono">
        <div><dt>addr</dt><dd>{{ host.address }}</dd></div>
        <div><dt>cpu</dt><dd>{{ host.cpu_count }}</dd></div>
        <div><dt>mem</dt><dd>{{ mib(host.allocated_memory_mib) }} / {{ mib(host.memory_mib) }}</dd></div>
        <div><dt>vms</dt><dd>{{ host.vm_count }}</dd></div>
      </dl>
    </header>
    <div
      class="bays"
      role="meter"
      :aria-valuenow="Math.round(usedPct)"
      aria-valuemin="0"
      aria-valuemax="100"
      :aria-label="`${host.name} memory allocated`"
    >
      <RouterLink
        v-for="b in bays"
        :key="b.vm.id"
        :to="{ name: 'vm', params: { id: b.vm.id } }"
        class="bay"
        :style="{ flexBasis: `${b.share}%` }"
        :title="`${b.vm.name} · ${mib(b.vm.memory_mib)} · ${vmState(b.vm).text}`"
      >
        <StatusLed :tone="vmState(b.vm).tone" />
        <span class="bay-name">{{ b.vm.name }}</span>
      </RouterLink>
      <span class="blank" aria-hidden="true" />
    </div>
    <footer class="mono muted">{{ host.cpu_model || 'unknown cpu' }} · libvirt {{ host.libvirt_version }} · agent {{ host.agent_version }}</footer>
  </article>
</template>

<style scoped>
.tag.retired {
  color: var(--muted);
}
.plate {
  position: relative;
  display: grid;
  gap: 12px;
  padding: 14px 40px;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
  background: var(--panel);
  box-shadow: var(--shadow);
}
.plate.offline {
  opacity: 0.7;
}
/* Rack ears with mounting holes. */
.ears span {
  position: absolute;
  top: 0;
  bottom: 0;
  width: 22px;
  background:
    radial-gradient(circle at 50% 22%, var(--aisle) 0 3px, transparent 3.5px),
    radial-gradient(circle at 50% 78%, var(--aisle) 0 3px, transparent 3.5px),
    var(--sunken);
}
.ears span:first-child {
  left: 0;
  border-right: 1px solid var(--rule);
}
.ears span:last-child {
  right: 0;
  border-left: 1px solid var(--rule);
}
header {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 10px 18px;
}
.tape {
  padding: 3px 9px 2px;
  background: var(--tape);
  color: var(--tape-ink);
  font: 600 13px var(--font-label);
  letter-spacing: 0.1em;
  text-transform: uppercase;
  clip-path: polygon(4px 0, 100% 0, calc(100% - 4px) 100%, 0 100%);
}
.specs {
  display: flex;
  flex-wrap: wrap;
  gap: 4px 18px;
  margin: 0 0 0 auto;
}
.specs div {
  display: flex;
  gap: 6px;
}
.specs dt {
  color: var(--muted);
}
.specs dd {
  margin: 0;
}
.bays {
  display: flex;
  height: 44px;
  overflow: hidden;
  border: 1px solid var(--rule);
  border-radius: 2px;
  background: var(--sunken);
}
.bay {
  display: flex;
  align-items: center;
  gap: 8px;
  /* Keep at least the LED and a few characters visible for small VMs. */
  min-width: 6rem;
  padding: 0 10px;
  border-right: 1px solid var(--rule);
  background: var(--panel);
  text-decoration: none;
  flex-grow: 0;
  flex-shrink: 1;
}
.bay:hover {
  background: color-mix(in srgb, var(--tape) 18%, var(--panel));
}
.bay-name {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-weight: 500;
}
/* Unallocated capacity: blanking panel. */
.blank {
  flex: 1;
  background: repeating-linear-gradient(-45deg, transparent 0 6px, var(--rule) 6px 7px);
}
footer {
  font-size: 10.5px;
}
.muted {
  color: var(--muted);
}
@media (max-width: 640px) {
  .plate {
    padding: 14px 16px;
  }
  .ears {
    display: none;
  }
  .specs {
    margin-left: 0;
  }
}
.tag.fenced {
  border-color: var(--led-fault);
  color: var(--led-fault);
}
.tag.maint {
  border-color: var(--led-busy);
  color: var(--led-busy);
}
.tag.stray {
  border-style: dashed;
}
.manage {
  order: 10;
}
</style>
