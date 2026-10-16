<script setup lang="ts">
import { ref, watch } from 'vue'
import { api, ApiError, ok, type Image } from '../api/client'
import IdChip from '../components/IdChip.vue'
import ModalDialog from '../components/ModalDialog.vue'
import { ago } from '../state'
import { useScope } from '../stores/scope'

const scope = useScope()
const images = ref<Image[] | null>(null)
const adding = ref(false)
const form = ref({ name: '', source_url: '', format: 'qcow2', shared: false })
const error = ref('')
const rowError = ref<Record<string, string>>({})

async function load() {
  if (!scope.current) return
  images.value = await ok(api.GET('/api/v1/images', { params: { query: { tenant_id: scope.current } } }))
}
watch(() => scope.current, load, { immediate: true })

async function add() {
  error.value = ''
  try {
    await ok(api.POST('/api/v1/images', { body: { ...form.value, tenant_id: scope.current } }))
    adding.value = false
    form.value = { name: '', source_url: '', format: 'qcow2', shared: false }
    await load()
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : 'The controller isn’t responding. Try again.'
  }
}

async function remove(img: Image) {
  rowError.value = {}
  try {
    await ok(api.DELETE('/api/v1/images/{id}', { params: { path: { id: img.id } } }))
    await load()
  } catch (e) {
    rowError.value[img.id] = e instanceof ApiError && e.status === 409 ? 'Still used by VM disks.' : 'Could not delete.'
  }
}

const own = (img: Image) => img.tenant_id === scope.current
</script>

<template>
  <section>
    <header class="head">
      <h1>Images</h1>
      <button v-if="scope.can('image.write')" class="btn primary" @click="adding = true">Add image</button>
    </header>
    <p class="intro">
      Hosts download an image the first time a VM needs it, then reuse their copy. VM disks are thin copies of
      the image, so an image can’t be deleted while disks still use it.
    </p>

    <div v-if="images && !images.length" class="empty">
      <p>Add an image so VMs have something to boot. Any cloud image in qcow2 or raw format works.</p>
      <button v-if="scope.can('image.write')" class="btn primary" @click="adding = true">Add image</button>
    </div>

    <ul v-else-if="images" class="list">
      <li v-for="img in images" :key="img.id">
        <div class="main">
          <strong>{{ img.name }}</strong>
          <span class="tag label">{{ img.format }}</span>
          <span v-if="img.shared" class="tag label">Shared with sub-tenants</span>
          <span v-if="!own(img)" class="tag label">From parent tenant</span>
        </div>
        <div class="url mono">{{ img.source_url }}</div>
        <div class="meta">
          <IdChip :value="img.id" short />
          <span class="muted">Added {{ ago(img.created_at) }}</span>
          <button v-if="own(img) && scope.can('image.write')" class="btn danger small" @click="remove(img)">Delete</button>
          <span v-if="rowError[img.id]" class="error-text">{{ rowError[img.id] }}</span>
        </div>
      </li>
    </ul>

    <ModalDialog v-if="adding" title="Add image" @close="adding = false">
      <form class="body" @submit.prevent="add">
        <label class="field">
          <span class="label">Name</span>
          <input v-model="form.name" required placeholder="debian-13" autofocus />
        </label>
        <label class="field">
          <span class="label">Download URL</span>
          <input v-model="form.source_url" required type="url" placeholder="https://cloud.debian.org/…/debian-13-genericcloud-amd64.qcow2" />
        </label>
        <label class="field">
          <span class="label">Format</span>
          <select v-model="form.format">
            <option value="qcow2">qcow2</option>
            <option value="raw">raw</option>
          </select>
        </label>
        <label class="check">
          <input v-model="form.shared" type="checkbox" />
          Let sub-tenants use this image
        </label>
        <p v-if="error" class="error-text" role="alert">{{ error }}</p>
        <footer>
          <button type="button" class="btn" @click="adding = false">Cancel</button>
          <button class="btn primary">Add image</button>
        </footer>
      </form>
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
  max-width: 68ch;
  margin: 0 0 20px;
  color: var(--muted);
}
.list {
  display: grid;
  gap: 8px;
  margin: 0;
  padding: 0;
  list-style: none;
}
.list li {
  display: grid;
  gap: 6px;
  padding: 14px 16px;
  border: 1px solid var(--rule);
  border-radius: var(--radius);
  background: var(--panel);
}
.main {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 10px;
}
.tag {
  padding: 1px 6px;
  border: 1px solid var(--rule);
  border-radius: 2px;
}
.url {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: var(--muted);
}
.meta {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 12px;
}
.small {
  height: 26px;
  margin-left: auto;
}
.muted {
  color: var(--muted);
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
  gap: 16px;
  padding: 18px;
}
.check {
  display: flex;
  align-items: center;
  gap: 8px;
}
footer {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
}
.error-text {
  margin: 0;
}
</style>
