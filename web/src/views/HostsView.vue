<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { api, ApiError, ok, type Host, type Vm } from '../api/client'
import HostFaceplate from '../components/HostFaceplate.vue'
import ModalDialog from '../components/ModalDialog.vue'

const hosts = ref<Host[] | null>(null)
const vms = ref<Vm[]>([])
const token = ref<{ token: string; expires_at: string } | null>(null)
const tokenError = ref('')
const adding = ref(false)

async function load() {
  ;[hosts.value, vms.value] = await Promise.all([
    ok(api.GET('/api/v1/hosts')),
    ok(api.GET('/api/v1/vms', { params: { query: { descendants: true } } })),
  ])
}

let timer = 0
onMounted(() => {
  void load()
  timer = window.setInterval(load, 5000)
})
onUnmounted(() => clearInterval(timer))

const byHost = computed(() => {
  const m = new Map<string, Vm[]>()
  for (const vm of vms.value) if (vm.host_id) m.set(vm.host_id, [...(m.get(vm.host_id) ?? []), vm])
  return m
})

async function addHost() {
  adding.value = true
  token.value = null
  tokenError.value = ''
  try {
    token.value = await ok(api.POST('/api/v1/hosts/join-tokens', { body: { ttl_minutes: 60 } }))
  } catch (e) {
    tokenError.value = e instanceof ApiError ? e.message : 'Could not create a join token.'
  }
}
</script>

<template>
  <section>
    <header class="head">
      <h1>Hosts</h1>
      <button class="btn primary" @click="addHost">Add host</button>
    </header>
    <p class="intro">Each bar is a host’s memory. VMs take up space in proportion to what they were given.</p>

    <div v-if="hosts && !hosts.length" class="empty">
      <p>No hosts are enrolled. Add a hypervisor so VMs have somewhere to run.</p>
      <button class="btn primary" @click="addHost">Add host</button>
    </div>
    <div v-else-if="hosts" class="rack">
      <HostFaceplate v-for="h in hosts" :key="h.id" :host="h" :vms="byHost.get(h.id) ?? []" />
    </div>

    <ModalDialog v-if="adding" title="Add host" @close="adding = false">
      <div class="body">
        <p v-if="tokenError" class="error-text">{{ tokenError }}</p>
        <template v-else-if="token">
          <p>On the new hypervisor, install <span class="mono">hm-agent</span> and start it once with this token:</p>
          <pre class="cmd mono">hm-agent --controller https://CONTROLLER:8443 \
  --join-token {{ token.token }}</pre>
          <p class="muted">
            The token works for one host and expires {{ new Date(token.expires_at).toLocaleTimeString() }}. The host
            appears here as soon as it connects.
          </p>
        </template>
        <p v-else class="muted">Creating a join token…</p>
        <footer>
          <button class="btn" @click="adding = false">Done</button>
        </footer>
      </div>
    </ModalDialog>
  </section>
</template>

<style scoped>
.head {
  display: flex;
  align-items: baseline;
  justify-content: space-between;
  margin-bottom: 8px;
}
h1 {
  font-size: 22px;
}
.intro {
  margin: 0 0 20px;
  color: var(--muted);
}
.rack {
  display: grid;
  gap: 10px;
}
.empty {
  display: grid;
  gap: 14px;
  justify-items: start;
  padding: 32px;
  border: 1px dashed var(--rule);
  border-radius: var(--radius);
}
.empty p {
  margin: 0;
  color: var(--muted);
}
.body {
  display: grid;
  gap: 12px;
  padding: 18px;
}
.body p {
  margin: 0;
}
.cmd {
  margin: 0;
  padding: 12px;
  overflow-x: auto;
  border-radius: var(--radius);
  background: var(--sunken);
  white-space: pre;
}
.muted {
  color: var(--muted);
}
footer {
  display: flex;
  justify-content: flex-end;
}
</style>
