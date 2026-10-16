<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref } from 'vue'
import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import '@xterm/xterm/css/xterm.css'
import { api, ApiError, ok, wsUrl } from '../api/client'

const props = defineProps<{ vmId: string }>()
const host = ref<HTMLDivElement>()
const status = ref<'connecting' | 'connected' | 'closed'>('connecting')
const message = ref('')

const term = new Terminal({
  fontFamily: "'Martian Mono', monospace",
  fontSize: 12,
  cursorBlink: true,
  theme: { background: '#000', foreground: '#d7dde2', cursor: '#f2c230' },
})
const fit = new FitAddon()
term.loadAddon(fit)
let ws: WebSocket | null = null
const encoder = new TextEncoder()
term.onData((d) => ws?.readyState === WebSocket.OPEN && ws.send(encoder.encode(d)))
const resize = new ResizeObserver(() => fit.fit())

async function connect() {
  ws?.close()
  status.value = 'connecting'
  try {
    const t = await ok(api.POST('/api/v1/vms/{id}/console', { params: { path: { id: props.vmId } }, body: { kind: 'serial' } }))
    ws = new WebSocket(wsUrl(t.path))
    ws.binaryType = 'arraybuffer'
    ws.onopen = () => {
      status.value = 'connected'
      term.focus()
      // Nudge the guest so a login prompt appears.
      ws?.send(encoder.encode('\r'))
    }
    ws.onmessage = (ev) => term.write(new Uint8Array(ev.data as ArrayBuffer))
    ws.onclose = () => {
      status.value = 'closed'
      message.value = 'The serial console disconnected.'
    }
  } catch (e) {
    status.value = 'closed'
    message.value = e instanceof ApiError ? e.message : 'Could not open the serial console.'
  }
}

onMounted(() => {
  if (!host.value) return
  term.open(host.value)
  fit.fit()
  resize.observe(host.value)
  void connect()
})
onBeforeUnmount(() => {
  resize.disconnect()
  ws?.close()
  term.dispose()
})
</script>

<template>
  <div>
    <div class="toolbar">
      <span class="label">{{ status === 'connected' ? 'Connected' : status === 'connecting' ? 'Connecting…' : 'Disconnected' }}</span>
      <button v-if="status === 'closed'" class="btn primary" @click="connect">Reconnect</button>
    </div>
    <p v-if="status === 'closed' && message" class="muted">{{ message }}</p>
    <div ref="host" class="term" />
  </div>
</template>

<style scoped>
.toolbar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  min-height: 32px;
  margin-bottom: 10px;
}
.term {
  height: 60vh;
  padding: 10px;
  border-radius: var(--radius);
  background: #000;
}
.muted {
  color: var(--muted);
}
</style>
