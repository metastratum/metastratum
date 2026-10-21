<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import RFB from '@novnc/novnc'
import { api, ApiError, ok, wsUrl } from '../api/client'
import StatusLed from './StatusLed.vue'

// `preview`: a small picture of the screen, taking no input and showing no
// controls (the page around it has those).
const props = defineProps<{ vmId: string; preview?: boolean }>()
const emit = defineEmits<{ status: [status: 'connecting' | 'connected' | 'closed'] }>()
const screen = ref<HTMLDivElement>()
const status = ref<'connecting' | 'connected' | 'closed'>('connecting')
const message = ref('')
// noVNC's view-only switch (not in its typings here).
let rfb: (RFB & { viewOnly: boolean }) | null = null

async function connect() {
  rfb?.disconnect()
  status.value = 'connecting'
  message.value = ''
  try {
    const t = await ok(api.POST('/api/v1/vms/{id}/console', { params: { path: { id: props.vmId } }, body: { kind: 'vnc' } }))
    if (!screen.value) return
    rfb = new RFB(screen.value, wsUrl(t.path), { wsProtocols: ['binary'] }) as RFB & { viewOnly: boolean }
    rfb.scaleViewport = true
    rfb.viewOnly = !!props.preview
    rfb.background = 'transparent'
    rfb.addEventListener('connect', () => {
      status.value = 'connected'
      emit('status', 'connected')
      // Focus for typing without jumping the page to the canvas.
      if (!props.preview) rfb?.focus({ preventScroll: true })
    })
    rfb.addEventListener('disconnect', () => {
      status.value = 'closed'
      emit('status', 'closed')
      message.value = 'The console disconnected.'
      rfb = null
    })
  } catch (e) {
    status.value = 'closed'
    emit('status', 'closed')
    message.value = e instanceof ApiError ? e.message : 'Could not open the console.'
  }
}

/** Ctrl+Alt+Del, a preview's too. */
function ctrlAltDel() {
  if (!rfb) return
  rfb.viewOnly = false
  rfb.sendCtrlAltDel()
  rfb.viewOnly = !!props.preview
}
defineExpose({ ctrlAltDel })

const tone = computed(() => (status.value === 'connected' ? 'on' : status.value === 'connecting' ? 'busy' : 'fault'))
const statusText = computed(() =>
  status.value === 'connected' ? 'Connected' : status.value === 'connecting' ? 'Connecting…' : 'Disconnected',
)

onMounted(connect)
onBeforeUnmount(() => rfb?.disconnect())
</script>

<template>
  <div class="console" :class="{ preview }">
    <div v-if="!preview" class="toolbar">
      <StatusLed :tone="tone" :label="statusText" class="label" />
      <button type="button" class="btn" :disabled="status !== 'connected'" @click="ctrlAltDel">Send Ctrl+Alt+Del</button>
    </div>
    <div class="frame">
      <div ref="screen" class="screen" />
      <div v-if="status === 'closed'" class="overlay">
        <p>{{ message }}</p>
        <button type="button" class="btn primary" @click="connect">Reconnect</button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.toolbar {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  justify-content: space-between;
  gap: 10px;
  min-height: 32px;
  margin-bottom: 10px;
}
.preview .frame {
  border-radius: 2px;
}
.preview .overlay {
  font-size: 12px;
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
