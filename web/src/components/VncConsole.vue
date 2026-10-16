<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref } from 'vue'
import RFB from '@novnc/novnc'
import { api, ApiError, ok, wsUrl } from '../api/client'

const props = defineProps<{ vmId: string }>()
const screen = ref<HTMLDivElement>()
const status = ref<'connecting' | 'connected' | 'closed'>('connecting')
const message = ref('')
let rfb: RFB | null = null

async function connect() {
  rfb?.disconnect()
  status.value = 'connecting'
  message.value = ''
  try {
    const t = await ok(api.POST('/api/v1/vms/{id}/console', { params: { path: { id: props.vmId } }, body: { kind: 'vnc' } }))
    if (!screen.value) return
    rfb = new RFB(screen.value, wsUrl(t.path), { wsProtocols: ['binary'] })
    rfb.scaleViewport = true
    rfb.background = 'transparent'
    rfb.addEventListener('connect', () => {
      status.value = 'connected'
      // Focus for typing without jumping the page to the canvas.
      rfb?.focus({ preventScroll: true })
    })
    rfb.addEventListener('disconnect', () => {
      status.value = 'closed'
      message.value = 'The console disconnected.'
      rfb = null
    })
  } catch (e) {
    status.value = 'closed'
    message.value = e instanceof ApiError ? e.message : 'Could not open the console.'
  }
}

onMounted(connect)
onBeforeUnmount(() => rfb?.disconnect())
</script>

<template>
  <div class="console">
    <div class="toolbar">
      <span class="label">{{ status === 'connected' ? 'Connected' : status === 'connecting' ? 'Connecting…' : 'Disconnected' }}</span>
      <button class="btn" :disabled="status !== 'connected'" @click="rfb?.sendCtrlAltDel()">Send Ctrl+Alt+Del</button>
    </div>
    <div class="frame">
      <div ref="screen" class="screen" />
      <div v-if="status === 'closed'" class="overlay">
        <p>{{ message }}</p>
        <button class="btn primary" @click="connect">Reconnect</button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.toolbar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-bottom: 10px;
}
.frame {
  position: relative;
  aspect-ratio: 4 / 3;
  max-height: 72vh;
  border-radius: var(--radius);
  background: #000;
  overflow: hidden;
}
.screen {
  width: 100%;
  height: 100%;
}
.overlay {
  position: absolute;
  inset: 0;
  display: grid;
  place-content: center;
  justify-items: center;
  gap: 12px;
  background: rgb(7 9 10 / 82%);
  color: #e3e8ec;
}
.overlay p {
  margin: 0;
}
</style>
